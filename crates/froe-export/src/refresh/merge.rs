//! Merging the old rows with the delta rows: old rows inside a dirty
//! range are dropped and the range's replacements written in their
//! place, keeping the result nearly document-ordered.

use super::{
    DirtyRange, ExportProvenance, ExportSink, NodeRow, ParquetExportOptions, ParquetSink, Path,
    PropertyRow, RangeIndex, RefCell, Repository, create_export_output, path_in_range,
};

/// The verdict of merging: fresh merged files, or the discovery that
/// the existing files' rows do not decode as a froe export.
#[derive(Debug)]
pub(crate) enum MergeVerdict {
    Done,
    Unusable(String),
}

/// Merges the old tables with the delta tables into fresh files at the
/// merged paths, stamped with `provenance`. The old tables arrive as
/// the readers validation opened and the refresh held ever since — an
/// open handle keeps its bytes even if its pathname is replaced, so a
/// base swapped by a writer outside the lock can never be merged and
/// stamped under the new head.
///
/// Each table carries its own three files together, so the nodes and
/// properties sides cannot be interleaved by a transposed argument.
#[derive(Clone, Copy)]
pub(crate) struct TableMerge<'merge> {
    /// The table already on disk, opened by the caller and validated.
    pub(crate) previous: &'merge ::parquet::file::reader::SerializedFileReader<std::fs::File>,
    /// The delta this run wrote for the dirty ranges.
    pub(crate) delta: &'merge Path,
    /// Where the merged table is written before the atomic swap.
    pub(crate) merged: &'merge Path,
}

pub(crate) fn merge_tables(
    repository: &Repository,
    nodes: TableMerge<'_>,
    properties: TableMerge<'_>,
    ranges: &[DirtyRange],
    provenance: &ExportProvenance,
    options: &ParquetExportOptions,
) -> froe::Result<MergeVerdict> {
    use ::parquet::file::reader::SerializedFileReader;

    let open_delta = |path: &Path| -> froe::Result<SerializedFileReader<std::fs::File>> {
        SerializedFileReader::new(std::fs::File::open(path)?).map_err(parquet_read_error)
    };
    let delta_nodes_reader = open_delta(nodes.delta)?;
    let delta_properties_reader = open_delta(properties.delta)?;

    // A decode or read failure inside an old stream does not abort the
    // merge; it ends the affected stream, and the flag turns the
    // verdict into Unusable afterwards — the partial merged files are
    // then discarded and a full export replaces the unparseable base.
    let failure = RefCell::new(None::<String>);

    let nodes_out = create_export_output(repository.directory(), nodes.merged)?;
    let properties_out = create_export_output(repository.directory(), properties.merged)?;
    let mut sink = ParquetSink::new_with_provenance(
        std::io::BufWriter::with_capacity(1 << 20, nodes_out),
        std::io::BufWriter::with_capacity(1 << 20, properties_out),
        options,
        provenance,
    )?;
    let index = RangeIndex::new(ranges);
    merge_row_streams(
        NodeRows::new(nodes.previous, &failure, RowSource::PreviousExport)?,
        NodeRows::new(&delta_nodes_reader, &failure, RowSource::FreshDelta)?,
        ranges,
        &index,
        |row| {
            sink.append_node_row(
                &row.path,
                row.parent_path.as_deref(),
                &row.name,
                row.depth,
                row.primary_type.as_deref(),
            )
        },
    )?;
    merge_row_streams(
        PropertyRows::new(properties.previous, &failure, RowSource::PreviousExport)?,
        PropertyRows::new(&delta_properties_reader, &failure, RowSource::FreshDelta)?,
        ranges,
        &index,
        |row| {
            sink.append_property_columns(
                &row.path,
                &row.name,
                &row.property_type,
                row.multiple,
                row.position,
                row.value.as_deref(),
                row.long_value,
                row.double_value,
                row.boolean_value,
                row.binary_length,
                row.binary_reference.as_deref(),
            )
        },
    )?;
    sink.finish()?;
    if let Some(reason) = failure.into_inner() {
        return Ok(MergeVerdict::Unusable(reason));
    }
    Ok(MergeVerdict::Done)
}

/// The row shape the merge needs: every table's rows carry their node
/// path.
pub(crate) trait MergeRow {
    /// The row's node path.
    fn path(&self) -> &str;
}

