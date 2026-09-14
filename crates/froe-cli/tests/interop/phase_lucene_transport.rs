//! The `lucene_dump` phase: froe's dump against Oak's own dumper.
//!
//! This is the strongest question a read-only transport can be asked. Not
//! "the files froe wrote are a coherent Lucene index", which a
//! self-consistent mistake satisfies, but **"are the bytes froe reads out
//! of `:data` the bytes Oak reads out of `:data`?"** — the same store, the
//! same definition, two independent readers, byte for byte.
//!
//! Oak's own `LuceneIndexDumper` is the oracle, and `IndexConsistencyChecker`
//! at its full level is the second: it copies the index out and runs
//! Lucene's own `CheckIndex` over it, which is the verdict froe cannot
//! produce and the one that says a directory is a real Lucene index.

use super::*;

/// Level 2 of oak-run's `--index-consistency-check`: the blob pass plus
/// Lucene's own index checker over a local copy.
const FULL_CONSISTENCY_LEVEL: &str = "2";

/// Every `lucene` definition in the fixture, discovered rather than listed.
///
/// A hardcoded name would quietly stop covering a definition the fixture
/// gained — and plan 0010 adds one — so the comparison is per definition
/// directory rather than one against one.
fn lucene_definitions(store: &Path) -> Vec<String> {
    let repository = froe::Repository::open(store).expect("open the fixture");
    let oak_index = repository
        .node_at_path("/oak:index")
        .expect("resolve /oak:index")
        .expect("the fixture has an /oak:index");
    let mut paths: Vec<String> = oak_index
        .child_node_entries()
        .expect("read /oak:index")
        .into_iter()
        .filter(|(name, _)| !name.starts_with(':'))
        .filter_map(|(name, node)| {
            let path = format!("/oak:index/{name}");
            froe::index::IndexDefinition::read(&node, &path)
                .ok()
                .filter(|definition| {
                    definition.index_type == Some(froe::index::IndexType::Lucene)
                        && node.child_node(":data").is_ok_and(|data| data.is_some())
                })
                .map(|_| path)
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "the fixture carries no lucene definition with :data, so this phase would compare nothing"
    );
    paths
}

/// Phase: froe's dump against Oak's own dumper, per definition.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn lucene_dump() {
    let work = work_root().join("lucene-dump");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    let store = oak_store();
    let before = store_file_snapshot(&store);
    let judge = Judge::compile();
    let definitions = lucene_definitions(&store);
    eprintln!(
        "  comparing {}: {}",
        definitions.len(),
        definitions.join(", ")
    );

    // One froe dump over every selected definition, as an operator runs it.
    let froe_output = work.join("froe");
    froe(&[
        "index",
        "dump",
        store.to_str().expect("utf-8"),
        "--output",
        froe_output.to_str().expect("utf-8"),
    ]);

    for (ordinal, index_path) in definitions.iter().enumerate() {
        compare_one_definition(judge, &store, &work, &froe_output, index_path, ordinal);
    }

    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a dump must not write a byte inside the store"
    );

    assert_the_comparison_catches_a_corrupt_dump(&work, &froe_output, &definitions[0]);
    eprintln!("  lucene_dump phase passed");
}

/// The negative control: the byte comparison has to *fail* on a difference.
///
/// A comparison that passed over everything would pass over this phase too,
/// and nobody would know. One byte is flipped in the middle of a copy of
/// froe's dump, and the same helper the phase uses must refuse it by name.
fn assert_the_comparison_catches_a_corrupt_dump(work: &Path, froe_output: &Path, index_path: &str) {
    eprintln!("  negative control: one corrupted byte must be named");
    let original = froe_dump_directory(froe_output, index_path).join("data");
    let corrupt = work.join("corrupt");
    copy_directory(&original, &corrupt);

    // The compound file, because it is the one every real index has and the
    // one large enough that a flipped byte is unambiguous damage.
    let target = corrupt.join("_0.cfs");
    corrupt_one_byte(&target);

    // The panic is expected, so its default report would read in the log
    // like the phase failing. Silence the hook across the one call and put
    // it back, whichever way that call goes.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(|| assert_same_files(&original, &corrupt));
    std::panic::set_hook(hook);
    let message = outcome
        .expect_err("the comparison accepted a corrupted dump")
        .downcast::<String>()
        .expect("the comparison's refusal is a formatted message");
    assert!(
        message.contains("_0.cfs") && message.contains("first difference at byte"),
        "the refusal names the file and where it differs: {message}"
    );
}

