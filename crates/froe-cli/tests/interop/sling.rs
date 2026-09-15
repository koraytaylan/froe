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
    let mut last = String::new();
    for attempt in 1..=SERVLET_GAP_ATTEMPTS {
        let (status, response) = post_once(port, path, fields);
        if status.starts_with('2') {
            if attempt > 1 {
                eprintln!("  posting {path} succeeded on attempt {attempt}");
            }
            return;
        }
        // A *bare Jetty* error means Sling's servlet is not mapped for this
        // request — the boot-time gap this suite has hit before, and which
        // also reopens briefly when a bundle re-wires mid-run. It is not a
        // rejection of what was posted: Sling's own errors carry Sling's
        // body. Waiting is the right response to a servlet that is not
        // there; failing on anything else is the right response to a
        // repository that refused.
        assert!(
            is_bare_jetty_error(&response),
            "posting {path} returned HTTP {status}\nfields: {fields:?}\nresponse:\n{}",
            &response[..response.len().min(2000)]
        );
        eprintln!(
            "  posting {path} hit a bare Jetty {status} (attempt {attempt} of \
             {SERVLET_GAP_ATTEMPTS}); Sling's servlet is momentarily unmapped, waiting"
        );
        last = response;
        std::thread::sleep(SERVLET_GAP_WAIT);
    }
    panic!(
        "posting {path} kept hitting a bare Jetty error through {SERVLET_GAP_ATTEMPTS} \
         attempts over {:?}; Sling's servlet never came back.\nfields: {fields:?}\n\
         response:\n{}",
        SERVLET_GAP_WAIT * SERVLET_GAP_ATTEMPTS,
        &last[..last.len().min(2000)]
    );
}

/// How many times a post waits out a servlet gap before giving up.
const SERVLET_GAP_ATTEMPTS: u32 = 12;

/// How long each of those waits is.
const SERVLET_GAP_WAIT: Duration = Duration::from_secs(5);

/// One POST, returning its status and body.
fn post_once(port: u16, path: &str, fields: &[(&str, &str)]) -> (String, String) {
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
    let response = String::from_utf8_lossy(&output.stdout).into_owned();
    let status = response
        .rsplit_once("HTTP ")
        .map_or_else(|| "unknown".to_owned(), |(_, code)| code.trim().to_owned());
    (status, response)
}

/// Whether the body is Jetty's own error page rather than Sling's.
///
/// Jetty answers for requests Sling's servlet is not mapped for. Its page
/// carries the container's own markup and none of Sling's, which is what
/// separates "the servlet is not there" from "the repository said no".
fn is_bare_jetty_error(response: &str) -> bool {
    response.contains("<title>Error 404 Not Found</title>")
        || (response.contains("HTTP ERROR") && !response.contains("Sling"))
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
    let mut last = String::new();
    // Waited out, for the same reason a post is: Sling answers a read of a
    // node it has just accepted with a bare Jetty 404 while its servlet is
    // momentarily unmapped, and `generate` has failed on exactly that —
    // reading back the reference target it had just posted. A read that
    // gave up on the first status turned a servlet gap into a fixture
    // failure attributed to the node.
    for attempt in 1..=SERVLET_GAP_ATTEMPTS {
        let output = Command::new("curl")
            .args(["-s", "-u", "admin:admin", "-w", "\nHTTP %{http_code}", &url])
            .output()
            .expect("curl GET json");
        let response = String::from_utf8_lossy(&output.stdout).into_owned();
        let status = response
            .rsplit_once("HTTP ")
            .map_or("unknown", |(_, code)| code.trim())
            .to_owned();
        if status.starts_with('2') {
            if attempt > 1 {
                eprintln!("  reading {path} succeeded on attempt {attempt}");
            }
            let body = response
                .rsplit_once("\nHTTP ")
                .map_or(response.as_str(), |(body, _)| body);
            return body.to_owned();
        }
        eprintln!(
            "  reading {path} returned HTTP {status} (attempt {attempt} of \
             {SERVLET_GAP_ATTEMPTS}), waiting"
        );
        last = response;
        std::thread::sleep(SERVLET_GAP_WAIT);
    }
    panic!(
        "reading {path} never succeeded over {:?}:\n{}",
        SERVLET_GAP_WAIT * SERVLET_GAP_ATTEMPTS,
        &last[..last.len().min(2000)]
    );
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

// ---------------------------------------------------------------------------
// The reindex oracle's helpers
// ---------------------------------------------------------------------------

/// One property to set through the POST servlet, with the type hint Sling
/// needs to store it as anything but a `String`.
pub(crate) struct SlingProperty<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: &'a str,
    pub(crate) type_hint: Option<&'a str>,
}

