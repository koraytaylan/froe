//! The judge's `enumerate` output, parsed so two indexes of the same
//! content can be compared as content rather than as text.
//!
//! `Corpus.enumerate` prints one line per field, term, posting, stored
//! value, doc value and norm, each keyed by Lucene's own **document
//! number**. That number is an artefact of the order documents were added
//! in, and the two sides of plan 0010's oracle do not share it: Oak's own
//! editor walks a node's children in the order its `MapRecord` yields them,
//! which is by the hash of each name, and froe's rebuild walks them sorted
//! by name. Neither order is part of the format — nothing in the index
//! records it, and no query can observe it — so comparing the two
//! enumerations line for line would compare the walk, not the index.
//!
//! What is comparable is every document *identified by its own `:path`*,
//! which Oak's document maker stores on every document it makes. This
//! module re-keys both enumerations by that path and renders them back in
//! one canonical order, so an equality is an equality of contents: the same
//! fields with the same options, the same terms with the same statistics,
//! the same postings with the same frequencies, positions and offsets, the
//! same stored values, doc values and norms, over the same documents.
//!
//! It also owns the **declared binary difference**. froe extracts no text,
//! so where Oak indexed a binary's extracted text froe indexed the
//! `TextExtractionError` marker under `--binary-text marker`. The exclusion
//! is applied at the posting level, to both sides, before any statistic is
//! derived: for each affected document and field the postings, the stored
//! value and the norm go, terms left with no postings go, and each
//! surviving term's document and total frequencies are recomputed from what
//! remains.

use std::collections::{BTreeMap, BTreeSet};

/// Oak's `FieldNames.PATH`, the stored field every document carries.
pub(crate) const PATH_FIELD: &str = ":path";

/// Oak's `FieldNames.FULLTEXT`, the field a node-scope binary's text goes
/// to.
pub(crate) const FULLTEXT_FIELD: &str = ":fulltext";

/// Oak's `FieldNames.createFullTextFieldName` prefix, which a
/// `relativeNode` aggregate include writes under instead.
pub(crate) const RELATIVE_NODE_FIELD_PREFIX: &str = "fullnode:";

/// One parsed enumeration, keyed by document path rather than by Lucene's
/// document number.
pub(crate) struct Enumeration {
    /// `numdocs`, the live document count.
    documents: u64,
    /// Every document's path, in the order this module canonicalizes them.
    paths: Vec<String>,
    /// One line per field, by field name.
    fields: BTreeMap<String, String>,
    /// Every term's postings, by field and term.
    terms: BTreeMap<(String, String), Term>,
    /// Every stored value, by path, in the order the document carries them.
    stored: BTreeMap<String, Vec<StoredValue>>,
    /// Every doc value, by field and path.
    doc_values: BTreeMap<(String, String), String>,
    /// Every norm, by field and path.
    norms: BTreeMap<(String, String), String>,
}

/// One term's postings, by document path.
struct Term {
    postings: BTreeMap<String, Posting>,
}

/// One posting: the frequency and the positions-with-offsets that follow it.
struct Posting {
    frequency: u64,
    positions: String,
}

/// One stored value: the field it is on and its rendered value.
struct StoredValue {
    field: String,
    rendered: String,
}

/// What the binary exclusion removes: a document's path and the field Oak
/// added that binary's text to.
pub(crate) type BinaryExclusion = BTreeSet<(String, String)>;