/// One definition: Oak's dump, froe's dump, and every oracle over both.
fn compare_one_definition(
    judge: &Judge,
    store: &Path,
    work: &Path,
    froe_output: &Path,
    index_path: &str,
    ordinal: usize,
) {
    eprintln!("  {index_path}");

    // Oak's own dumper, into a directory of its own so two definitions
    // never share one.
    let oak_work = work.join(format!("oak-{ordinal}"));
    std::fs::create_dir_all(oak_work.join("dump")).expect("create Oak's dump root");
    judge.run(
        "LuceneJudge",
        &["dump", "/store", index_path, "/out/dump", "/out/dump-path"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(&oak_work, "/out"),
        ],
    );
    let oak_directory =
        std::fs::read_to_string(oak_work.join("dump-path")).expect("read the dumped path");
    let oak_directory = host_path_for(&oak_work, oak_directory.trim());

    let froe_directory = froe_dump_directory(froe_output, index_path);

    assert_same_files(&oak_directory.join("data"), &froe_directory.join("data"));
    assert_same_index_details(&oak_directory, &froe_directory, index_path);

    eprintln!("    judge: checkindex over froe's dump");
    judge.run(
        "LuceneJudge",
        &["checkindex", "/out"],
        vec![Mount::read_only(froe_directory.join("data"), "/out")],
    );

    eprintln!("    judge: numdocs over froe's dump");
    let documents = judge
        .run(
            "LuceneJudge",
            &["numdocs", "/out"],
            vec![Mount::read_only(froe_directory.join("data"), "/out")],
        )
        .trim()
        .parse::<u64>()
        .expect("numdocs prints one number on standard output");
    assert_eq!(
        documents,
        froe_document_count(store, index_path),
        "{index_path}: Oak's document count over froe's dump must equal the count froe \
         computes from segments_N and each .si"
    );
    eprintln!("    {documents} documents, agreed");

    assert_full_consistency(judge, store, work, index_path, ordinal);
}

/// Oak's consistency checker at its full level, over the store itself.
///
/// The status has to have been **reached**: the checker runs Lucene's own
/// index checker only once the directory's content came out consistent, so
/// a blob failure would otherwise read as a quiet pass at the level that
/// matters.
fn assert_full_consistency(
    judge: &Judge,
    store: &Path,
    work: &Path,
    index_path: &str,
    ordinal: usize,
) {
    eprintln!("    judge: consistency level 2");
    let checker_work = work.join(format!("consistency-{ordinal}"));
    std::fs::create_dir_all(&checker_work).expect("create the checker's work directory");
    let output = judge.run(
        "Consistency",
        &["/store", index_path, FULL_CONSISTENCY_LEVEL, "/work"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(&checker_work, "/work"),
        ],
    );
    assert!(
        output.contains("clean=true"),
        "{index_path}: Oak's own consistency checker refused the index:\n{output}"
    );
    assert!(
        output.contains("indexCheckStatus=clean"),
        "{index_path}: the full level's index check must have been reached and come out \
         clean — \"not-reached\" means the blob pass stopped it:\n{output}"
    );
}

/// The directory froe's dump wrote for `index_path`, found by its
/// `index-details.txt` rather than by guessing Oak's naming rule.
fn froe_dump_directory(froe_output: &Path, index_path: &str) -> PathBuf {
    let dumps = froe_output.join("index-dumps");
    for entry in std::fs::read_dir(&dumps).expect("read froe's index-dumps") {
        let entry = entry.expect("read an entry");
        let details = entry.path().join("index-details.txt");
        if !details.is_file() {
            continue;
        }
        if read_details(&details).get("indexPath").map(String::as_str) == Some(index_path) {
            return entry.path();
        }
    }
    panic!(
        "froe's dump has no directory for {index_path} under {}",
        dumps.display()
    );
}