impl MergeRow for NodeRow {
    fn path(&self) -> &str {
        &self.path
    }
}

impl MergeRow for PropertyRow {
    fn path(&self) -> &str {
        &self.path
    }
}

/// Merges one table's old and delta rows into `write`, streaming.
///
/// Old rows inside a dirty range are dropped wherever they appear —
/// the containment test, not row order, decides — and each range's
/// replacement rows (contiguous in the delta, ranges exported in path
/// order) are written where the merge walk passes the range. Rows
/// therefore stay ordered exactly as far as the old file and document
/// order agree, which keeps path-column statistics selective.
pub(crate) fn merge_row_streams<R: MergeRow>(
    old_rows: impl Iterator<Item = froe::Result<R>>,
    delta_rows: impl Iterator<Item = froe::Result<R>>,
    ranges: &[DirtyRange],
    index: &RangeIndex<'_>,
    mut write: impl FnMut(R) -> froe::Result<()>,
) -> froe::Result<()> {
    let mut old_rows = old_rows.peekable();
    let mut delta_rows = delta_rows.peekable();
    for range in ranges {
        loop {
            match old_rows.peek() {
                Some(Ok(row)) if index.contains(row.path()) => {
                    old_rows.next();
                }
                Some(Ok(row)) if row.path() < range.path.as_str() => {
                    write(old_rows.next().expect("the peeked row")?)?;
                }
                Some(Ok(_)) | None => break,
                Some(Err(_)) => return Err(take_error(&mut old_rows)),
            }
        }
        loop {
            match delta_rows.peek() {
                Some(Ok(row)) if path_in_range(row.path(), range) => {
                    write(delta_rows.next().expect("the peeked row")?)?;
                }
                Some(Ok(_)) | None => break,
                Some(Err(_)) => return Err(take_error(&mut delta_rows)),
            }
        }
    }
    for row in old_rows {
        let row = row?;
        if !index.contains(row.path()) {
            write(row)?;
        }
    }
    if delta_rows.next().is_some() {
        return Err(froe::Error::InvalidFormat {
            details: "the refresh delta holds rows outside the changed ranges".to_owned(),
        });
    }
    Ok(())
}

/// Extracts the error a peek showed from a row stream.
pub(crate) fn take_error<R, I: Iterator<Item = froe::Result<R>>>(
    rows: &mut std::iter::Peekable<I>,
) -> froe::Error {
    if let Some(Err(error)) = rows.next() {
        return error;
    }
    froe::Error::InvalidFormat {
        details: "a row stream changed underneath the merge".to_owned(),
    }
}

/// Where a row stream came from, which decides whether a read error is
/// fatal or merely recorded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowSource {
    /// The export already on disk, which may legitimately be corrupt: a
    /// read error is recorded and ends the stream.
    PreviousExport,
    /// A delta this run just wrote, where a read error is a hard failure.
    FreshDelta,
}

/// An iterator decoding nodes-table rows. A decode failure records in
/// `failure` and ends the stream. A read error is hard, except on the
/// previous export's stream, where it records instead.
pub(crate) struct NodeRows<'a> {
    pub(crate) inner: ::parquet::record::reader::RowIter<'a>,
    pub(crate) failure: &'a RefCell<Option<String>>,
    pub(crate) source: RowSource,
}

impl<'a> NodeRows<'a> {
    /// Decodes nodes-table rows from `reader`.
    pub(crate) fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
        source: RowSource,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
            source,
        })
    }
}

impl Iterator for NodeRows<'_> {
    type Item = froe::Result<NodeRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok(row) => {
                if let Some(decoded) = NodeRow::decode(&row) {
                    Some(Ok(decoded))
                } else {
                    record_read_failure(
                        self.failure,
                        "an export file's rows do not match the export schema; \
                         the files were not written by this export"
                            .to_owned(),
                    );
                    None
                }
            }
            Err(error) => {
                if self.source == RowSource::PreviousExport {
                    record_read_failure(
                        self.failure,
                        format!("the existing export's rows are not readable: {error}"),
                    );
                    None
                } else {
                    Some(Err(parquet_read_error(error)))
                }
            }
        }
    }
}

/// An iterator decoding properties-table rows; behaves like
/// [`NodeRows`].
pub(crate) struct PropertyRows<'a> {
    pub(crate) inner: ::parquet::record::reader::RowIter<'a>,
    pub(crate) failure: &'a RefCell<Option<String>>,
    pub(crate) source: RowSource,
}

