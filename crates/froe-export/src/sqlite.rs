//! The `SQLite` sink: a single-file relational export with interned strings.
//!
//! One export produces one `.db` file holding two tables, one shared
//! string dictionary, and a view layer that presents both as flat,
//! directly queryable rows:
//!
//! * **strings** — `(id, value)`, every string in the export stored once:
//!   node names, primary types, property names, textual values, and
//!   external blob references all share this dictionary. Uniqueness is
//!   guaranteed by the sink (an in-memory map), so the table carries no
//!   `UNIQUE` index — that index would duplicate every string's bytes.
//! * **nodes** — `(id, parent_id, name_id, depth, primary_type_id)`,
//!   `id` assigned in document order. The full path is never stored:
//!   `parent_id` plus `name_id` reconstruct it, and the recursive view
//!   **`node_paths`** does that reconstruction for you.
//! * **properties** — one row per property *value*, multi-valued
//!   properties exploded with a `position`, mirroring the Parquet
//!   properties table but with interned strings: `(node_id, name_id,
//!   type_id, multiple, position, value_id, long_value, double_value,
//!   boolean_value, binary_length, binary_reference_id)`. The table is
//!   `WITHOUT ROWID` with primary key `(node_id, name_id, position)`, so
//!   "all properties of node X" is a clustered range scan and no
//!   synthetic rowid is stored. `position` is 0-based per value; an
//!   *empty* multi-valued property is one marker row with position −1
//!   (`NULL` cannot serve — primary-key columns are `NOT NULL` in a
//!   `WITHOUT ROWID` table). `type_id` is the JCR numeric tag, resolved
//!   by the 12-row **`property_types`** lookup.
//! * **export** — one metadata row: the exported root path, which
//!   anchors the path reconstruction in `node_paths`.
//!
//! Binary *content* is never embedded: inline binaries appear as
//! `binary_length`, external ones as `binary_reference_id` into the
//! dictionary.
//!
//! Why intern everything: a path or property name repeated a million
//! times then costs a 2–3 byte integer per row instead of its full text
//! per row, which is the manual equivalent of Parquet's dictionary
//! encoding — on a 139k-node AEM-shaped store this schema measures
//! ~8.7× smaller than the `TarMK` repository (a schema storing strings
//! inline per row manages under 2×). Strings used only once cost a few
//! bytes more than inline storage, but that overhead measures within
//! 2–3% of the theoretical optimum, so the uniform rule — every string
//! goes through the dictionary — wins on simplicity and size alike.
//!
//! Querying: the views cost no storage, so consumers write plain SQL
//! against `node_paths` and `properties_expanded` as if the data were
//! denormalized. The tables carry no secondary indexes by default —
//! they are analytical artifacts scanned wholesale, and indexes roughly
//! double the file; [`SqliteExportOptions::create_indexes`] adds
//! `nodes(parent_id)` and `properties(name_id, type_id)` indexes for
//! consumers issuing repeated point lookups in `SQLite` itself.
//! `finish` ends with `ANALYZE`, so the query planner has statistics.
//!
//! Two value-shape caveats versus the Parquet format: `SQLite` stores
//! NaN doubles as `NULL` (infinities survive), and `binary_length` is
//! saturated at `i64::MAX`.
//!
//! The file is written in a single transaction with the rollback
//! journal disabled — it is a build artifact, not a live database, and
//! on failure the partially written file is deleted.
//!
//! [`SqliteSink::create`] upholds the export's safety guarantees end to
//! end. The output file is created exclusively (never overwriting, never
//! inside the repository) and held open for the whole export. On Unix,
//! `SQLite` opens the guard file's own descriptor (`/proc/self/fd` on
//! Linux, `/dev/fd` elsewhere), so no pathname race — rename, swap, or
//! symlink — can redirect a single byte into a replacement file; on
//! Windows the guard is created without delete-share, which pins the
//! pathname to the created file while the export holds it. `finish`
//! verifies the path still names the created file, and dropping an
//! unfinished sink removes that file — again only while it provably
//! names it — so a failed or abandoned export leaves neither a partial
//! database nor anyone else's file behind.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use froe::content::value::BinaryValue;
use froe::content::{PropertyType, PropertyValue, PropertyValues};
use rusqlite::{Connection, OpenFlags, params};

use crate::export::{ExportSink, ExportedNode};

/// Where `SQLite` should open the database. On Unix, the guard file's
/// own descriptor: reopening `/proc/self/fd/N` (Linux) or `/dev/fd/N`
/// yields a new descriptor for the same inode, so what `SQLite` writes
/// is anchored to the file the export created, no matter how the
/// pathname is renamed or swapped meanwhile. Elsewhere the pathname
/// itself, which the guard's share mode keeps pinned (Windows) — see
/// [`crate::create_export_output`].
#[cfg(target_os = "linux")]
fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    let _ = path;
    PathBuf::from(format!("/proc/self/fd/{}", guard.as_raw_fd()))
}