/// Every file in both directories, by name and by bytes.
fn assert_same_files(oak: &Path, froe_files: &Path) {
    let oak_files = files_in(oak);
    let ours = files_in(froe_files);

    let oak_names: Vec<&String> = oak_files.keys().collect();
    let our_names: Vec<&String> = ours.keys().collect();
    assert_eq!(
        our_names, oak_names,
        "froe's dump and Oak's dump hold different file sets"
    );

    for (name, oak_bytes) in &oak_files {
        let our_bytes = &ours[name];
        if our_bytes == oak_bytes {
            continue;
        }
        // Never print megabytes into an assertion: name the file, the
        // lengths, and the first byte that differs.
        let first = our_bytes
            .iter()
            .zip(oak_bytes.iter())
            .position(|(ours, theirs)| ours != theirs);
        panic!(
            "{name} differs: froe wrote {} bytes, Oak wrote {}{}",
            our_bytes.len(),
            oak_bytes.len(),
            match first {
                Some(offset) => format!(
                    "; first difference at byte {offset}: {:#04x} against {:#04x}",
                    our_bytes[offset], oak_bytes[offset]
                ),
                None => String::new(),
            }
        );
    }
}

/// `index-details.txt` must carry the same `indexPath` and the same
/// directory mappings — that file is how oak-run's importer finds both.
fn assert_same_index_details(oak: &Path, froe_directory: &Path, index_path: &str) {
    let oak_details = read_details(&oak.join("index-details.txt"));
    let our_details = read_details(&froe_directory.join("index-details.txt"));
    assert_eq!(
        our_details.get("indexPath").map(String::as_str),
        Some(index_path),
        "froe's index-details.txt names the wrong indexPath"
    );
    let mappings = |details: &std::collections::BTreeMap<String, String>| {
        details
            .iter()
            .filter(|(key, _)| key.starts_with("dir."))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        mappings(&our_details),
        mappings(&oak_details),
        "froe's index-details.txt maps different directories than Oak's"
    );
}

/// A Java properties file, as `index-details.txt` is.
///
/// Both sides write it through `java.util.Properties`, whose `saveConvert`
/// escapes `=`, `:`, `#` and `!` in **values** as well as in keys — so
/// `indexPath` lands as `/oak\:index/lucene`. Comparing the raw lines
/// would compare an encoding rather than the facts, and would silently
/// pass if one side stopped escaping.
fn read_details(path: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| split_unescaped(line))
        .map(|(key, value)| (unescape_properties(&key), unescape_properties(value.trim())))
        .collect()
}

/// Splits at the first `=` that is not itself escaped.
fn split_unescaped(line: &str) -> Option<(String, &str)> {
    let bytes = line.as_bytes();
    let mut escaped = false;
    for (offset, byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'=' => return Some((line[..offset].trim().to_owned(), &line[offset + 1..])),
            _ => {}
        }
    }
    None
}

/// `Properties.loadConvert`, for the escapes `saveConvert` writes.
fn unescape_properties(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let digits: String = characters.by_ref().take(4).collect();
                let code = u32::from_str_radix(&digits, 16)
                    .unwrap_or_else(|_| panic!("{digits} is not a \\u escape"));
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Every file of a directory with its bytes.
fn files_in(directory: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| {
            let entry = entry.expect("read an entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read a file"),
            )
        })
        .collect()
}

/// The document count froe computes from `segments_N` and each `.si`, as
/// Oak's own document count over a directory composes it.
///
/// `:status/indexedNodes` is deliberately not used: it is a per-cycle
/// counter, not a document count.
fn froe_document_count(store: &Path, index_path: &str) -> u64 {
    let repository = froe::Repository::open(store).expect("open the fixture");
    let node = repository
        .node_at_path(index_path)
        .expect("resolve the definition")
        .expect("the definition exists");
    let definition =
        froe::index::IndexDefinition::read(&node, index_path).expect("model the definition");
    let directory =
        froe::index::lucene::OakDirectory::open(&repository, &node, &definition, ":data")
            .expect("open :data")
            .expect(":data exists");
    let report =
        froe::index::lucene::check::check_structure(&directory).expect("check the structure");
    assert!(
        report.is_coherent(),
        "{index_path}: froe's own structural check refused the fixture's index: {report:?}"
    );
    u64::try_from(report.live_document_count).expect("a document count is not negative")
}

// ---------------------------------------------------------------------------
// The `lucene_import` phase
// ---------------------------------------------------------------------------

/// The definition this phase moves. Named explicitly because the delta
/// assertion names one subtree, and plan 0010 adds a second definition.
const IMPORTED_DEFINITION: &str = "/oak:index/lucene";

