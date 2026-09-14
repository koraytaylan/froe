//! The `index_inventory` phase: froe's reading of index structures against
//! Oak's own, over a store Oak wrote.
//!
//! Its oracle is the judge running Oak's printers over the same bytes. That
//! is the whole point of the phase: a test that compared froe against froe
//! would pass on a reader that is wrong in a self-consistent way, which is
//! exactly how a format port fails.

use super::*;

/// Fields Oak's index printer emits that froe deliberately does not model.
///
/// `Is active` is constantly true over a store without non-default mounts —
/// every index path the service yields is active there — so a comparison
/// including it would assert a constant. This plan reports mounts rather
/// than modelling them.
const UNMODELLED_OAK_FIELDS: [&str; 1] = ["Is active"];

/// Phase: froe's index inventory against Oak's own printers.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn index_inventory() {
    let store = oak_store();
    let judge = Judge::compile();
    let work = work_root().join("index-inventory");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    // This phase must run before `commit`. froe's direct commits run none of
    // Oak's index editors, so a fixture froe has already written to is
    // legitimately short an entry in the synchronous `jcr:title` index — and
    // the comparison against Oak would then report a real difference that
    // means nothing about either side's reading. The marker `commit` leaves
    // is the assertion: it is cheap, and it fails with the reason rather
    // than with a field mismatch three steps later.
    let written = froe(&[
        "tree",
        store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "1",
    ]);
    assert!(
        !written.contains("froe-written"),
        "the `commit` phase has already written to this fixture, so froe's own \
         unindexed nodes would be compared against Oak's index as though they \
         were a reading difference; run index_inventory before commit:\n{written}"
    );

    // Read-only from here: the snapshot is taken before anything runs and
    // compared after, which is the first read-only phase to assert it.
    let before = store_file_snapshot(&store);

    assert_definitions_match_oaks_printer(judge, &store, &work);
    assert_list_agrees_with_oaks_index_printer(judge, &store, &work);
    assert_check_passes_and_names_a_forged_defect(&store, &work);
    let documents = assert_lucene_dumps_are_valid(judge, &store, &work);
    settle_the_unindexed_node_question(judge, &store, &work);

    assert_eq!(
        store_file_snapshot(&store),
        before,
        "the index_inventory phase modified the store it was reading"
    );
    std::fs::write(
        work_root().join("index-lucene-numdocs.txt"),
        documents.to_string(),
    )
    .expect("record the Lucene document count for the run record");

    eprintln!("  index_inventory phase passed");
}

/// Step 1: `froe index definitions` must be byte-identical to Oak's printer.
///
/// Normalized only for trailing whitespace. Everything else — the key order,
/// the type codes, the pretty-printing, the hidden properties such as the
/// `lucene` definition's `:version` — must agree exactly, because this file
/// is what Oak's own definition updater consumes and applying it replaces
/// the whole definition node.
fn assert_definitions_match_oaks_printer(judge: &Judge, store: &Path, work: &Path) {
    eprintln!("  index definitions: byte comparison against Oak's printer");
    judge.run(
        "IndexJudge",
        &["definitions", "/store", "/out/definitions.json"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(work, "/out"),
        ],
    );
    let oaks =
        std::fs::read_to_string(work.join("definitions.json")).expect("read Oak's definitions");
    let froes = froe(&["index", "definitions", store.to_str().unwrap(), "--silent"]);

    let oaks = oaks.trim_end();
    let froes = froes.trim_end();
    if oaks != froes {
        let difference = oaks
            .lines()
            .zip(froes.lines())
            .enumerate()
            .find(|(_, (oak, froe))| oak != froe);
        let detail = match difference {
            Some((line, (oak, froe))) => {
                format!("line {}:\n  Oak:  {oak}\n  froe: {froe}", line + 1)
            }
            None => format!(
                "the first {} lines agree; Oak has {} lines and froe {}",
                oaks.lines().count().min(froes.lines().count()),
                oaks.lines().count(),
                froes.lines().count()
            ),
        };
        panic!("froe's definitions differ from Oak's printer — {detail}");
    }
    eprintln!("    {} bytes identical", froes.len());
}