/// The `/dev/fd` variant of [`open_target`] for non-Linux Unixes.
#[cfg(all(unix, not(target_os = "linux")))]
fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    let _ = path;
    PathBuf::from(format!("/dev/fd/{}", guard.as_raw_fd()))
}

/// The pathname variant of [`open_target`]: the guard created by
/// [`crate::create_export_output`] denies delete-share on Windows, so
/// the pathname cannot be repointed while the sink holds the file.
#[cfg(not(unix))]
fn open_target(path: &Path, guard: &std::fs::File) -> PathBuf {
    let _ = guard;
    path.to_path_buf()
}

/// Whether `path` still names `file`: same device and inode. A vanished
/// or unreadable path counts as "not ours".
#[cfg(unix)]
fn path_still_names(path: &Path, file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(created), Ok(named)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    created.dev() == named.dev() && created.ino() == named.ino()
}

/// Whether `path` still names `file`. On Windows the guard's missing
/// delete-share makes this trivially true for as long as the sink holds
/// the file; the metadata comparison is a sanity read. Other non-Unix
/// platforms get a best-effort size comparison — the export's identity
/// guarantee is Unix- and Windows-grade.
#[cfg(not(unix))]
fn path_still_names(path: &Path, file: &std::fs::File) -> bool {
    let (Ok(created), Ok(named)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    named.is_file() && named.len() == created.len()
}

/// The `position` marking an empty multi-valued property's marker row.
const EMPTY_MULTI_POSITION: i64 = -1;

/// Tables and views, created inside the export's single transaction.
const SCHEMA: &str = "
    CREATE TABLE strings(
        id INTEGER PRIMARY KEY,
        value TEXT NOT NULL UNIQUE
    );
    CREATE TABLE nodes(
        id INTEGER PRIMARY KEY,
        parent_id INTEGER REFERENCES nodes(id),
        name_id INTEGER NOT NULL REFERENCES strings(id),
        depth INTEGER NOT NULL,
        primary_type_id INTEGER REFERENCES strings(id)
    );
    CREATE TABLE property_types(
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL
    );
    CREATE TABLE properties(
        node_id INTEGER NOT NULL REFERENCES nodes(id),
        name_id INTEGER NOT NULL REFERENCES strings(id),
        type_id INTEGER NOT NULL REFERENCES property_types(id),
        multiple INTEGER NOT NULL CHECK (multiple IN (0, 1)),
        position INTEGER NOT NULL,
        value_id INTEGER REFERENCES strings(id),
        long_value INTEGER,
        double_value REAL,
        boolean_value INTEGER CHECK (boolean_value IN (0, 1)),
        binary_length INTEGER,
        binary_reference_id INTEGER REFERENCES strings(id),
        PRIMARY KEY (node_id, name_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE export(
        root_path TEXT NOT NULL
    );
    CREATE VIEW node_paths(id, path, depth, primary_type) AS
    WITH RECURSIVE walk(id, path, depth, primary_type) AS (
        SELECT n.id, (SELECT root_path FROM export), 0, p.value
          FROM nodes n
          LEFT JOIN strings p ON p.id = n.primary_type_id
         WHERE n.parent_id IS NULL
        UNION ALL
        SELECT n.id,
               walk.path || CASE WHEN walk.path = '/' THEN '' ELSE '/' END || s.value,
               n.depth, p.value
          FROM nodes n
          JOIN walk ON n.parent_id = walk.id
          JOIN strings s ON s.id = n.name_id
          LEFT JOIN strings p ON p.id = n.primary_type_id
    )
    SELECT id, path, depth, primary_type FROM walk;
    CREATE VIEW properties_expanded AS
    SELECT np.path, sn.value AS name, pt.name AS property_type,
           p.multiple, p.position, sv.value AS value,
           p.long_value, p.double_value, p.boolean_value,
           p.binary_length, sr.value AS binary_reference
      FROM properties p
      JOIN node_paths np ON np.id = p.node_id
      JOIN strings sn ON sn.id = p.name_id
      JOIN property_types pt ON pt.id = p.type_id
      LEFT JOIN strings sv ON sv.id = p.value_id
      LEFT JOIN strings sr ON sr.id = p.binary_reference_id;
";

/// Tuning knobs for a `SQLite` export.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteExportOptions {
    /// Adds `nodes(parent_id)` and `properties(name_id, type_id)`
    /// indexes after the export. Indexes roughly double the file size;
    /// keep them off for an artifact consumed analytically, enable them
    /// for repeated point lookups in `SQLite` itself.
    pub create_indexes: bool,
}

/// Byte budget for the in-memory half of the string dictionary.
const STRING_DICTIONARY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// A byte-budgeted string-to-id cache in front of the `strings` table.
///
/// Deliberately small and local: the table is the authority, so this only
/// has to make the repeated vocabulary — primary types, property names —
/// cheap. Eviction in insertion order costs a round-trip to `SQLite`, never a
/// wrong id, because a miss re-reads the row the first insert created.
struct BoundedStringCache {
    entries: HashMap<String, i64>,
    insertion_order: VecDeque<String>,
    used_bytes: usize,
    budget_bytes: usize,
}

impl BoundedStringCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            used_bytes: 0,
            budget_bytes,
        }
    }

    fn get(&self, text: &str) -> Option<i64> {
        self.entries.get(text).copied()
    }

    fn insert(&mut self, text: String, id: i64) {
        let weight = text.len() + 64;
        if self.entries.insert(text.clone(), id).is_none() {
            self.insertion_order.push_back(text);
            self.used_bytes = self.used_bytes.saturating_add(weight);
        }
        while self.used_bytes > self.budget_bytes {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            let oldest_weight = oldest.len() + 64;
            self.entries.remove(&oldest);
            self.used_bytes = self.used_bytes.saturating_sub(oldest_weight);
        }
    }
}

