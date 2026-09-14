//! Driving content through Sling's HTTP interface: posting nodes,
//! churning them into garbage, and reading a snapshot back.

use super::*;

// ---------------------------------------------------------------------------
// Content population
// ---------------------------------------------------------------------------

/// Create content nodes via the `SlingPostServlet`.
///
/// Asserted rather than ignored: every phase copies the fixture this
/// builds, so a post that silently failed poisons a *later* phase — the
/// Aug 20 run whose binary upload never landed recorded a 14-entry
/// baseline instead of 21 and failed 90 s later in `read` as
/// `tree shows nt:file`, attributed to the wrong operation. `--fail`
/// turns an HTTP error into a curl exit status, which the assertion
/// turns into a `generate` failure naming the path that did not land.
pub(crate) fn sling_post(port: u16, path: &str, primary_type: &str, title: &str) {
    sling_post_fields(
        port,
        path,
        &[("jcr:primaryType", primary_type), ("jcr:title", title)],
    );
}

/// Posts `fields` to `path` and refuses anything but a 2xx, naming the
/// status and the body.
///
/// The status is what makes a failure attributable. An assertion that says
/// only "posting X failed" cannot distinguish a servlet that is not up yet
/// (404), a repository that has not finished starting (503), a rejected
/// value (500) and a wrong credential (401) — and every phase copies the
/// fixture this builds, so a post that silently failed poisons a *later*
/// phase. The Aug 20 run whose binary upload never landed recorded a
/// 14-entry baseline instead of 21 and failed 90 s later in `read` as
/// `tree shows nt:file`, attributed to the wrong operation.
fn post_or_refuse(port: u16, path: &str, fields: &[(&str, &str)]) {
    let url = format!("http://localhost:{port}{path}");
    let mut command = Command::new("curl");
    // `-w` appends the status after the body, and no `--fail`, so the body
    // of an error response survives to be reported rather than discarded.
    command.args(["-s", "-u", "admin:admin", "-w", "\nHTTP %{http_code}"]);
    for (name, value) in fields {
        command.args(["-F", &format!("{name}={value}")]);
    }
    command.arg(&url);
    let output = command.output().expect("curl POST");
    assert!(
        output.status.success(),
        "curl itself failed posting {path}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response = String::from_utf8_lossy(&output.stdout);
    let status = response
        .rsplit_once("HTTP ")
        .map_or("unknown", |(_, code)| code.trim());
    assert!(
        status.starts_with('2'),
        "posting {path} returned HTTP {status}\nfields: {fields:?}\nresponse:\n{}",
        &response[..response.len().min(2000)]
    );
}

/// Churn content: create subtrees, then delete them. Produces orphaned
/// segments that compaction can later reclaim.
pub(crate) fn churn_content(port: u16) {
    for round in 0..3u32 {
        eprintln!(
            "  churn round {}/{round}: creating 20 throwaway subtrees",
            round + 1
        );
        for i in 0..20u32 {
            let path = format!("/content/interop/throwaway/{round}/{i}");
            sling_post(
                port,
                &path,
                "sling:Folder",
                &format!("Throwaway {round}.{i}"),
            );
            for k in 1..=5u32 {
                let child = format!("{path}/child{k}");
                sling_post(
                    port,
                    &child,
                    "sling:OrderedFolder",
                    &format!("Child {round}.{i}.{k}"),
                );
            }
        }
        eprintln!(
            "  churn round {}/{round}: deleting 20 throwaway subtrees",
            round + 1
        );
        for i in 0..20u32 {
            let url = format!("http://localhost:{port}/content/interop/throwaway/{round}/{i}");
            let _ = Command::new("curl")
                .args([
                    "-s",
                    "-o",
                    "/dev/null",
                    "-X",
                    "DELETE",
                    "-u",
                    "admin:admin",
                    &url,
                ])
                .status();
        }
    }
}

/// Populate a realistic content tree under /content/interop.
pub(crate) fn populate_content(port: u16) {
    sling_post(port, "/content/interop", "sling:Folder", "Interop Fixture");
    sling_post(port, "/content/interop/pages", "sling:Folder", "Test Pages");
    for i in 1..=5u32 {
        sling_post(
            port,
            &format!("/content/interop/pages/page{i}"),
            "sling:OrderedFolder",
            &format!("Page {i}"),
        );
    }
    // Binary node: nt:file with inline jcr:data, deliberately large enough to
    // be stored as bulk segments rather than materialized inline.
    //
    // Oak splits a value at or above 16512 bytes into a block list, and full
    // 256 KiB runs of those blocks become bulk segments. That is what makes
    // this fixture resemble a real repository: compaction references bulk
    // segments where they lie instead of copying them, so the archives holding
    // them survive a compaction while their data segments die — which is the
    // only shape that exercises the partial-archive rewrite, and the shape the
    // field report that motivated one-command maintenance was made of. A short
    // binary is materialized whole and never produces a bulk segment at all.
    let binary_path = work_root().join("binary.txt");
    std::fs::write(
        &binary_path,
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
         Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n"
            .repeat(16_384),
    )
    .expect("write binary.txt");
    let status = Command::new("curl")
        .args([
            "-s",
            "--fail",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            "jcr:primaryType=sling:Folder",
            "-F",
            &format!("file=@{}", binary_path.display()),
            &format!("http://localhost:{port}/content/interop/files"),
        ])
        .status()
        .expect("curl upload binary");
    assert!(
        status.success(),
        "uploading the binary fixture to /content/interop/files failed; the phases \
         that assert the binary round-trips would fail later without it"
    );
}

/// Fetch the content tree JSON from Sling for verification.
pub(crate) fn content_snapshot(port: u16) -> String {
    let output = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            &format!("http://localhost:{port}/content/interop.tidy.-1.json"),
        ])
        .output()
        .expect("curl content snapshot");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Versioning through Sling, for the purge phase