/// The subtree an import is allowed to change, and the only one.
const IMPORTED_SUBTREE: &[&str] = &["/oak:index/lucene"];

/// Properties that are new by design on every import and cannot be
/// compared against the original.
///
/// `uniqueKey` is sixteen fresh bytes per file, `jcr:lastModified` is the
/// moment the file was written, and `:status/uid` is the fresh identifier
/// Oak's own importer leaves. `dirListing` is compared as a set instead of
/// being excluded, because its *contents* are the claim and only its order
/// is Oak's hash-set iteration.
const FRESH_BY_DESIGN: &[&str] = &["uniqueKey", "jcr:lastModified", "uid"];

/// The three `:status` properties an import legitimately does not write.
///
/// A recorded departure from oak-run's importer rather than a difference
/// this phase can assert away: they are per-cycle state, and Oak's own
/// importer leaves none to copy them from.
const PER_CYCLE_STATUS_PROPERTIES: &[&str] =
    &["indexedNodes", "lastUpdated", "reindexCompletionTimestamp"];

/// Phase: froe's import against Oak, in both directions the import exists
/// for.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn lucene_import() {
    let work = work_root().join("lucene-import");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    round_trip_through_froes_own_dump(&work);
    assert_the_refusals(&work);
    eprintln!("  lucene_import phase passed");
}

/// The round trip: dump the fixture's index, lose it, put it back.
///
/// "Lose it" is the state a definition is actually in when this command is
/// reached for — the hidden children gone — rather than a contrived one.
fn round_trip_through_froes_own_dump(work: &Path) {
    eprintln!("  round trip through froe's own dump");
    let source = oak_store();

    let store = work.join("round-trip");
    copy_store(&source, &store);

    let dump = work.join("round-trip-dump");
    froe(&[
        "index",
        "dump",
        store.to_str().expect("utf-8"),
        "--output",
        dump.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
    ]);

    let original = definition_digest(&store);
    let original_count = reindex_count(&store);
    let checkpoints_before = checkpoint_names(&store);
    let baseline = digest_store(&store);

    // The state a lost index leaves: every hidden child gone, the
    // definition still there. `reindexCount` is put back as it was, so the
    // import's own increment lands one above the original.
    remove_hidden_children(
        &store,
        &[BookkeepingReset {
            name: "lucene",
            reindex_count: original_count,
        }],
    );

    froe(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        dump.join("index-dumps").to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
        "--yes",
    ]);

    assert_the_index_is_back(&store, &original, &dump, original_count + 1);
    assert_eq!(
        checkpoint_names(&store),
        checkpoints_before,
        "an import releases no checkpoint"
    );
    assert_digest_delta(
        &baseline,
        &digest_store(&store),
        ExpectedDigestDelta::Subtrees(IMPORTED_SUBTREE),
        "lucene_import round trip",
    );
    froe(&["check", store.to_str().expect("utf-8")]);
    eprintln!("    the index is back, byte for byte");
}

/// The imported definition renders as the original did, allowing for what
/// is fresh by design, and every file reads back byte for byte.
fn assert_the_index_is_back(store: &Path, original: &str, dump: &Path, expected_count: i64) {
    froe(&["index", "check", store.to_str().expect("utf-8")]);

    assert_eq!(
        normalized_definition(&definition_digest(store)),
        normalized_definition(original),
        "the imported definition must render as the original did"
    );
    assert_eq!(
        reindex_count(store),
        expected_count,
        "reindexCount must be the file's value plus one"
    );
    assert_the_files_read_back(store, dump);
    assert_the_status_is_what_the_importer_leaves(store);
}