/// An [`ExportSink`] writing the interned `SQLite` schema described in the
/// [module documentation](self).
pub struct SqliteSink {
    connection: Option<Connection>,
    /// Recently interned strings, so the common repeats — primary types,
    /// property names — do not round-trip to `SQLite` on every row.
    ///
    /// The authoritative dictionary is the `strings` table itself, which is
    /// why this may evict. Holding every distinct string of the export in
    /// memory was the one thing keeping this sink from streaming: names and
    /// primary types repeat, but textual property values do not, so the map
    /// grew with the content rather than with the vocabulary.
    strings: BoundedStringCache,
    /// The depth-indexed stack of ancestor node ids: after processing a
    /// node, `ancestry[depth]` is its id, so its children's `parent_id`
    /// is one lookup away.
    ancestry: Vec<i64>,
    node_count: i64,
    create_indexes: bool,
    /// The exclusively created output file, held for the whole export:
    /// on Unix it is what `SQLite` writes to (through the descriptor),
    /// and everywhere it is the identity cleanup compares against.
    guard: Option<std::fs::File>,
    output_path: PathBuf,
    /// Set by `finish` — an unfinished export is a partial file whose
    /// cleanup the drop implementation owns.
    completed: bool,
}

impl SqliteSink {
    /// Creates a sink writing a new database at `path`.
    ///
    /// The file is created exclusively through
    /// [`crate::create_export_output`] — never overwriting an existing
    /// file, never inside `repository_path`, with owner-only
    /// permissions — and held open for the whole export. What `SQLite`
    /// then writes is anchored to that created file rather than to the
    /// mutable pathname (see the [module documentation](self)): on Unix
    /// the connection opens the guard's own descriptor, on Windows the
    /// guard's missing delete-share pins the pathname. On failure the
    /// fresh file is removed when, and only when, the path still names
    /// it.
    ///
    /// # Panics
    ///
    /// Panics only if the property-type table were incomplete; tags 1
    /// through 12 are defined by construction.
    pub fn create(
        repository_path: &Path,
        path: &Path,
        options: SqliteExportOptions,
    ) -> froe::Result<Self> {
        let guard = crate::create_export_output(repository_path, path)?;
        match Self::open_anchored(path, guard, options) {
            Ok(sink) => Ok(sink),
            Err((guard, error)) => {
                // Remove the fresh file only when the path still names
                // it — otherwise it may name someone else's file.
                let still_ours = path_still_names(path, &guard);
                drop(guard);
                if still_ours {
                    let _ = std::fs::remove_file(path);
                }
                Err(error)
            }
        }
    }

    /// Opens the database for [`SqliteSink::create`], anchored to the
    /// `guard` file, and initializes the schema. On failure the guard
    /// comes back with the error, so the caller can tell whether the
    /// output path still names it.
    fn open_anchored(
        path: &Path,
        guard: std::fs::File,
        options: SqliteExportOptions,
    ) -> Result<Self, (std::fs::File, froe::Error)> {
        match Self::initialize(path, &guard) {
            Ok(connection) => Ok(Self {
                connection: Some(connection),
                strings: BoundedStringCache::new(STRING_DICTIONARY_BUDGET_BYTES),
                ancestry: Vec::new(),
                node_count: 0,
                create_indexes: options.create_indexes,
                guard: Some(guard),
                output_path: path.to_path_buf(),
                completed: false,
            }),
            Err(error) => Err((guard, error)),
        }
    }