impl Enumeration {
    /// Parses one `enumerate` output.
    ///
    /// # Panics
    ///
    /// When a line is not one this module knows, or when a document carries
    /// no `:path`: either means the judge's output changed shape, and a
    /// parser that silently skipped such a line would compare two subsets
    /// and call them equal.
    pub(crate) fn parse(text: &str) -> Self {
        let paths_by_document = document_paths(text);
        let mut documents = 0;
        let mut fields = BTreeMap::new();
        let mut terms: BTreeMap<(String, String), Term> = BTreeMap::new();
        let mut stored: BTreeMap<String, Vec<StoredValue>> = BTreeMap::new();
        let mut doc_values = BTreeMap::new();
        let mut norms = BTreeMap::new();
        let path_of = |number: &str| -> String {
            paths_by_document
                .get(number)
                .unwrap_or_else(|| panic!("document {number} carries no {PATH_FIELD}"))
                .clone()
        };

        for line in text.lines().filter(|line| !line.is_empty()) {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts[0] {
                // The commit file's counter is the number of segments the
                // writer has created, which says how many flushes and
                // merges produced the index rather than what is in it. Oak
                // rebuilds through its own writer under its own merge
                // policy; froe writes one segment in one commit. The count
                // of live documents below is the comparable claim.
                "counter" => {}
                "numdocs" => documents = parts[1].parse().expect("numdocs is a number"),
                "field" => {
                    fields.insert(parts[1].to_owned(), line.to_owned());
                }
                "term" => {
                    terms.insert(
                        (parts[1].to_owned(), parts[2].to_owned()),
                        Term {
                            postings: BTreeMap::new(),
                        },
                    );
                }
                "posting" => {
                    let term = terms
                        .get_mut(&(parts[1].to_owned(), parts[2].to_owned()))
                        .expect("a posting follows its own term line");
                    term.postings.insert(
                        path_of(parts[3]),
                        Posting {
                            frequency: parts[4].parse().expect("a frequency is a number"),
                            positions: parts[5..].join("\t"),
                        },
                    );
                }
                "stored" => {
                    stored
                        .entry(path_of(parts[1]))
                        .or_default()
                        .push(StoredValue {
                            field: parts[2].to_owned(),
                            rendered: parts[3..].join("\t"),
                        });
                }
                "docvalue" => {
                    doc_values.insert(
                        (parts[1].to_owned(), path_of(parts[2])),
                        parts[3..].join("\t"),
                    );
                }
                "norm" => {
                    norms.insert(
                        (parts[1].to_owned(), path_of(parts[2])),
                        parts[3..].join("\t"),
                    );
                }
                other => panic!("the judge's enumeration carries an unknown line kind {other:?}"),
            }
        }

        let mut paths: Vec<String> = paths_by_document.into_values().collect();
        paths.sort();
        Self {
            documents,
            paths,
            fields,
            terms,
            stored,
            doc_values,
            norms,
        }
    }

    /// The live document count the judge reported.
    pub(crate) const fn document_count(&self) -> u64 {
        self.documents
    }

    /// Every document's path.
    pub(crate) fn document_paths(&self) -> &[String] {
        &self.paths
    }

    /// The documents whose `:fulltext` — or `fullnode:<path>` — carries
    /// froe's extraction-error marker, paired with that field.
    ///
    /// This is froe's own declaration of where a binary went, read out of
    /// the index it wrote rather than assumed from the content: the marker
    /// is the one term `--binary-text marker` adds and nothing else in the
    /// fixture contains it.
    pub(crate) fn documents_carrying_the_marker(&self, marker_term: &str) -> BinaryExclusion {
        let mut affected = BinaryExclusion::new();
        for ((field, term), postings) in &self.terms {
            let fulltext = field == FULLTEXT_FIELD || field.starts_with(RELATIVE_NODE_FIELD_PREFIX);
            if !fulltext || term != marker_term {
                continue;
            }
            for path in postings.postings.keys() {
                affected.insert((path.clone(), field.clone()));
            }
        }
        affected
    }

    /// Removes the declared binary difference from this enumeration.
    ///
    /// Posting level, and before any statistic is derived: the postings go,
    /// then the stored value and the norm of the same field on the same
    /// document, then every term the removal left with no postings, and
    /// each surviving term's frequencies are recomputed from what remains
    /// rather than carried over.
    pub(crate) fn exclude_binary_text(&mut self, excluded: &BinaryExclusion) {
        for (path, field) in excluded {
            for ((term_field, _), term) in &mut self.terms {
                if term_field == field {
                    term.postings.remove(path);
                }
            }
            if let Some(values) = self.stored.get_mut(path) {
                values.retain(|value| &value.field != field);
            }
            self.norms.remove(&(field.clone(), path.clone()));
            self.doc_values.remove(&(field.clone(), path.clone()));
        }
        self.terms.retain(|_, term| !term.postings.is_empty());
    }