/// Step 2: `froe index list` must agree with Oak's index printer on every
/// field both compute, over the definitions both list.
///
/// Over the **intersection** of paths, deliberately. Oak's information
/// service omits `disabled` and untyped definitions and any index whose
/// provider throws — a Lucene definition whose lane has no `/:async` entry
/// is the case that throws — so the sets need not match. The phase asserts
/// the fixture holds none of those, which makes the intersection the whole
/// of Oak's set and keeps the comparison from quietly shrinking.
fn assert_list_agrees_with_oaks_index_printer(judge: &Judge, store: &Path, work: &Path) {
    eprintln!("  index list: field comparison against Oak's index printer");
    judge.run(
        "IndexJudge",
        &["info", "/store", "/out/info.json", "/tmp/judge-work"],
        vec![
            Mount::read_only(store, "/store"),
            Mount::writable(work, "/out"),
        ],
    );
    let oaks = std::fs::read_to_string(work.join("info.json")).expect("read Oak's index info");
    let froes = froe(&["index", "list", store.to_str().unwrap(), "--silent"]);
    let froes = parse_froe_index_list(&froes);

    let oak_paths = oak_index_paths(&oaks);
    assert!(
        !oak_paths.is_empty(),
        "Oak's index printer listed nothing, so the comparison would be vacuous:\n{oaks}"
    );
    // The fixture must hold none of the definitions Oak omits, or the
    // difference of the two sets would excuse a real disagreement.
    let missing_from_froe: Vec<&String> = oak_paths
        .iter()
        .filter(|path| !froes.contains_key(*path))
        .collect();
    assert!(
        missing_from_froe.is_empty(),
        "Oak lists definitions froe does not: {missing_from_froe:?}"
    );
    let extra: Vec<&String> = froes
        .keys()
        .filter(|path| !oak_paths.contains(*path))
        .collect();
    assert!(
        extra.is_empty(),
        "froe lists definitions Oak does not, so the fixture holds a disabled, \
         untyped or throwing definition this comparison was asserted not to \
         have: {extra:?}"
    );

    for path in &oak_paths {
        let oak_fields = oak_index_fields(&oaks, path);
        let froe_fields = &froes[path];
        for (field, oak_value) in &oak_fields {
            if UNMODELLED_OAK_FIELDS.contains(&field.as_str()) {
                continue;
            }
            compare_one_field(path, field, oak_value, froe_fields);
        }
    }
    eprintln!("    {} definitions agree field by field", oak_paths.len());
}

/// One of Oak's fields against froe's rendering of the same fact.
fn compare_one_field(
    path: &str,
    field: &str,
    oak_value: &str,
    froe_fields: &std::collections::BTreeMap<String, String>,
) {
    let froe_value = |name: &str| {
        froe_fields
            .get(name)
            .unwrap_or_else(|| panic!("froe's listing of {path} has no {name:?}: {froe_fields:?}"))
            .clone()
    };
    match field {
        "Type" => assert_eq!(oak_value, froe_value("type"), "{path}: type"),
        "Async lane name" => assert_eq!(oak_value, froe_value("lane"), "{path}: lane"),
        // Oak formats to whole seconds; froe renders the stored form, which
        // carries milliseconds. Comparing the second is comparing the fact.
        "Last indexed up to" | "Last updated time" | "Reindex completion time" => {
            let froe_field = match field {
                "Last indexed up to" => "indexed up to",
                _ => return,
            };
            let stored = froe_value(froe_field);
            assert_eq!(
                oak_value.get(..19),
                stored.get(..19),
                "{path}: {field} — Oak {oak_value}, froe {stored}"
            );
        }
        "Size (in bytes)" => assert_eq!(
            oak_value,
            byte_count(&froe_value("size")),
            "{path}: size in bytes"
        ),
        // froe omits a size it has no child for, where Oak computes zero.
        // Both mean "no suggester"; comparing them as equal is comparing the
        // fact rather than the rendering.
        "Suggest size (in bytes)" => {
            let froes = froe_fields
                .get("suggest size")
                .map_or_else(|| "0".to_owned(), |rendered| byte_count(rendered));
            assert_eq!(oak_value, froes, "{path}: suggest size in bytes");
        }
        "Has hidden oak mount" => {
            assert_eq!(
                oak_value,
                froe_value("hidden mount"),
                "{path}: hidden mount"
            );
        }
        "Has property index" => assert_eq!(
            oak_value,
            froe_value("property index"),
            "{path}: property index"
        ),
        // Task 0802 gave froe the commit-file reader this number needs, so
        // a Lucene definition is compared the same way every other type is.
        // Oak's own count over the directory is documents minus deletions,
        // which is what froe's structural check computes.
        "Estimated entry count" => assert_eq!(
            oak_value,
            froe_value("estimated entries").replace(',', ""),
            "{path}: estimated entry count"
        ),
        // Oak emits these two only for Lucene and froe reports them
        // elsewhere; neither is a field both compute.
        "Async" | "Size" | "Suggest size" | "Index size" => {}
        other => panic!("{path}: Oak emits {other:?}, which this phase does not compare"),
    }
}

