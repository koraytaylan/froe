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

mod schema;
mod target;
#[cfg(test)]
mod test_support;

pub(crate) use schema::*;
pub(crate) use target::*;

/// Tuning knobs for a `SQLite` export.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteExportOptions {
    /// Adds `nodes(parent_id)` and `properties(name_id, type_id)`
    /// indexes after the export. Indexes roughly double the file size;
    /// keep them off for an artifact consumed analytically, enable them
    /// for repeated point lookups in `SQLite` itself.
    pub create_indexes: bool,
}

/// An [`ExportSink`] writing the interned `SQLite` schema described in the
/// [module documentation](self).
pub struct SqliteSink {
    pub(crate) connection: Option<Connection>,
    /// Recently interned strings, so the common repeats — primary types,
    /// property names — do not round-trip to `SQLite` on every row.
    ///
    /// The authoritative dictionary is the `strings` table itself, which is
    /// why this may evict. Holding every distinct string of the export in
    /// memory was the one thing keeping this sink from streaming: names and
    /// primary types repeat, but textual property values do not, so the map
    /// grew with the content rather than with the vocabulary.
    pub(crate) strings: BoundedStringCache,
    /// The depth-indexed stack of ancestor node ids: after processing a
    /// node, `ancestry[depth]` is its id, so its children's `parent_id`
    /// is one lookup away.
    pub(crate) ancestry: Vec<i64>,
    pub(crate) node_count: i64,
    pub(crate) create_indexes: bool,
    /// The exclusively created output file, held for the whole export:
    /// on Unix it is what `SQLite` writes to (through the descriptor),
    /// and everywhere it is the identity cleanup compares against.
    pub(crate) guard: Option<std::fs::File>,
    pub(crate) output_path: PathBuf,
    /// Set by `finish` — an unfinished export is a partial file whose
    /// cleanup the drop implementation owns.
    pub(crate) completed: bool,
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
    pub(super) fn open_anchored(
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
    pub(super) fn initialize(path: &Path, guard: &std::fs::File) -> froe::Result<Connection> {
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
    pub(super) fn connection(&self) -> &Connection {
        self.connection
            .as_ref()
            .expect("the connection is taken only by the drop implementation")
    }

    /// Returns the dictionary id of `text`, inserting it into the
    /// strings table on first sight. Strings are inserted before any
    /// row referencing them, so the foreign keys stay valid even under
    /// `PRAGMA foreign_key_check`.
    pub(super) fn intern(&mut self, text: &str) -> froe::Result<i64> {
        if let Some(string_identifier) = self.strings.get(text) {
            return Ok(string_identifier);
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
        let string_identifier: i64 = connection
            .prepare_cached("SELECT id FROM strings WHERE value = ?")
            .and_then(|mut statement| statement.query_row(params![text], |row| row.get(0)))
            .map_err(sqlite_error)?;
        self.strings.insert(text.to_owned(), string_identifier);
        Ok(string_identifier)
    }

    /// Appends one row to the properties table. `position` is the
    /// value's index within a multi-valued property (0 for
    /// single-valued), [`EMPTY_MULTI_POSITION`] for the marker row of an
    /// empty one; `value` is `None` on the marker row.
    pub(super) fn write_property_row(
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
pub(crate) fn sqlite_error(error: rusqlite::Error) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

/// The `position` marking an empty multi-valued property's marker row.
pub(crate) const EMPTY_MULTI_POSITION: i64 = -1;

#[cfg(test)]
mod tests {
    use crate::sqlite::test_support::{TestDirectory, export, export_with_indexes, populate};

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
}
