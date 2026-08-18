//! The stamp both files carry: which froe wrote them, at which revision,
//! and over which root and depth. A refresh reuses an export only when
//! both stamps agree with each other and with what was asked for.

use super::{KeyValue, parquet_error};

/// The footer metadata key carrying the export format version.
pub(crate) const FORMAT_KEY: &str = "froe.format";

/// The only format version written so far.
pub(crate) const FORMAT_VERSION: &str = "1";

/// The footer metadata key carrying the head revision the export
/// reflects, in record identifier text form.
pub(crate) const REVISION_KEY: &str = "froe.revision";

/// The footer metadata key carrying the exported root path, normalized
/// to a leading-slash, trailing-slash-free form.
pub(crate) const ROOT_PATH_KEY: &str = "froe.root_path";

/// The footer metadata key carrying the depth limit: a decimal number,
/// or `"none"` for an unlimited export.
pub(crate) const DEPTH_LIMIT_KEY: &str = "froe.depth_limit";

/// What an existing export claims to reflect: the provenance a Parquet
/// export stamps into both file footers. A refresh validates the stamp
/// against the requested export before trusting the files as a base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportProvenance {
    pub(crate) revision: String,
    pub(crate) root_path: String,
    pub(crate) depth_limit: Option<usize>,
}

impl ExportProvenance {
    /// Creates the provenance of an export at `revision` (a record
    /// identifier in text form) of `root_path` with `depth_limit`.
    /// The root path is normalized, so differently spelled requests of
    /// the same subtree (`"content"`, `"/content/"`) stamp identically.
    #[must_use]
    pub fn new(revision: String, root_path: &str, depth_limit: Option<usize>) -> Self {
        let segments: Vec<&str> = root_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let root_path = if segments.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", segments.join("/"))
        };
        Self {
            revision,
            root_path,
            depth_limit,
        }
    }

    /// The head revision the export reflects.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The exported root path, normalized.
    #[must_use]
    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    /// The export's depth limit, when limited.
    #[must_use]
    pub fn depth_limit(&self) -> Option<usize> {
        self.depth_limit
    }

    /// The footer key-value pairs stamping an export.
    pub(crate) fn to_metadata(&self) -> Vec<KeyValue> {
        let depth_limit = self
            .depth_limit
            .map_or_else(|| "none".to_owned(), |limit| limit.to_string());
        vec![
            KeyValue::new(FORMAT_KEY.to_owned(), FORMAT_VERSION.to_owned()),
            KeyValue::new(REVISION_KEY.to_owned(), self.revision.clone()),
            KeyValue::new(ROOT_PATH_KEY.to_owned(), self.root_path.clone()),
            KeyValue::new(DEPTH_LIMIT_KEY.to_owned(), depth_limit),
        ]
    }

    /// Reads the provenance from one file's footer key-value pairs, or
    /// `None` when any key is missing or malformed — the file is then
    /// not a froe Parquet export a refresh can build on.
    pub(crate) fn from_metadata(metadata: &[KeyValue]) -> Option<Self> {
        let get = |key: &str| {
            metadata
                .iter()
                .find(|pair| pair.key == key)
                .and_then(|pair| pair.value.as_deref())
        };
        if get(FORMAT_KEY)? != FORMAT_VERSION {
            return None;
        }
        let depth_limit = match get(DEPTH_LIMIT_KEY)? {
            "none" => None,
            text => Some(text.parse().ok()?),
        };
        Some(Self {
            revision: get(REVISION_KEY)?.to_owned(),
            root_path: get(ROOT_PATH_KEY)?.to_owned(),
            depth_limit,
        })
    }
}

/// Reads the provenance stamped into one Parquet export file's footer,
/// or `Ok(None)` when the file carries none.
pub fn read_export_provenance(path: &std::path::Path) -> froe::Result<Option<ExportProvenance>> {
    use ::parquet::file::reader::SerializedFileReader;

    let file = std::fs::File::open(path)?;
    let reader = SerializedFileReader::new(file).map_err(parquet_error)?;
    Ok(provenance_of(&reader))
}

