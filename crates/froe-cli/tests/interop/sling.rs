//! Driving content through Sling's HTTP interface: posting nodes,
//! churning them into garbage, and reading a snapshot back.

use super::*;

// ---------------------------------------------------------------------------
// Content population
// ---------------------------------------------------------------------------

/// Create content nodes via the `SlingPostServlet`.
pub(crate) fn sling_post(port: u16, path: &str, primary_type: &str, title: &str) {
    let url = format!("http://localhost:{port}{path}");
    let _ = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-u",
            "admin:admin",
            "-F",
            &format!("jcr:primaryType={primary_type}"),
            "-F",
            &format!("jcr:title={title}"),
            &url,
        ])
        .status()
        .expect("curl POST");
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
    let _ = Command::new("curl")
        .args([
            "-s",
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