// ---------------------------------------------------------------------------

/// Creates a `mix:versionable` node via the `SlingPostServlet`.
pub(crate) fn sling_post_versionable(port: u16, path: &str, title: &str) {
    let url = format!("http://localhost:{port}{path}");
    let status = Command::new("curl")
        .args([
            "-s",
            "--fail",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            "jcr:primaryType=nt:unstructured",
            "-F",
            "jcr:mixinTypes=mix:versionable",
            "-F",
            &format!("jcr:title={title}"),
            &url,
        ])
        .status()
        .expect("curl POST versionable");
    assert!(status.success(), "posting {path} failed");
}

/// One `SlingPostServlet` `:operation` against a node.
fn sling_operation(port: u16, path: &str, operation: &str) {
    let url = format!("http://localhost:{port}{path}");
    let status = Command::new("curl")
        .args([
            "-s",
            "--fail",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            &format!(":operation={operation}"),
            &url,
        ])
        .status()
        .expect("curl POST operation");
    assert!(status.success(), "{operation} on {path} failed");
}

/// Checks a versionable node in, making Oak append a version to its
/// history. Verified from the node itself: a checkin the servlet accepted
/// but Oak rejected would silently leave the fixture without the version
/// the purge phase depends on.
pub(crate) fn sling_checkin(port: u16, path: &str) {
    sling_operation(port, path, "checkin");
    let rendered = sling_get_json(port, path);
    assert!(
        rendered.contains("\"jcr:isCheckedOut\":false"),
        "{path} still reads as checked out after checkin: {rendered}"
    );
}

/// Checks a versionable node out again, so a later checkin creates a
/// fresh version.
pub(crate) fn sling_checkout(port: u16, path: &str) {
    sling_operation(port, path, "checkout");
}

/// Deletes a node, orphaning whatever version history it had.
pub(crate) fn sling_delete(port: u16, path: &str) {
    sling_operation(port, path, "delete");
}