/// Step 3: the check passes on the pristine store and names a forged defect.
fn assert_check_passes_and_names_a_forged_defect(store: &Path, work: &Path) {
    eprintln!("  index check: pristine store");
    let report = froe(&["index", "check", store.to_str().unwrap(), "--silent"]);
    assert!(
        !report.contains("INCONSISTENT"),
        "the pristine Oak store must check clean:\n{report}"
    );

    eprintln!("  index check: names a forged defect");
    let forged = work.join("forged-store");
    copy_store(store, &forged);
    let removed = remove_the_reference_target(&forged);

    let output = std::process::Command::new(froe_bin())
        .args(["index", "check", forged.to_str().unwrap(), "--silent"])
        .output()
        .expect("run the check over the forged store");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "an inconsistent index exits 3:\n{stdout}{stderr}"
    );
    assert!(
        stdout.contains(&removed),
        "the verdict does not name the path the entry points at ({removed}):\n{stdout}"
    );
    assert!(
        stdout.contains("stale"),
        "an entry naming a node that no longer exists is stale:\n{stdout}"
    );
    eprintln!("    a stale entry at {removed} is reported, exit 3");
}

/// Removes `/content/interop/references/target` from a store copy, leaving
/// every index entry that names it pointing at nothing.
///
/// The reference target rather than an arbitrary node, because task 0616 put
/// it there precisely so several indexes name it: `uuid` (unique),
/// `nodetype`, `reference` and the rebuilt `interopTitle`. One removal
/// therefore forges a stale entry in more than one storage shape at once.
///
/// Returns the path that no longer resolves.
fn remove_the_reference_target(store: &Path) -> String {
    const REMOVED: &str = "/content/interop/references/target";
    let writable = WritableRepository::open(store).expect("open the forged store for writing");
    let generation = writable.writing_generation().expect("writing generation");
    let head = writable.head();
    {
        let mut writer = writable.record_writer(generation);
        let spine = resolve_spine(&writable, head, REMOVED);
        let mut identifier = None;
        // Rebuilt from the leaf upward: each ancestor is rewritten to point
        // at the rewritten child, and the leaf's own edit removes it.
        for (node, name) in spine.iter().rev() {
            let mut edits = froe::writer::commit::ChildEdits::new();
            edits.insert((*name).to_owned(), identifier);
            identifier = Some(
                rewrite_node_with_child_edits(&writable, &mut writer, Some(*node), &edits)
                    .expect("rewrite an ancestor of the removed node"),
            );
        }
        writer.finish().expect("finish the forging writer");
        assert!(
            writable.compare_and_set_head(head, identifier.expect("a rewritten super-root")),
            "advance the forged store's head"
        );
    }
    writable.close().expect("close the forged store");
    REMOVED.to_owned()
}

/// The chain from the super-root down to `path`'s parent, each with the name
/// of the child to replace.
fn resolve_spine(
    writable: &WritableRepository,
    head: RecordIdentifier,
    path: &str,
) -> Vec<(RecordIdentifier, &'static str)> {
    // `/content/interop/references/target` and nothing else, so the element
    // names are known at compile time and a typo is a compile error rather
    // than an empty spine.
    const ELEMENTS: [&str; 5] = ["root", "content", "interop", "references", "target"];
    assert_eq!(path, "/content/interop/references/target");
    let mut spine = Vec::with_capacity(ELEMENTS.len());
    let mut node = head;
    for element in ELEMENTS {
        spine.push((node, element));
        node = froe::content::node::NodeState::new(writable, node)
            .child_node(element)
            .expect("read a child on the way down")
            .unwrap_or_else(|| panic!("{element} is missing on the way to {path}"))
            .record_identifier();
    }
    spine
}

/// Step 4: every Lucene directory Oak dumps is a valid Lucene index.
///
/// This pins that froe's readers were pointed at real index data rather than
/// at something that merely parsed. The document count is recorded rather
/// than compared with `:status/indexedNodes`, which is a per-cycle counter
/// Oak resets on every indexing cycle and not a document count.
fn assert_lucene_dumps_are_valid(judge: &Judge, store: &Path, work: &Path) -> u64 {
    eprintln!("  lucene: Oak dumps, Lucene's own checker verifies");
    let dump_root = work.join("dump");
    std::fs::create_dir_all(&dump_root).expect("create the dump root");
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
    let relative = dumped
        .trim()
        .strip_prefix("/out/")
        .expect("the judge reports a path under /out");
    let directory = work.join(relative).join("data");
    judge.run(
        "LuceneJudge",
        &["checkindex", "/out"],
        vec![Mount::read_only(&directory, "/out")],
    );
    let documents = judge
        .run(
            "LuceneJudge",
            &["numdocs", "/out"],
            vec![Mount::read_only(&directory, "/out")],
        )
        .trim()
        .parse::<u64>()
        .expect("numdocs prints one number");
    eprintln!("    clean, {documents} documents");
    documents
}