impl<'a> PropertyRows<'a> {
    /// Decodes properties-table rows from `reader`.
    pub(crate) fn new(
        reader: &'a ::parquet::file::reader::SerializedFileReader<std::fs::File>,
        failure: &'a RefCell<Option<String>>,
        source: RowSource,
    ) -> froe::Result<Self> {
        use ::parquet::file::reader::FileReader;
        Ok(Self {
            inner: reader.get_row_iter(None).map_err(parquet_read_error)?,
            failure,
            source,
        })
    }
}

impl Iterator for PropertyRows<'_> {
    type Item = froe::Result<PropertyRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok(row) => {
                if let Some(decoded) = PropertyRow::decode(&row) {
                    Some(Ok(decoded))
                } else {
                    record_read_failure(
                        self.failure,
                        "an export file's rows do not match the export schema; \
                         the files were not written by this export"
                            .to_owned(),
                    );
                    None
                }
            }
            Err(error) => {
                if self.source == RowSource::PreviousExport {
                    record_read_failure(
                        self.failure,
                        format!("the existing export's rows are not readable: {error}"),
                    );
                    None
                } else {
                    Some(Err(parquet_read_error(error)))
                }
            }
        }
    }
}

/// Records the first read or decode failure; later failures keep the
/// first's message.
pub(crate) fn record_read_failure(failure: &RefCell<Option<String>>, message: String) {
    let mut failure = failure.borrow_mut();
    if failure.is_none() {
        *failure = Some(message);
    }
}