    /// The fallible opening sequence: returns the connection with the
    /// schema initialized.
    fn initialize(path: &Path, guard: &std::fs::File) -> froe::Result<Connection> {
        // Never create-on-missing: the file must be the one the guard
        // just created. On the pathname variants, never follow a final
        // symlink either. (The descriptor variants must not set
        // NOFOLLOW: /proc/self/fd and /dev/fd entries are symlinks.)
        #[cfg(unix)]
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        #[cfg(not(unix))]
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection =
            Connection::open_with_flags(open_target(path, guard), flags).map_err(sqlite_error)?;
        #[cfg(not(unix))]
        {
            // The pathname is pinned by the guard's share mode; force the
            // lazy open and sanity-check the identity anyway.
            connection
                .execute_batch("SELECT count(*) FROM sqlite_master;")
                .map_err(sqlite_error)?;
            if !path_still_names(path, guard) {
                return Err(froe::Error::InvalidFormat {
                    details: format!(
                        "output file {} was replaced while opening; the export never touches \
                         an existing file",
                        path.display()
                    ),
                });
            }
        }
        // A build artifact, not a live database: no rollback journal, no
        // fsync contract — a crash leaves a partial file to delete, and
        // everything runs in one transaction for speed and consistency.
        // The journal must stay off on Unix for a second reason: with
        // the descriptor-based open, the derived journal path
        // ("/proc/self/fd/N-journal") cannot be created at all.
        connection
            .execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; BEGIN;")
            .map_err(sqlite_error)?;
        connection.execute_batch(SCHEMA).map_err(sqlite_error)?;
        for tag in 1..=12u8 {
            let property_type = PropertyType::from_tag(tag).expect("tags 1 through 12 are defined");
            connection
                .prepare_cached("INSERT INTO property_types (id, name) VALUES (?, ?)")
                .and_then(|mut statement| {
                    statement.execute(params![i64::from(tag), property_type.jcr_name()])
                })
                .map_err(sqlite_error)?;
        }
        Ok(connection)
    }

    /// The connection, present until the drop implementation takes it.
    fn connection(&self) -> &Connection {
        self.connection
            .as_ref()
            .expect("the connection is taken only by the drop implementation")
    }

    /// Returns the dictionary id of `text`, inserting it into the
    /// strings table on first sight. Strings are inserted before any
    /// row referencing them, so the foreign keys stay valid even under
    /// `PRAGMA foreign_key_check`.
    fn intern(&mut self, text: &str) -> froe::Result<i64> {
        if let Some(id) = self.strings.get(text) {
            return Ok(id);
        }
        // A miss asks the table, which is the authority. `INSERT OR IGNORE`
        // then `SELECT` is two statements rather than one, but both are
        // cached and served from SQLite's own bounded page cache, so the
        // dictionary can outgrow memory without the process doing so.
        let connection = self.connection();
        connection
            .prepare_cached("INSERT OR IGNORE INTO strings (value) VALUES (?)")
            .and_then(|mut statement| statement.execute(params![text]))
            .map_err(sqlite_error)?;
        let id: i64 = connection
            .prepare_cached("SELECT id FROM strings WHERE value = ?")
            .and_then(|mut statement| statement.query_row(params![text], |row| row.get(0)))
            .map_err(sqlite_error)?;
        self.strings.insert(text.to_owned(), id);
        Ok(id)
    }

    /// Appends one row to the properties table. `position` is the
    /// value's index within a multi-valued property (0 for
    /// single-valued), [`EMPTY_MULTI_POSITION`] for the marker row of an
    /// empty one; `value` is `None` on the marker row.
    fn write_property_row(
        &mut self,
        node_id: i64,
        name: &str,
        type_id: i64,
        multiple: bool,
        position: i64,
        value: Option<&PropertyValue>,
    ) -> froe::Result<()> {
        let name_id = self.intern(name)?;
        let mut value_id = None;
        let mut long_value = None;
        let mut double_value = None;
        let mut boolean_value = None;
        let mut binary_length = None;
        let mut binary_reference_id = None;
        match value {
            None => {}
            Some(
                PropertyValue::String(content)
                | PropertyValue::Date(content)
                | PropertyValue::Name(content)
                | PropertyValue::Path(content)
                | PropertyValue::Reference(content)
                | PropertyValue::WeakReference(content)
                | PropertyValue::Uri(content)
                | PropertyValue::Decimal(content),
            ) => value_id = Some(self.intern(content)?),
            Some(PropertyValue::Long(number)) => long_value = Some(*number),
            // SQLite converts NaN to NULL; the infinities survive as
            // REAL. The Parquet format preserves NaN.
            Some(PropertyValue::Double(number)) => double_value = Some(*number),
            Some(PropertyValue::Boolean(truth)) => boolean_value = Some(i64::from(*truth)),
            Some(PropertyValue::Binary(BinaryValue::Inline { length, .. })) => {
                binary_length = Some((*length).min(i64::MAX as u64) as i64);
            }
            Some(PropertyValue::Binary(BinaryValue::External { blob_identifier })) => {
                binary_reference_id = Some(self.intern(blob_identifier)?);
            }
        }
        self.connection()
            .prepare_cached(
                "INSERT INTO properties (node_id, name_id, type_id, multiple, position, \
                 value_id, long_value, double_value, boolean_value, binary_length, \
                 binary_reference_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    node_id,
                    name_id,
                    type_id,
                    i64::from(multiple),
                    position,
                    value_id,
                    long_value,
                    double_value,
                    boolean_value,
                    binary_length,
                    binary_reference_id,
                ])
            })
            .map_err(sqlite_error)?;
        Ok(())
    }
}