/// Every file reads back out of the store byte-identical to the file the
/// dump wrote.
///
/// The digest cannot make this claim: a `jcr:data` digest covers the
/// *stored blob*, which is the file followed by the sixteen fresh
/// `uniqueKey` bytes, so two honest imports of one file hash differently.
/// Reading through `OakDirectory` compares the file, which is the claim.
fn assert_the_files_read_back(store: &Path, dump: &Path) {
    use std::io::Read as _;

    let repository = froe::Repository::open(store).expect("open the store");
    let node = repository
        .node_at_path(IMPORTED_DEFINITION)
        .expect("resolve the definition")
        .expect("the definition exists");
    let definition = froe::index::IndexDefinition::read(&node, IMPORTED_DEFINITION)
        .expect("model the definition");
    let directory =
        froe::index::lucene::OakDirectory::open(&repository, &node, &definition, ":data")
            .expect("open :data")
            .expect(":data exists");

    let source = froe_dump_directory(dump, IMPORTED_DEFINITION).join("data");
    let mut names: Vec<String> = directory.file_names().to_vec();
    names.sort();
    let mut on_disk: Vec<String> = std::fs::read_dir(&source)
        .expect("read the dumped directory")
        .map(|entry| {
            entry
                .expect("read an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    assert_eq!(
        names, on_disk,
        "the imported directory holds a different file set than the dump"
    );

    for name in &names {
        let file = directory.file(name).expect("open a file");
        let mut stored = Vec::new();
        file.reader().read_to_end(&mut stored).expect("read a file");
        let original = std::fs::read(source.join(name)).expect("read the dumped file");
        assert_eq!(
            stored.len(),
            original.len(),
            "{name} reads back at a different length than the dump wrote"
        );
        if stored != original {
            let offset = stored
                .iter()
                .zip(original.iter())
                .position(|(ours, theirs)| ours != theirs)
                .expect("the lengths are equal and the contents differ");
            panic!(
                "{name} differs at byte {offset}: {:#04x} against {:#04x}",
                stored[offset], original[offset]
            );
        }
    }
    eprintln!("    {} files read back byte for byte", names.len());
}

/// `:status` carries the fresh `uid` and none of the per-cycle state.
fn assert_the_status_is_what_the_importer_leaves(store: &Path) {
    let status = digest_store(store)
        .lines()
        .find(|line| line.starts_with(&format!("{IMPORTED_DEFINITION}/:status\t")))
        .map(str::to_owned)
        .expect("the imported definition has a :status node");
    assert!(
        status.contains("uid=String:"),
        ":status must carry a fresh uid: {status}"
    );
    for absent in PER_CYCLE_STATUS_PROPERTIES {
        assert!(
            !status.contains(absent),
            ":status must not carry {absent}, which Oak's own importer leaves no state to              copy it from: {status}"
        );
    }
}

/// The digest lines of the imported definition's subtree.
fn definition_digest(store: &Path) -> String {
    digest_store(store)
        .lines()
        .filter(|line| {
            line.strip_prefix(IMPORTED_DEFINITION).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('/') || rest.starts_with('\t')
            })
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The digest with everything an honest import legitimately changes
/// reduced to a placeholder, and `dirListing` reduced to a sorted set.
///
/// `:status` is dropped entirely and asserted on its own, because the
/// three per-cycle properties are a recorded departure rather than a
/// difference that can be normalized field by field.
fn normalized_definition(digest: &str) -> Vec<String> {
    digest
        .lines()
        .filter(|line| !line.starts_with(&format!("{IMPORTED_DEFINITION}/:status\t")))
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let mut kept: Vec<String> = Vec::new();
            for (position, field) in fields.iter().enumerate() {
                if position == 0 {
                    kept.push((*field).to_owned());
                    continue;
                }
                let Some((name, value)) = field.split_once('=') else {
                    kept.push((*field).to_owned());
                    continue;
                };
                if FRESH_BY_DESIGN.contains(&name) {
                    // The property must still be *present*: dropping the
                    // name as well would let an import that wrote none of
                    // them pass.
                    kept.push(format!("{name}=<fresh>"));
                    continue;
                }
                if name == "dirListing" {
                    // Oak stores the listing in a concurrent hash set's
                    // iteration order and reads it back as a set, so only
                    // its contents are a claim. froe writes it in name
                    // order, a deviation recorded at the writer.
                    // The digest renders a property as `<type>:<values>`,
                    // so the type prefix has to come off before the values
                    // are sorted: left on, it pins whichever value happens
                    // to be first and the sort changes nothing.
                    let (property_type, listing) = value
                        .split_once(':')
                        .expect("a digest property value carries its type");
                    let mut names: Vec<&str> = listing.split('\u{1f}').collect();
                    names.sort_unstable();
                    kept.push(format!(
                        "dirListing={property_type}:{}",
                        names.join("\u{1f}")
                    ));
                    continue;
                }
                if name == "reindexCount" {
                    // Asserted exactly, on its own: the import's whole
                    // point is that this value advances.
                    kept.push("reindexCount=<advanced>".to_owned());
                    continue;
                }
                if name == "jcr:data" {
                    // The stored blob is the file followed by the sixteen
                    // fresh `uniqueKey` bytes, so its digest differs on
                    // every honest import. The bytes are compared through
                    // the reader instead.
                    let length = value.split_once('@').map_or(value, |(length, _)| length);
                    kept.push(format!("jcr:data=<blob {length}>"));
                    continue;
                }
                kept.push((*field).to_owned());
            }
            kept.join("\t")
        })
        .collect()
}

/// The definition's `reindexCount`.
fn reindex_count(store: &Path) -> i64 {
    let repository = froe::Repository::open(store).expect("open the store");
    let node = repository
        .node_at_path(IMPORTED_DEFINITION)
        .expect("resolve the definition")
        .expect("the definition exists");
    froe::index::IndexDefinition::read(&node, IMPORTED_DEFINITION)
        .expect("model the definition")
        .reindex
        .count
}

/// The checkpoint names a store holds.
fn checkpoint_names(store: &Path) -> Vec<String> {
    let repository = froe::Repository::open(store).expect("open the store");
    let mut names: Vec<String> = repository
        .checkpoints()
        .expect("read the checkpoints")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
}

/// What the import refuses, against a real store, with the store unchanged
/// in every case.
fn assert_the_refusals(work: &Path) {
    eprintln!("  refusals");

    // A checkpoint that is not the attachment state.
    let (store, input) = store_and_dump(work, "refuse-checkpoint");
    let before = store_file_snapshot(&store);
    std::fs::write(
        input.join("indexer-info.properties"),
        "checkpoint=not-a-checkpoint-this-store-holds\n",
    )
    .expect("rewrite the properties file");
    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--yes",
    ]);
    assert!(
        refusal.contains("does not resolve in this store"),
        "a checkpoint the store does not hold must be refused: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a checkpoint the store does not hold is refused");

    assert_a_drifting_definitions_file_is_refused(work);
    assert_a_synchronous_definition_is_refused(work);
}