/// The provenance of an already open reader — validating a file on the
/// exact handle its rows will be consumed from, so no pathname swap can
/// slip between check and use.
pub(crate) fn provenance_of(
    reader: &::parquet::file::reader::SerializedFileReader<std::fs::File>,
) -> Option<ExportProvenance> {
    use ::parquet::file::reader::FileReader;

    reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|pairs| ExportProvenance::from_metadata(pairs))
}

#[cfg(test)]
pub(crate) mod provenance_tests {
    use ::parquet::file::metadata::KeyValue;

    use crate::export::ExportSink;
    use crate::parquet::{
        DEPTH_LIMIT_KEY, ExportProvenance, FORMAT_KEY, ParquetExportOptions, ParquetSink,
        REVISION_KEY, ROOT_PATH_KEY, read_export_provenance,
    };

    #[test]
    fn the_root_path_normalizes() {
        let provenance = ExportProvenance::new("rev".to_owned(), "content/", None);
        assert_eq!(provenance.root_path(), "/content");
        assert_eq!(
            ExportProvenance::new("rev".to_owned(), "/", None).root_path(),
            "/"
        );
        assert_eq!(
            ExportProvenance::new("rev".to_owned(), "//a//b", None).root_path(),
            "/a/b"
        );
    }

    #[test]
    fn the_metadata_round_trips() {
        let provenance = ExportProvenance::new("abc.00000001".to_owned(), "/content", Some(3));
        assert_eq!(
            ExportProvenance::from_metadata(&provenance.to_metadata()),
            Some(provenance)
        );
        let unlimited = ExportProvenance::new("abc.00000001".to_owned(), "/", None);
        assert_eq!(
            ExportProvenance::from_metadata(&unlimited.to_metadata()),
            Some(unlimited)
        );
    }

    #[test]
    fn malformed_metadata_decodes_to_none() {
        let provenance = ExportProvenance::new("rev".to_owned(), "/content", None);
        let complete = provenance.to_metadata();
        // Every key is load-bearing.
        for dropped in [FORMAT_KEY, REVISION_KEY, ROOT_PATH_KEY, DEPTH_LIMIT_KEY] {
            let partial: Vec<KeyValue> = complete
                .iter()
                .filter(|pair| pair.key != dropped)
                .cloned()
                .collect();
            assert_eq!(
                ExportProvenance::from_metadata(&partial),
                None,
                "without {dropped} there is no provenance"
            );
        }
        let mut wrong_version = complete.clone();
        wrong_version[0] = KeyValue::new(FORMAT_KEY.to_owned(), "0".to_owned());
        assert_eq!(ExportProvenance::from_metadata(&wrong_version), None);
        let mut wrong_depth = complete;
        wrong_depth[3] = KeyValue::new(DEPTH_LIMIT_KEY.to_owned(), "deep".to_owned());
        assert_eq!(ExportProvenance::from_metadata(&wrong_depth), None);
    }

    #[test]
    fn a_written_file_carries_the_stamp() {
        let directory =
            std::env::temp_dir().join(format!("froe-parquet-provenance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create");
        let nodes_path = directory.join("nodes.parquet");
        let properties_path = directory.join("properties.parquet");
        let provenance = ExportProvenance::new("abc.00000001".to_owned(), "/content", Some(2));
        let mut sink = ParquetSink::new_with_provenance(
            std::fs::File::create(&nodes_path).expect("nodes"),
            std::fs::File::create(&properties_path).expect("properties"),
            &ParquetExportOptions::default(),
            &provenance,
        )
        .expect("sink");
        sink.finish().expect("finish");

        for path in [&nodes_path, &properties_path] {
            assert_eq!(
                read_export_provenance(path).expect("read"),
                Some(provenance.clone()),
                "{} carries the stamp",
                path.display()
            );
        }
        // An unstamped sink writes no provenance.
        let plain_nodes = directory.join("plain-nodes.parquet");
        let plain_properties = directory.join("plain-properties.parquet");
        let mut plain = ParquetSink::new(
            std::fs::File::create(&plain_nodes).expect("nodes"),
            std::fs::File::create(&plain_properties).expect("properties"),
            &ParquetExportOptions::default(),
        )
        .expect("sink");
        plain.finish().expect("finish");
        assert_eq!(read_export_provenance(&plain_nodes).expect("read"), None);

        let _ = std::fs::remove_dir_all(&directory);
    }
}