/// A node rendered as JSON, straight from Sling.
pub(crate) fn sling_get_json(port: u16, path: &str) -> String {
    let url = format!("http://localhost:{port}{path}.json");
    let output = Command::new("curl")
        .args(["-s", "--fail", "-u", "admin:admin", &url])
        .output()
        .expect("curl GET json");
    assert!(output.status.success(), "reading {path} failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The node's `jcr:uuid`, read from Sling's JSON rendering.
pub(crate) fn sling_node_identifier(port: u16, path: &str) -> String {
    let rendered = sling_get_json(port, path);
    let marker = "\"jcr:uuid\":\"";
    let start = rendered
        .find(marker)
        .unwrap_or_else(|| panic!("{path} has no jcr:uuid: {rendered}"))
        + marker.len();
    let identifier = &rendered[start..start + 36];
    assert_eq!(identifier.len(), 36, "identifier shape: {identifier}");
    identifier.to_owned()
}

// ---------------------------------------------------------------------------
// The index shapes plans 0007 onward rebuild
// ---------------------------------------------------------------------------

/// A `SlingPostServlet` post carrying arbitrary fields.
///
/// The typed posts above each fix their own property set; this is for the
/// shapes that need a `@TypeHint`, which is how Sling is told to store a
/// value as something other than a `STRING`. The distinction is not
/// cosmetic: Oak's reference index only indexes `REFERENCE` and
/// `WEAKREFERENCE` properties, and its query planner reads a definition's
/// `propertyNames` strictly as `NAMES`, so a value that arrived as a
/// `STRING` produces a fixture that looks right and indexes nothing.
pub(crate) fn sling_post_fields(port: u16, path: &str, fields: &[(&str, &str)]) {
    post_or_refuse(port, path, fields);
}

/// Adds the reference shapes: a `mix:referenceable` target and a sibling
/// holding a `REFERENCE` and a `WEAKREFERENCE` to it.
///
/// This is what puts an entry in `/oak:index/reference/:references` **and**
/// in `:weakreferences`. Without it the fixture's reference index is empty,
/// and an empty index proves nothing about a reader of one.
///
/// A second referenceable whose reference lives under
/// `/jcr:system/jcr:versionStorage` is out of reach through Sling — that
/// subtree is not writable through the post servlet — and is covered by the
/// unit tests instead.
pub(crate) fn populate_references(port: u16) {
    let target = "/content/interop/references/target";
    sling_post_fields(
        port,
        target,
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("jcr:mixinTypes", "mix:referenceable"),
            ("jcr:title", "Reference Target"),
        ],
    );
    let identifier = sling_node_identifier(port, target);
    sling_post_fields(
        port,
        "/content/interop/references/source",
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("jcr:title", "Reference Source"),
            ("ref@TypeHint", "Reference"),
            ("ref", &identifier),
            ("weakRef@TypeHint", "WeakReference"),
            ("weakRef", &identifier),
        ],
    );
}

/// Creates a group with two members through Sling's user manager servlet.
///
/// This is what puts `rep:members` under a `rep:MemberReferences` node, which
/// is the one definition in the fixture with a `declaringNodeTypes`
/// restriction over a *multi-valued* property. Nothing else in the store
/// exercises that pair.
pub(crate) fn populate_group_with_members(port: u16) {
    for member in ["interop-member-one", "interop-member-two"] {
        sling_user_manager(
            port,
            "/system/userManager/user.create.html",
            &[
                (":name", member),
                ("pwd", "interop-secret"),
                ("pwdConfirm", "interop-secret"),
            ],
        );
    }
    sling_user_manager(
        port,
        "/system/userManager/group.create.html",
        &[(":name", "interop-group")],
    );
    sling_user_manager(
        port,
        "/system/userManager/group/interop-group.update.html",
        &[
            (":member", "/system/userManager/user/interop-member-one"),
            (":member", "/system/userManager/user/interop-member-two"),
        ],
    );
}

fn sling_user_manager(port: u16, path: &str, fields: &[(&str, &str)]) {
    post_or_refuse(port, path, fields);
}

/// Posts a property index definition over `jcr:title` **after** the content
/// exists, so Oak rebuilds it from that content.
///
/// The reindex test is true for the `reindex` flag and for a brand-new
/// definition alike, under the default `oak.indexUpdate.ignoreReindexFlags=false`:
/// collecting the editors clears the flag, increments `reindexCount` and
/// registers the editor, and the cycle then runs that editor from the
/// missing state to the head before the commit completes. So the fixture
/// **cannot** carry a definition that is still flagged — a still-flagged one
/// comes from a synthetic store or from a writer helper on a copy. What it
/// carries instead is the shape plan 0007's oracle needs: a property index
/// Oak rebuilt from existing content in one synchronous cycle.
///
/// `propertyNames` is posted as `Name[]` deliberately. The editor would
/// convert a `STRING`, but the query planner reads it strictly and sees a
/// `STRING`, or a single `NAME`, as empty — so a fixture posted without the
/// type hint would index correctly and plan as though the index did not
/// exist.
pub(crate) fn populate_rebuilt_property_index(port: u16) {
    sling_post_fields(
        port,
        "/oak:index/interopTitle",
        &[
            ("jcr:primaryType", "oak:QueryIndexDefinition"),
            ("type", "property"),
            ("propertyNames@TypeHint", "Name[]"),
            ("propertyNames", "jcr:title"),
            ("reindex@TypeHint", "Boolean"),
            ("reindex", "true"),
        ],
    );
}
