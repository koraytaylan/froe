//! The `judge_smoke` phase: proving the Oak-side judge itself works before
//! any later phase trusts a verdict from it.
//!
//! Every assertion here is about the *judge*, not about froe. A judge that
//! silently ran against the wrong class path, or whose `checkindex` called
//! everything clean, would make every later comparison meaningless while
//! passing — which is exactly the failure a smoke phase exists to catch.

use super::*;

/// The file set a single-flush `oakCodec` index has, sorted.
const SAMPLE_INDEX_FILES: [&str; 5] = ["_0.cfe", "_0.cfs", "_0.si", "segments.gen", "segments_1"];

/// How many documents `sample-index` writes; `LuceneJudge` agrees.
const SAMPLE_DOCUMENT_COUNT: u64 = 5;

/// Compile the judge and prove each of its verdicts is reachable.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn judge_smoke() {
    let store = oak_store();
    let judge = Judge::compile();
    let work = work_root().join("judge-smoke");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the judge's work directory");

    let dumped = dump_and_check(judge, &store, &work);
    let sample = write_and_check_the_sample(judge, &work);
    refuse_a_corrupt_copy(judge, &sample, &work);
    run_oaks_printers(judge, &store, &work);

    let _ = dumped;
    eprintln!("  judge_smoke: the judge compiles, dumps, checks, samples and refuses");
}

/// Oak's own dumper, then Lucene's own checker over what it produced.
///
/// The directory is one froe has **not** written, which is what makes a
/// clean verdict evidence about the judge rather than about froe. It is also
/// the proof that the class path resolves `oakCodec`: without the codec
/// registration the reader cannot open a segment at all.
fn dump_and_check(judge: &Judge, store: &Path, work: &Path) -> PathBuf {
    eprintln!("  judge: dump /oak:index/lucene");
    std::fs::create_dir_all(work.join("dump")).expect("create the dump root");
    judge.run(
        "LuceneJudge",
        &[
            "dump",
            "/store",
            "/oak:index/lucene",
            "/out/dump",
            "/out/dump-path",
        ],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(work, "/out"),
        ],
    );
    let dumped = std::fs::read_to_string(work.join("dump-path")).expect("read the dumped path");
    let dumped = host_path_for(work, dumped.trim()).join("data");
    assert!(
        dumped.join("segments.gen").exists(),
        "the dump has no segments.gen: {}",
        dumped.display()
    );

    eprintln!("  judge: checkindex over the dump");
    judge.run(
        "LuceneJudge",
        &["checkindex", "/out"],
        vec![Mount::read_only(&dumped, "/out")],
    );

    eprintln!("  judge: numdocs over the dump");
    let documents = judge
        .run(
            "LuceneJudge",
            &["numdocs", "/out"],
            vec![Mount::read_only(&dumped, "/out")],
        )
        .trim()
        .parse::<u64>()
        .expect("numdocs prints one number on standard output");
    assert!(
        documents > 0,
        "the fixture's Lucene index holds no documents, so a later comparison \
         against a document count would compare nothing"
    );
    eprintln!("    {documents} documents");
    dumped
}