    /// The canonical rendering: one line per fact, ordered by what the fact
    /// is about and never by what order a writer happened to add it in.
    pub(crate) fn render(&self) -> Vec<String> {
        let ordinal: BTreeMap<&str, usize> = self
            .paths
            .iter()
            .enumerate()
            .map(|(at, path)| (path.as_str(), at))
            .collect();
        let at = |path: &str| -> usize {
            *ordinal
                .get(path)
                .unwrap_or_else(|| panic!("{path} is not one of the enumerated documents"))
        };
        let mut lines = vec![format!("numdocs\t{}", self.documents)];
        lines.extend(self.fields.values().cloned());
        for ((field, term), postings) in &self.terms {
            let frequency: u64 = postings
                .postings
                .values()
                .map(|posting| posting.frequency)
                .sum();
            lines.push(format!(
                "term\t{field}\t{term}\t{}\t{frequency}",
                postings.postings.len()
            ));
            for (path, posting) in &postings.postings {
                lines.push(format!(
                    "posting\t{field}\t{term}\t{:08}\t{}\t{}",
                    at(path),
                    posting.frequency,
                    posting.positions
                ));
            }
        }
        for (path, values) in &self.stored {
            for value in values {
                lines.push(format!(
                    "stored\t{:08}\t{}\t{}",
                    at(path),
                    value.field,
                    value.rendered
                ));
            }
        }
        for ((field, path), rendered) in &self.doc_values {
            lines.push(format!("docvalue\t{field}\t{:08}\t{rendered}", at(path)));
        }
        for ((field, path), rendered) in &self.norms {
            lines.push(format!("norm\t{field}\t{:08}\t{rendered}", at(path)));
        }
        lines
    }
}

/// Every document number's `:path`, read from the stored values.
fn document_paths(text: &str) -> BTreeMap<String, String> {
    let mut paths = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.first() != Some(&"stored") || parts.get(2) != Some(&PATH_FIELD) {
            continue;
        }
        let path = parts[4..].join("\t");
        let previous = paths.insert(parts[1].to_owned(), path.clone());
        assert!(
            previous.is_none() || previous.as_deref() == Some(path.as_str()),
            "document {} carries two different {PATH_FIELD} values",
            parts[1]
        );
    }
    assert!(
        !paths.is_empty(),
        "the enumeration carries no {PATH_FIELD} at all, so re-keying it by document would \
         compare nothing"
    );
    paths
}

/// Compares two canonical renderings, naming the first difference.
///
/// Returns the failure rather than asserting it, so the phase's own
/// negative control can prove the comparison refuses what it must.
pub(crate) fn compare(
    left_name: &str,
    left: &Enumeration,
    right_name: &str,
    right: &Enumeration,
) -> Result<usize, String> {
    let left_paths: BTreeSet<&String> = left.document_paths().iter().collect();
    let right_paths: BTreeSet<&String> = right.document_paths().iter().collect();
    if left_paths != right_paths {
        let only_left: Vec<&&String> = left_paths.difference(&right_paths).take(5).collect();
        let only_right: Vec<&&String> = right_paths.difference(&left_paths).take(5).collect();
        return Err(format!(
            "the two indexes cover different documents: {} only in {left_name} (e.g. \
             {only_left:?}), {} only in {right_name} (e.g. {only_right:?})",
            left_paths.difference(&right_paths).count(),
            right_paths.difference(&left_paths).count(),
        ));
    }
    if left.document_count() != right.document_count() {
        return Err(format!(
            "{left_name} holds {} live documents and {right_name} {}",
            left.document_count(),
            right.document_count()
        ));
    }
    let one = left.render();
    let other = right.render();
    for (at, (first, second)) in one.iter().zip(&other).enumerate() {
        if first != second {
            return Err(format!(
                "line {}:\n    {left_name}: {first}\n    {right_name}: {second}",
                at + 1
            ));
        }
    }
    if one.len() != other.len() {
        let longer = if one.len() > other.len() {
            &one
        } else {
            &other
        };
        return Err(format!(
            "{left_name} renders {} lines and {right_name} {}; the first unmatched line is \
             {:?}",
            one.len(),
            other.len(),
            longer[one.len().min(other.len())]
        ));
    }
    Ok(one.len())
}