/// Wraps a Parquet read error as an output error.
pub(crate) fn parquet_read_error(error: ::parquet::errors::ParquetError) -> froe::Error {
    froe::Error::InputOutput(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::{MergeRow, merge_row_streams};
    use crate::parquet::ParquetExportOptions;
    use crate::refresh::ranges::{DirtyRange, RangeIndex, Replacement};
    use crate::refresh::refresh_parquet_export;
    use crate::refresh::test_support::*;
    use froe::store::Repository;

    /// A merge test row: just a path.
    struct TestRow(&'static str);

    impl MergeRow for TestRow {
        fn path(&self) -> &str {
            self.0
        }
    }

    fn merged(
        old: Vec<&'static str>,
        delta: Vec<&'static str>,
        ranges: &[DirtyRange],
    ) -> Vec<&'static str> {
        let index = RangeIndex::new(ranges);
        let mut out = Vec::new();
        merge_row_streams(
            old.into_iter().map(|path| Ok(TestRow(path))),
            delta.into_iter().map(|path| Ok(TestRow(path))),
            ranges,
            &index,
            |row| {
                out.push(row.0);
                Ok(())
            },
        )
        .expect("merge");
        out
    }

    fn excise(path: &str) -> DirtyRange {
        DirtyRange {
            path: path.to_owned(),
            subtree: true,
            replacement: Replacement::Excise,
        }
    }

    #[test]
    fn the_merge_without_ranges_passes_the_old_rows_through() {
        assert_eq!(
            merged(vec!["/a", "/a/b"], Vec::new(), &[]),
            vec!["/a", "/a/b"]
        );
    }

    #[test]
    fn the_merge_excises_a_subtree() {
        assert_eq!(
            merged(
                vec!["/a", "/a/b", "/a/b/x", "/a/c"],
                Vec::new(),
                &[excise("/a/b")],
            ),
            vec!["/a", "/a/c"]
        );
    }

    #[test]
    fn the_merge_filters_dirty_rows_wherever_they_sort() {
        // Non-alphabetical storage order: the dirty subtree's rows sit
        // after a row that sorts past the range.
        assert_eq!(
            merged(
                vec!["/a/z", "/a/m/1", "/a/m/2", "/a/a"],
                Vec::new(),
                &[excise("/a/m")],
            ),
            vec!["/a/z", "/a/a"],
            "containment, not position, decides — dirty rows never leak"
        );
    }

    #[test]
    fn the_merge_injects_an_added_subtree() {
        assert_eq!(
            merged(
                vec!["/a", "/a/c"],
                vec!["/a/b", "/a/b/x"],
                &[DirtyRange {
                    path: "/a/b".to_owned(),
                    subtree: true,
                    replacement: Replacement::ReExport { depth: None },
                }],
            ),
            vec!["/a", "/a/b", "/a/b/x", "/a/c"]
        );
    }

    #[test]
    fn the_merge_replaces_a_changed_nodes_own_rows_only() {
        assert_eq!(
            merged(
                vec!["/a", "/a/b", "/a/b/c"],
                vec!["/a/b"],
                &[DirtyRange {
                    path: "/a/b".to_owned(),
                    subtree: false,
                    replacement: Replacement::ReExport { depth: Some(0) },
                }],
            ),
            vec!["/a", "/a/b", "/a/b/c"],
            "the descendant row survives the exact replacement"
        );
    }

    #[test]
    fn the_merge_handles_multiple_ranges() {
        assert_eq!(
            merged(
                vec!["/r", "/r/a", "/r/a/x", "/r/b", "/r/c"],
                vec!["/r/a", "/r/d", "/r/d/y"],
                &[
                    DirtyRange {
                        path: "/r/a".to_owned(),
                        subtree: true,
                        replacement: Replacement::ReExport { depth: Some(0) },
                    },
                    excise("/r/c"),
                    DirtyRange {
                        path: "/r/d".to_owned(),
                        subtree: true,
                        replacement: Replacement::ReExport { depth: None },
                    },
                ],
            ),
            vec!["/r", "/r/a", "/r/b", "/r/d", "/r/d/y"],
            "/r/a's subtree collapses to its new root row, /r/c is excised, /r/d injected"
        );
    }

    #[test]
    fn the_merge_refuses_leftover_delta_rows() {
        let index = RangeIndex::new(&[]);
        let result = merge_row_streams(
            Vec::new().into_iter().map(|path| Ok(TestRow(path))),
            vec!["/stray"].into_iter().map(|path| Ok(TestRow(path))),
            &[],
            &index,
            |_| Ok(()),
        );
        assert!(
            result.is_err(),
            "delta rows outside every range are an error"
        );
    }

    #[test]
    fn the_merge_propagates_stream_errors() {
        let failing: Vec<froe::Result<TestRow>> = vec![Err(froe::Error::InvalidFormat {
            details: "corrupt".to_owned(),
        })];
        let index = RangeIndex::new(&[]);
        let result = merge_row_streams(
            failing.into_iter(),
            Vec::new().into_iter(),
            &[],
            &index,
            |_| Ok(()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_same_stamp_swap_mid_refresh_is_never_merged() {
        let directory = TestDirectory::new("swap-mid-refresh");
        populate_first(&directory.store());
        let first_revision = full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        // The rogue pair: a different repository's export carrying the
        // same stamp — footer equality is not content identity.
        let rogue = TestDirectory::new("swap-rogue");
        revise(&rogue.store(), |writer| {
            let evil = writer.leaf(&[]);
            let content = writer.child("evil", evil, &[]);
            writer.child("content", content, &[])
        });
        full_export(
            &rogue.store(),
            "/content",
            None,
            &rogue.export(),
            Some(first_revision),
        );

        populate_second(&directory.store());

        // Rename the rogue pair over the base from the delta-export
        // progress callback: after validation, before the merge.
        let mut swapped = false;
        let mut on_node = |_: u64| {
            if !swapped {
                swapped = true;
                for name in ["nodes.parquet", "properties.parquet"] {
                    std::fs::rename(rogue.export().join(name), directory.export().join(name))
                        .expect("swap");
                }
            }
        };
        let repository = Repository::open(&directory.store()).expect("open");
        let outcome = refresh_parquet_export(
            &repository,
            "/content",
            None,
            &directory.export(),
            &ParquetExportOptions::default(),
            &mut on_node,
        )
        .expect("refresh");
        assert!(swapped, "the swap really happened mid-refresh: {outcome:?}");

        // The merge consumed the readers validation opened — the honest
        // base — so the result is exactly a full export of the new head.
        let reference = directory.path.join("reference");
        full_export(&directory.store(), "/content", None, &reference, None);
        assert_eq!(
            node_rows(&directory.export()),
            node_rows(&reference),
            "the swapped-in rogue rows never entered the merge"
        );
        assert_eq!(
            property_rows(&directory.export()),
            property_rows(&reference)
        );
        assert!(
            node_rows(&directory.export())
                .iter()
                .all(|row| !row.path.contains("evil")),
            "no rogue row survives under the new stamp"
        );
    }
}