/// A sample index written by Lucene's own writer under the configuration Oak
/// builds for `codec = oakCodec`. Small enough to commit, unlike the
/// fixture's megabyte-sized compound file.
fn write_and_check_the_sample(judge: &Judge, work: &Path) -> PathBuf {
    eprintln!("  judge: sample-index");
    let sample = work.join("sample");
    std::fs::create_dir_all(&sample).expect("create the sample directory");
    judge.run(
        "LuceneJudge",
        &["sample-index", "/out"],
        vec![Mount::writable(&sample, "/out")],
    );
    let mut written: Vec<String> = std::fs::read_dir(&sample)
        .expect("read the sample directory")
        .map(|entry| {
            entry
                .expect("read a sample entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    written.sort();
    assert_eq!(
        written, SAMPLE_INDEX_FILES,
        "a single flush with no merge produces exactly this file set; anything \
         else means the writer merged or did not use a compound file"
    );
    let segments = std::fs::read(sample.join("segments_1")).expect("read segments_1");
    assert!(
        contains_bytes(&segments, b"oakCodec"),
        "segments_1 does not name oakCodec, so the sample exercises a codec \
         froe will never meet"
    );
    judge.run(
        "LuceneJudge",
        &["checkindex", "/out"],
        vec![Mount::read_only(&sample, "/out")],
    );
    let documents = judge
        .run(
            "LuceneJudge",
            &["numdocs", "/out"],
            vec![Mount::read_only(&sample, "/out")],
        )
        .trim()
        .parse::<u64>()
        .expect("numdocs prints one number");
    assert_eq!(
        documents, SAMPLE_DOCUMENT_COUNT,
        "the sample's document count"
    );
    sample
}

/// One byte of `segments_1` overwritten must make the checker exit non-zero.
///
/// Without this the clean verdicts above would prove nothing, because a
/// checker that always passes also passes.
fn refuse_a_corrupt_copy(judge: &Judge, sample: &Path, work: &Path) {
    eprintln!("  judge: checkindex refuses a corrupt directory");
    let corrupt = work.join("corrupt");
    copy_directory(sample, &corrupt);
    corrupt_one_byte(&corrupt.join("segments_1"));
    let refusal = judge.run_failure(
        "LuceneJudge",
        &["checkindex", "/out"],
        vec![Mount::read_only(&corrupt, "/out")],
    );
    assert!(
        refusal.contains("not clean"),
        "the refusal must say what it refused:\n{refusal}"
    );
}

/// Oak's two index printers, which are the oracles the `index_inventory`
/// phase compares froe against.
///
/// Running them here means a broken printer invocation fails in the phase
/// that is about the judge rather than in the one that is about froe.
fn run_oaks_printers(judge: &Judge, store: &Path, work: &Path) {
    eprintln!("  judge: Oak's definition and index printers");
    judge.run(
        "IndexJudge",
        &["definitions", "/store", "/out/definitions.json"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(work, "/out"),
        ],
    );
    let definitions =
        std::fs::read_to_string(work.join("definitions.json")).expect("read the definitions");
    assert!(
        definitions.starts_with('{') && definitions.ends_with('}'),
        "Oak's printer ends at the closing brace with no trailing newline:\n{definitions}"
    );
    assert!(
        definitions.contains("/oak:index/lucene"),
        "the printer listed no lucene definition:\n{definitions}"
    );

    judge.run(
        "IndexJudge",
        &["info", "/store", "/out/info.json", "/tmp/judge-work"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(work, "/out"),
        ],
    );
    let info = std::fs::read_to_string(work.join("info.json")).expect("read the index info");
    assert!(
        info.contains("\"Total number of indexes\""),
        "Oak's index printer produced no inventory:\n{info}"
    );
    assert!(
        info.contains("\"Async Indexers State\""),
        "the async lane service was not bound:\n{info}"
    );
}

/// Maps a path the container printed back to its host counterpart.
///
/// The judge writes absolute container paths, because that is what Oak's own
/// API hands it; the mount is the only thing that knows where `/out` is.
pub(crate) fn host_path_for(host_mount: &Path, container_path: &str) -> PathBuf {
    let relative = container_path
        .strip_prefix("/out/")
        .unwrap_or_else(|| panic!("the judge reported {container_path}, which is not under /out"));
    host_mount.join(relative)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// A shallow copy: a Lucene directory has no subdirectories.
pub(crate) fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("create the copy's directory");
    for entry in std::fs::read_dir(source).expect("read the directory to copy") {
        let entry = entry.expect("read an entry");
        std::fs::copy(entry.path(), target.join(entry.file_name())).expect("copy a file");
    }
}

/// Flips one byte in the middle of a file, which is damage no reader can
/// mistake for a legal state.
pub(crate) fn corrupt_one_byte(path: &Path) {
    let mut bytes = std::fs::read(path).expect("read the file to corrupt");
    assert!(
        bytes.len() > 16,
        "{} is too short to corrupt meaningfully",
        path.display()
    );
    let position = bytes.len() / 2;
    bytes[position] = !bytes[position];
    std::fs::write(path, bytes).expect("write the corrupted file");
}