/// Sets one property on an existing node.
///
/// The type hint matters: `reindex` is read converting, but `reindexCount`
/// and `retainNodeInReindex` are not, and a `String` `"true"` where Oak
/// stores a `Boolean` is a different store — which is exactly the kind of
/// difference this suite exists to catch, so the fixture must not create it
/// by accident.
pub(crate) fn sling_set_property(port: u16, path: &str, property: &SlingProperty<'_>) {
    let hint_name = property
        .type_hint
        .map(|_| format!("{}@TypeHint", property.name));
    let mut fields: Vec<(&str, &str)> = vec![(property.name, property.value)];
    if let (Some(name), Some(hint)) = (hint_name.as_deref(), property.type_hint) {
        fields.push((name, hint));
    }
    post_or_refuse(port, path, &fields);
}

/// Waits until Oak has finished rebuilding `index_path`.
///
/// Oak clears `reindex` and advances `reindexCount` in the commit that
/// finishes the rebuild, so both together are the completion signal: the
/// flag alone would be satisfied by a definition that was never flagged.
pub(crate) fn sling_wait_until_reindexed(port: u16, index_path: &str, count_before: i64) -> i64 {
    let within = Duration::from_secs(180);
    let deadline = Instant::now() + within;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = sling_get_json(port, index_path);
        let flagged = last.contains("\"reindex\":true");
        let count = json_number(&last, "reindexCount").unwrap_or(0);
        if !flagged && count > count_before {
            return count;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!("Oak did not finish rebuilding {index_path} within {within:?}; last rendering:\n{last}");
}

/// A JSON number field, read without a JSON parser.
///
/// The suite deliberately has no JSON dependency: these renderings are
/// flat, and a hand-rolled read that fails loudly beats a dependency in a
/// test harness.
pub(crate) fn json_number(rendered: &str, name: &str) -> Option<i64> {
    let marker = format!("\"{name}\":");
    let start = rendered.find(&marker)? + marker.len();
    let rest = &rendered[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// The query probe
// ---------------------------------------------------------------------------

/// Installs a JSP that runs a JCR-SQL2 statement, and a node to invoke it.
///
/// The Sling GET servlet in this image ships no query servlet, so this is
/// the only way to put a statement through the live query engine — which is
/// what makes the comparison a claim about *Oak answering queries from
/// froe's index* rather than about two byte renderings agreeing.
///
/// Two nodes, because Sling's script resolution needs both: the script at
/// `/apps/froe/query/html.jsp`, and a resource whose `sling:resourceType`
/// is `froe/query` to request it through. Requesting the `.jsp` node
/// directly would return the file, not run it.
pub(crate) fn sling_install_query_probe(port: u16) {
    // `sling:defineObjects` is what puts `resourceResolver` in scope; a JSP
    // using it without the taglib does not compile. EXPLAIN is detected
    // from the statement rather than from the result's columns, because
    // Oak's explain result does not name its column the way an ordinary
    // one does and `getPath()` then throws "this query does not have a
    // selector" — a 500 that says nothing about the query.
    //
    // A `column` parameter prints that column's value instead of the row's
    // path, which is the only way to read a facet result: `rep:facet(x)`
    // has no path of its own, and a probe that could only print paths
    // would make every facet comparison a comparison of row counts.
    const PROBE: &str = concat!(
        "<%@page session=\"false\" import=\"javax.jcr.*,javax.jcr.query.*\"%>",
        "<%@taglib prefix=\"sling\" uri=\"http://sling.apache.org/taglibs/sling/1.0\"%>",
        "<sling:defineObjects/><%\n",
        "response.setContentType(\"text/plain\");\n",
        "String statement = request.getParameter(\"statement\");\n",
        "String column = request.getParameter(\"column\");\n",
        "boolean explaining = statement.trim().toUpperCase().startsWith(\"EXPLAIN\");\n",
        "Session session = resourceResolver.adaptTo(Session.class);\n",
        "QueryManager manager = session.getWorkspace().getQueryManager();\n",
        "Query query = manager.createQuery(statement, \"JCR-SQL2\");\n",
        "QueryResult result = query.execute();\n",
        "RowIterator rows = result.getRows();\n",
        "while (rows.hasNext()) {\n",
        "  Row row = rows.nextRow();\n",
        "  if (explaining) {\n",
        "    out.println(row.getValue(\"plan\").getString());\n",
        "  } else if (column != null && column.length() > 0) {\n",
        "    javax.jcr.Value value = row.getValue(column);\n",
        "    out.println(value == null ? \"\" : value.getString());\n",
        "  } else {\n",
        "    out.println(row.getPath());\n",
        "  }\n",
        "}\n",
        "%>"
    );

    sling_post_fields(
        port,
        PROBE_SCRIPT_FOLDER,
        &[("jcr:primaryType", "sling:Folder")],
    );

    // Sling's own file upload: POST to the parent with a form field named
    // after the file, and the servlet creates the `nt:file` and its
    // `jcr:content` itself.
    let source = std::env::temp_dir().join(format!("froe-query-probe-{}.jsp", std::process::id()));
    std::fs::write(&source, PROBE).expect("write the probe source");
    // No trailing slash: a POST to `…/query/` makes Sling generate a node
    // name rather than using the form field's, and the script then is not
    // where script resolution looks for it.
    let url = format!("http://localhost:{port}{PROBE_SCRIPT_FOLDER}");
    let output = Command::new("curl")
        .args([
            "-s",
            "-u",
            "admin:admin",
            "-w",
            "\nHTTP %{http_code}",
            "-F",
            &format!("html.jsp=@{}", source.display()),
            &url,
        ])
        .output()
        .expect("upload the query probe");
    let response = String::from_utf8_lossy(&output.stdout);
    let status = response
        .rsplit_once("HTTP ")
        .map_or("unknown", |(_, code)| code.trim());
    assert!(
        status.starts_with('2'),
        "installing the query probe returned HTTP {status}:\n{}",
        &response[..response.len().min(2000)]
    );
    let _ = std::fs::remove_file(&source);

    // The resource the script answers for.
    sling_post_fields(
        port,
        PROBE_RESOURCE,
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("sling:resourceType", "froe/query"),
        ],
    );

    // Prove the probe runs before any comparison depends on it. A probe
    // that silently returned nothing would make every result set equal.
    let rows = sling_query(port, "SELECT * FROM [rep:root]");
    assert_eq!(
        rows,
        vec!["/".to_owned()],
        "the query probe does not answer; every comparison built on it would be vacuous"
    );
}

/// Where the probe's script lives.
const PROBE_SCRIPT_FOLDER: &str = "/apps/froe/query";

/// The resource the probe is requested through.
const PROBE_RESOURCE: &str = "/content/froe-query-probe";

/// Runs one JCR-SQL2 statement through the probe and returns its lines.
pub(crate) fn sling_query(port: u16, statement: &str) -> Vec<String> {
    run_the_probe(port, statement, None)
}

/// The same, printing one named column's value per row instead of the
/// row's path.
///
/// This is how a facet result is read: `rep:facet(<property>)` names no
/// node, so every row of such a query has the same (absent) path and a
/// comparison of paths would compare nothing.
pub(crate) fn sling_query_column(port: u16, statement: &str, column: &str) -> Vec<String> {
    run_the_probe(port, statement, Some(column))
}

/// One request to the probe, with or without a column.
fn run_the_probe(port: u16, statement: &str, column: Option<&str>) -> Vec<String> {
    let url = format!("http://localhost:{port}{PROBE_RESOURCE}.html");
    let mut command = Command::new("curl");
    command.args([
        "-s",
        "-u",
        "admin:admin",
        "-w",
        "\nHTTP %{http_code}",
        "--data-urlencode",
        &format!("statement={statement}"),
    ]);
    if let Some(column) = column {
        command.args(["--data-urlencode", &format!("column={column}")]);
    }
    command.args(["-G", &url]);
    let output = command.output().expect("run the query probe");
    let response = String::from_utf8_lossy(&output.stdout);
    let (body, status) = response
        .rsplit_once("\nHTTP ")
        .expect("curl appends the status");
    assert!(
        status.trim().starts_with('2'),
        "the query probe refused {statement:?} with HTTP {}:\n{}",
        status.trim(),
        &body[..body.len().min(2000)]
    );
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}
