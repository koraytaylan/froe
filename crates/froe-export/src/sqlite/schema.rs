//! The database an export writes into: its tables and views, and the
//! bounded dictionary that stores each distinct string once.

use super::{HashMap, VecDeque};

/// Tables and views, created inside the export's single transaction.
pub(crate) const SCHEMA: &str = "
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

/// Byte budget for the in-memory half of the string dictionary.
pub(crate) const STRING_DICTIONARY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// A byte-budgeted string-to-id cache in front of the `strings` table.
///
/// Deliberately small and local: the table is the authority, so this only
/// has to make the repeated vocabulary — primary types, property names —
/// cheap. Eviction in insertion order costs a round-trip to `SQLite`, never a
/// wrong id, because a miss re-reads the row the first insert created.
pub(crate) struct BoundedStringCache {
    pub(crate) entries: HashMap<String, i64>,
    pub(crate) insertion_order: VecDeque<String>,
    pub(crate) used_bytes: usize,
    pub(crate) budget_bytes: usize,
}

impl BoundedStringCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            used_bytes: 0,
            budget_bytes,
        }
    }

    pub(crate) fn get(&self, text: &str) -> Option<i64> {
        self.entries.get(text).copied()
    }

    pub(crate) fn insert(&mut self, text: String, id: i64) {
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

#[cfg(test)]
mod tests {
    use super::BoundedStringCache;
    use crate::sqlite::test_support::{TestDirectory, export, populate};

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
}