/// Step 5: why a pristine Oak store holds nodes its node-type index does not
/// name.
///
/// `docs/analysis/index-property-storage.md` §13 invariant 7 is what this
/// keeps honest, and the question is not academic: if Oak's editor *excluded*
/// those subtrees, froe's covered-node half would be over-reporting and
/// should apply the rule; if it covers them, the absences are a permanent
/// property of stores Oak wrote and can never be a verdict.
///
/// A **new node** is what settles it. `PropertyIndexEditor` writes an entry
/// when an indexed property is added, changed or removed, so touching an
/// existing node on an unrelated property is correctly a no-op whatever the
/// coverage rule is — the first version of this experiment did exactly that
/// and would have concluded the opposite. The content path is the control:
/// if its entry does not appear, the harness is what the verdict is about.
fn settle_the_unindexed_node_question(judge: &Judge, store: &Path, work: &Path) {
    eprintln!("  covered-node question: asking Oak's own editor");
    for (target, role) in [
        ("/content/interop", "the control"),
        ("/oak:index/lucene/indexRules", "a definition's own subtree"),
        ("/jcr:system/rep:permissionStore", "the permission store"),
    ] {
        let copy = work.join(format!("probe{}", target.replace(['/', ':'], "_")));
        copy_store(store, &copy);
        let result = format!("/out/probe{}.txt", target.replace(['/', ':'], "_"));
        judge.run(
            "IndexJudge",
            &["probe", "/store", &result, target],
            vec![
                Mount::writable(&copy, "/store"),
                Mount::writable(work, "/out"),
            ],
        );
        let reported = std::fs::read_to_string(
            work.join(result.strip_prefix("/out/").expect("a path under /out")),
        )
        .expect("read the probe result");
        assert!(
            reported.contains("before=false"),
            "{target} ({role}): the probe's own node already had an entry, so the \
             experiment proves nothing:\n{reported}"
        );
        assert!(
            reported.contains("after=true"),
            "{target} ({role}): Oak's own index update did NOT index a new node here. \
             That reverses invariant 7 of docs/analysis/index-property-storage.md — \
             the covered-node half is over-reporting and must apply Oak's rule \
             instead of reporting an observation:\n{reported}"
        );
    }
    eprintln!(
        "    Oak's editor covers all three, so the fixture's eighteen unindexed \
         nodes are about which commit wrote them"
    );
}

// ---------------------------------------------------------------------------
// Parsing, forging and copying
// ---------------------------------------------------------------------------

/// froe's `index list` output as one field map per index path.
fn parse_froe_index_list(
    rendered: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    let mut indexes = std::collections::BTreeMap::new();
    let mut current: Option<(String, std::collections::BTreeMap<String, String>)> = None;
    for line in rendered.lines() {
        if let Some(body) = line.strip_prefix("  ") {
            let (name, value) = body
                .split_once("  ")
                .map_or((body, ""), |(name, value)| (name, value.trim()));
            let Some((_, fields)) = current.as_mut() else {
                panic!("a field line before any index path: {line}");
            };
            fields.insert(name.trim().to_owned(), value.to_owned());
        } else if !line.trim().is_empty() {
            if let Some((path, fields)) = current.take() {
                indexes.insert(path, fields);
            }
            current = Some((line.to_owned(), std::collections::BTreeMap::new()));
        }
    }
    if let Some((path, fields)) = current.take() {
        indexes.insert(path, fields);
    }
    indexes
}

/// The index paths Oak's index printer lists.
fn oak_index_paths(json: &str) -> Vec<String> {
    json.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let path = trimmed.strip_prefix("\"/oak:index/")?;
            let path = path.strip_suffix("\": {")?;
            Some(format!("/oak:index/{path}"))
        })
        .collect()
}

/// One index's fields from Oak's index printer, as text.
fn oak_index_fields(json: &str, path: &str) -> Vec<(String, String)> {
    let marker = format!("\"{path}\": {{");
    let start = json
        .find(&marker)
        .unwrap_or_else(|| panic!("Oak's printer has no object for {path}"))
        + marker.len();
    let body = &json[start..];
    let end = body
        .find("\n    }")
        .or_else(|| body.find("\n      }"))
        .unwrap_or(body.len());
    body[..end]
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_end_matches(',');
            let (name, value) = trimmed.split_once("\": ")?;
            let name = name.strip_prefix('"')?;
            Some((name.to_owned(), value.trim_matches('"').to_owned()))
        })
        .collect()
}

/// The byte count froe prints in parentheses after a human-readable size.
fn byte_count(rendered: &str) -> String {
    rendered
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.strip_suffix(')'))
        .unwrap_or(rendered)
        .to_owned()
}

/// Copies a store directory, `repo.lock` aside.
fn copy_store(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("create the store copy");
    for entry in std::fs::read_dir(source).expect("read the store to copy") {
        let entry = entry.expect("read a store entry");
        if entry.file_name() == "repo.lock" {
            continue;
        }
        std::fs::copy(entry.path(), target.join(entry.file_name())).expect("copy a store file");
    }
}