impl ExportSink for SqliteSink {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        self.node_count += 1;
        let node_id = self.node_count;
        self.ancestry.truncate(node.depth);
        let parent_id = if node.depth == 0 {
            // The export root: record its full path as the anchor the
            // node_paths view reconstructs every path from.
            self.connection()
                .prepare_cached("INSERT INTO export (root_path) VALUES (?)")
                .and_then(|mut statement| statement.execute(params![node.path]))
                .map_err(sqlite_error)?;
            None
        } else {
            Some(self.ancestry[node.depth - 1])
        };
        self.ancestry.push(node_id);

        // The name is the path's last segment ("" for "/").
        let name = node.path.rsplit('/').next().unwrap_or(node.path);
        let name_id = self.intern(name)?;
        let primary_type = node.properties.iter().find_map(|property| {
            if property.name != "jcr:primaryType" {
                return None;
            }
            match &property.values {
                PropertyValues::Single(PropertyValue::Name(name)) => Some(name.as_str()),
                _ => None,
            }
        });
        let primary_type_id = match primary_type {
            Some(name) => Some(self.intern(name)?),
            None => None,
        };
        self.connection()
            .prepare_cached(
                "INSERT INTO nodes (id, parent_id, name_id, depth, primary_type_id) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    node_id,
                    parent_id,
                    name_id,
                    node.depth as i64,
                    primary_type_id,
                ])
            })
            .map_err(sqlite_error)?;

        for property in node.properties {
            let type_id = i64::from(property.property_type as u8);
            match &property.values {
                PropertyValues::Single(value) => {
                    self.write_property_row(
                        node_id,
                        &property.name,
                        type_id,
                        false,
                        0,
                        Some(value),
                    )?;
                }
                PropertyValues::Multiple(values) if values.is_empty() => {
                    // The marker row: without it, an empty multi-valued
                    // property would vanish from the export entirely.
                    self.write_property_row(
                        node_id,
                        &property.name,
                        type_id,
                        true,
                        EMPTY_MULTI_POSITION,
                        None,
                    )?;
                }
                PropertyValues::Multiple(values) => {
                    for (position, value) in values.iter().enumerate() {
                        self.write_property_row(
                            node_id,
                            &property.name,
                            type_id,
                            true,
                            position as i64,
                            Some(value),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.connection()
            .execute_batch("COMMIT;")
            .map_err(sqlite_error)?;
        if self.create_indexes {
            self.connection()
                .execute_batch(
                    "CREATE INDEX nodes_by_parent ON nodes(parent_id);
                     CREATE INDEX properties_by_name ON properties(name_id, type_id);",
                )
                .map_err(sqlite_error)?;
        }
        // Statistics after the indexes, so sqlite_stat1 covers them.
        self.connection()
            .execute_batch("ANALYZE;")
            .map_err(sqlite_error)?;
        // The export is complete; confirm it landed where the caller
        // asked. On Unix the bytes went through the descriptor either
        // way — this only reports a pathname displaced mid-export. On
        // Windows the guard's share mode makes displacement impossible.
        let still_ours = self
            .guard
            .as_ref()
            .is_some_and(|guard| path_still_names(&self.output_path, guard));
        if !still_ours {
            return Err(froe::Error::InvalidFormat {
                details: format!(
                    "output file {} was replaced during the export; the completed database \
                     is not at that path, and the replacement was never touched",
                    self.output_path.display()
                ),
            });
        }
        self.completed = true;
        Ok(())
    }
}

impl Drop for SqliteSink {
    /// An unfinished export leaves no file behind — but removes the
    /// output only while the path still names the file this sink
    /// created, never a replacement.
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let still_ours = self
            .guard
            .as_ref()
            .is_some_and(|guard| path_still_names(&self.output_path, guard));
        // Release the handles first: on Windows their sharing mode would
        // otherwise block the removal itself.
        drop(self.connection.take());
        drop(self.guard.take());
        if still_ours {
            let _ = std::fs::remove_file(&self.output_path);
        }
    }
}

/// Wraps a `SQLite` library error as an output error.
fn sqlite_error(error: rusqlite::Error) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use froe::content::PropertyType;
    use froe::store::Repository;
    use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
    use froe::writer::store_writer::WritableRepository;
    use rusqlite::Connection;

    use super::{BoundedStringCache, SqliteExportOptions, SqliteSink};
    use crate::export::export_subtree;

    #[test]
    fn an_evicted_string_reinterns_to_the_same_identifier() {
        // The table is the dictionary's authority and the in-memory map is
        // only a shortcut, so a string evicted between two sightings must
        // come back with the id the first sighting created. Getting this
        // wrong would silently split one string across two dictionary rows
        // and corrupt every foreign key pointing at it.
        let mut cache = BoundedStringCache::new(0);
        cache.insert("jcr:primaryType".to_owned(), 7);
        assert_eq!(cache.get("jcr:primaryType"), None, "a zero budget evicts");

        let mut cache = BoundedStringCache::new(1024);
        cache.insert("nt:unstructured".to_owned(), 3);
        assert_eq!(cache.get("nt:unstructured"), Some(3));
        cache.insert("nt:unstructured".to_owned(), 3);
        assert_eq!(
            cache.get("nt:unstructured"),
            Some(3),
            "reinsertion is idempotent"
        );
    }

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-sqlite-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create");
            Self { path }
        }

        fn store(&self) -> std::path::PathBuf {
            self.path.join("segmentstore")
        }

        fn database(&self) -> std::path::PathBuf {
            self.path.join("export.db")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Writes a store whose content tree is `/content/jcr:content`, with
    /// one property of every physical value shape on `/content`.
    fn populate(directory: &std::path::Path) {
        let store = WritableRepository::open(directory).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let page_content = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("jcr:content");

        let title = writer.write_string("Hello").expect("title");
        let tag_a = writer.write_string("a").expect("tag a");
        let tag_b = writer.write_string("b").expect("tag b");
        let count = writer.write_string("42").expect("count");
        let ratio = writer.write_string("2.5").expect("ratio");
        let flag = writer.write_string("true").expect("flag");
        let data = writer.write_binary_content(&[1, 2, 3]).expect("data");
        let external = writer
            .write_external_binary_identifier("blob-1")
            .expect("external");
        let single = |value| PropertyValuesToWrite::Single(value);
        let properties = [
            PropertyToWrite {
                name: "title".to_owned(),
                property_type: PropertyType::String,
                values: single(title),
            },
            PropertyToWrite {
                name: "tags".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(vec![tag_a, tag_b]),
            },
            PropertyToWrite {
                name: "empty_tags".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(Vec::new()),
            },
            PropertyToWrite {
                name: "count".to_owned(),
                property_type: PropertyType::Long,
                values: single(count),
            },
            PropertyToWrite {
                name: "ratio".to_owned(),
                property_type: PropertyType::Double,
                values: single(ratio),
            },
            PropertyToWrite {
                name: "flag".to_owned(),
                property_type: PropertyType::Boolean,
                values: single(flag),
            },
            PropertyToWrite {
                name: "data".to_owned(),
                property_type: PropertyType::Binary,
                values: single(data),
            },
            PropertyToWrite {
                name: "external".to_owned(),
                property_type: PropertyType::Binary,
                values: single(external),
            },
        ];
        let content = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::One {
                    name: "jcr:content".to_owned(),
                    node: page_content,
                },
                &properties,
            )
            .expect("content");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "content".to_owned(),
                    node: content,
                },
                &[],
            )
            .expect("root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.set_head(previous, head));
        store.close().expect("close");
    }

    /// Exports `path` from the store into the test database and returns
    /// a fresh read connection to it.
    fn export(directory: &TestDirectory, path: &str) -> Connection {
        export_with_options(
            directory,
            path,
            SqliteExportOptions {
                create_indexes: false,
            },
        )
    }

    /// Exports exactly like [`export`], additionally creating the lookup
    /// indexes.
    fn export_with_indexes(directory: &TestDirectory, path: &str) -> Connection {
        export_with_options(
            directory,
            path,
            SqliteExportOptions {
                create_indexes: true,
            },
        )
    }

    fn export_with_options(
        directory: &TestDirectory,
        path: &str,
        options: SqliteExportOptions,
    ) -> Connection {
        let repository = Repository::open(&directory.store()).expect("open");
        let mut sink =
            SqliteSink::create(&directory.store(), &directory.database(), options).expect("sink");
        export_subtree(&repository, path, None, &mut sink)
            .expect("export")
            .expect("root present");
        drop(sink);
        Connection::open(directory.database()).expect("reopen")
    }

    #[test]
    fn the_node_paths_view_reconstructs_full_paths() {
        let directory = TestDirectory::new("node-paths");
        populate(&directory.store());
        let connection = export(&directory, "/");

        let rows: Vec<(i64, String, i64, Option<String>)> = connection
            .prepare("SELECT id, path, depth, primary_type FROM node_paths ORDER BY id")
            .expect("prepare")
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, "/".to_owned(), 0, None),
                (
                    2,
                    "/content".to_owned(),
                    1,
                    Some("nt:unstructured".to_owned())
                ),
                (
                    3,
                    "/content/jcr:content".to_owned(),
                    2,
                    Some("nt:unstructured".to_owned())
                ),
            ]
        );
        // Parent links: 1 is the export root; 2's parent is 1; 3's is 2.
        let parents: Vec<Option<i64>> = connection
            .prepare("SELECT parent_id FROM nodes ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(parents, vec![None, Some(1), Some(2)]);
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "2.5 is exactly representable and round-trips bit-exact through SQLite REAL"
    )]
    fn the_properties_tables_carry_every_value_shape() {
        let directory = TestDirectory::new("properties");
        populate(&directory.store());
        let connection = export(&directory, "/");

        let value: String = connection
            .query_row(
                "SELECT value FROM properties_expanded \
                 WHERE path = '/content' AND name = 'title'",
                [],
                |row| row.get(0),
            )
            .expect("title");
        assert_eq!(value, "Hello");

        let tags: Vec<(String, i64)> = connection
            .prepare(
                "SELECT value, position FROM properties_expanded \
                 WHERE path = '/content' AND name = 'tags' ORDER BY position",
            )
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(
            tags,
            vec![("a".to_owned(), 0), ("b".to_owned(), 1)],
            "multi-valued properties explode with a position"
        );

        let marker: (i64, i64, Option<String>) = connection
            .query_row(
                "SELECT multiple, position, value FROM properties_expanded \
                 WHERE path = '/content' AND name = 'empty_tags'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("empty_tags marker");
        assert_eq!(marker, (1, -1, None), "the marker row keeps it visible");

        let count: (String, i64) = connection
            .query_row(
                "SELECT property_type, long_value FROM properties_expanded \
                 WHERE path = '/content' AND name = 'count'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count");
        assert_eq!(count, ("Long".to_owned(), 42));

        let ratio: f64 = connection
            .query_row(
                "SELECT double_value FROM properties_expanded \
                 WHERE path = '/content' AND name = 'ratio'",
                [],
                |row| row.get(0),
            )
            .expect("ratio");
        assert_eq!(ratio, 2.5);

        let flag: i64 = connection
            .query_row(
                "SELECT boolean_value FROM properties_expanded \
                 WHERE path = '/content' AND name = 'flag'",
                [],
                |row| row.get(0),
            )
            .expect("flag");
        assert_eq!(flag, 1);

        let data: (String, i64) = connection
            .query_row(
                "SELECT property_type, binary_length FROM properties_expanded \
                 WHERE path = '/content' AND name = 'data'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("data");
        assert_eq!(data, ("Binary".to_owned(), 3));

        let external: String = connection
            .query_row(
                "SELECT binary_reference FROM properties_expanded \
                 WHERE path = '/content' AND name = 'external'",
                [],
                |row| row.get(0),
            )
            .expect("external");
        assert_eq!(external, "blob-1");
    }

    #[test]
    fn strings_are_stored_once() {
        let directory = TestDirectory::new("strings");
        populate(&directory.store());
        let connection = export(&directory, "/");

        let occurrences: i64 = connection
            .query_row(
                "SELECT count(*) FROM strings WHERE value = 'nt:unstructured'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(occurrences, 1, "two nodes share one dictionary entry");

        let seeded: i64 = connection
            .query_row("SELECT count(*) FROM property_types", [], |row| row.get(0))
            .expect("property_types");
        assert_eq!(seeded, 12);

        let root_path: String = connection
            .query_row("SELECT root_path FROM export", [], |row| row.get(0))
            .expect("root_path");
        assert_eq!(root_path, "/");
    }

    #[test]
    fn a_subtree_export_anchors_paths_at_the_export_root() {
        let directory = TestDirectory::new("subtree");
        populate(&directory.store());
        let connection = export(&directory, "/content");

        let paths: Vec<String> = connection
            .prepare("SELECT path FROM node_paths ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(paths, vec!["/content", "/content/jcr:content"]);

        let root_path: String = connection
            .query_row("SELECT root_path FROM export", [], |row| row.get(0))
            .expect("root_path");
        assert_eq!(root_path, "/content");
    }

    #[test]
    fn the_schema_is_without_rowid_and_analyzed() {
        let directory = TestDirectory::new("schema");
        populate(&directory.store());
        let connection = export(&directory, "/");

        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'properties'",
                [],
                |row| row.get(0),
            )
            .expect("properties ddl");
        assert!(
            sql.contains("WITHOUT ROWID"),
            "clustered on the natural key"
        );

        let analyzed: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE name = 'sqlite_stat1'",
                [],
                |row| row.get(0),
            )
            .expect("stat1");
        assert!(analyzed, "finish runs ANALYZE");

        let indexed: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE name = 'nodes_by_parent'",
                [],
                |row| row.get(0),
            )
            .expect("indexes");
        assert!(!indexed, "no secondary indexes by default");
    }

    #[test]
    fn indexes_are_created_on_request() {
        let directory = TestDirectory::new("indexes");
        populate(&directory.store());
        let connection = export_with_indexes(&directory, "/");

        for index in ["nodes_by_parent", "properties_by_name"] {
            let present: bool = connection
                .query_row(
                    "SELECT count(*) > 0 FROM sqlite_master WHERE name = ?",
                    [index],
                    |row| row.get(0),
                )
                .expect("index");
            assert!(present, "{index} must exist with create_indexes");
        }
    }

    #[test]
    fn the_export_never_overwrites_an_existing_file() {
        let directory = TestDirectory::new("never-overwrites");
        populate(&directory.store());
        let database = directory.database();
        std::fs::write(&database, b"someone else's data").expect("seed");

        let result = SqliteSink::create(
            &directory.store(),
            &database,
            SqliteExportOptions::default(),
        );
        assert!(result.is_err(), "an existing file must be refused");
        assert_eq!(
            std::fs::read(&database).expect("read"),
            b"someone else's data",
            "the existing file must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_check_detects_a_replaced_file() {
        let directory = TestDirectory::new("identity");
        let first = directory.path.join("first.db");
        let second = directory.path.join("second.db");
        let guard = std::fs::File::create_new(&first).expect("create first");
        std::fs::File::create_new(&second).expect("create second");

        assert!(
            super::path_still_names(&first, &guard),
            "the created file is named by its path"
        );
        assert!(
            !super::path_still_names(&second, &guard),
            "a different file at the path must be detected"
        );
    }

    /// The ABA attack from the design review: the exclusively created
    /// file is renamed away, a victim takes the pathname, and the
    /// export runs to its (failing) end. The victim must be neither
    /// modified nor deleted, and our file must have received the data
    /// through the descriptor.
    #[cfg(unix)]
    #[test]
    fn a_mid_export_pathname_swap_neither_modifies_nor_deletes_the_replacement() {
        let directory = TestDirectory::new("aba");
        populate(&directory.store());
        let database = directory.database();
        let aside = directory.path.join("aside.db");
        let victim = directory.path.join("victim.db");

        let repository = Repository::open(&directory.store()).expect("open");
        let mut sink = SqliteSink::create(
            &directory.store(),
            &database,
            SqliteExportOptions::default(),
        )
        .expect("sink");

        // The swap: our freshly created file aside, a victim at the
        // pathname.
        std::fs::rename(&database, &aside).expect("aside");
        std::fs::write(&victim, b"precious original content").expect("victim seed");
        std::fs::rename(&victim, &database).expect("victim into place");

        let result = export_subtree(&repository, "/", None, &mut sink);
        assert!(result.is_err(), "finish must report the displacement");
        assert_eq!(
            std::fs::read(&database).expect("victim read"),
            b"precious original content",
            "the replacement must be unmodified"
        );
        drop(sink);
        assert_eq!(
            std::fs::read(&database).expect("victim read"),
            b"precious original content",
            "and must not be deleted by cleanup either"
        );

        // Our file kept receiving the export through the descriptor.
        let connection = Connection::open(&aside).expect("aside");
        let nodes: i64 = connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .expect("nodes");
        assert_eq!(nodes, 3, "the export wrote into the file it created");
    }

    #[test]
    fn an_abandoned_export_leaves_no_file_behind() {
        let directory = TestDirectory::new("abandoned");
        populate(&directory.store());
        let database = directory.database();
        {
            let _sink = SqliteSink::create(
                &directory.store(),
                &database,
                SqliteExportOptions::default(),
            )
            .expect("sink");
            // No export, no finish: the sink is dropped uncompleted.
        }
        assert!(!database.exists(), "an abandoned export must not linger");
    }
}