/// A definitions file carrying a visible property the store's definition
/// does not.
fn assert_a_drifting_definitions_file_is_refused(work: &Path) {
    let (store, input) = store_and_dump(work, "refuse-drift");
    let before = store_file_snapshot(&store);

    let path = input.join("index-definitions.json");
    let content = std::fs::read_to_string(&path).expect("read the definitions file");
    let opening = format!("\"{IMPORTED_DEFINITION}\": {{");
    let at = content
        .find(&opening)
        .expect("the file carries the definition")
        + opening.len();
    let mut edited = content;
    edited.insert_str(at, "\n    \"evaluatePathRestrictions\": true,");
    std::fs::write(&path, edited).expect("rewrite the definitions file");

    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--yes",
    ]);
    assert!(
        refusal.contains("/evaluatePathRestrictions")
            && refusal.contains("never a definition change"),
        "a drifting definitions file must be refused naming the difference: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a drifting definitions file is refused naming the property");
}

/// A definition made synchronous on the copy: oak-run's own importer never
/// completes that case, so froe refuses it rather than inventing one.
fn assert_a_synchronous_definition_is_refused(work: &Path) {
    let (store, input) = store_and_dump(work, "refuse-synchronous");

    edit_definitions(
        &store,
        &[BookkeepingReset {
            name: "lucene",
            reindex_count: reindex_count(&store),
        }],
        DefinitionEdit {
            remove_async: true,
            ..DefinitionEdit::default()
        },
    );
    let before = store_file_snapshot(&store);

    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
        "--yes",
    ]);
    assert!(
        refusal.contains(IMPORTED_DEFINITION) && refusal.contains("synchronous"),
        "a synchronous definition must be refused by name: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a synchronous definition is refused by name");
}

/// A fresh copy of the fixture with its Lucene index dumped beside it.
fn store_and_dump(work: &Path, name: &str) -> (PathBuf, PathBuf) {
    let store = work.join(name);
    copy_store(&oak_store(), &store);
    let dump = work.join(format!("{name}-dump"));
    froe(&[
        "index",
        "dump",
        store.to_str().expect("utf-8"),
        "--output",
        dump.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
    ]);
    (store, dump.join("index-dumps"))
}
