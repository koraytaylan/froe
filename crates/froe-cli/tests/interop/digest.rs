//! froe's canonical rendering of a store's content, and the delta a phase
//! is allowed to leave behind in it.

use super::*;

// ---------------------------------------------------------------------------
// Content digest: the attribution mechanism
// ---------------------------------------------------------------------------

/// Digests with content subtrees excluded — the comparison a confirmed
/// purge is held to: before-digest and after-digest carry the same named
/// exclusions, and everything outside them must match.
pub(crate) fn digest_store_excluding(store: &Path, excluded: &[&str]) -> String {
    let mut arguments = vec!["digest", store.to_str().unwrap()];
    for prefix in excluded {
        arguments.push("--exclude-subtree");
        arguments.push(prefix);
    }
    froe(&arguments)
}

/// The canonical content rendering of a store.
///
/// This is what makes damage attributable rather than merely detectable.
/// Every mutating phase digests its store and compares against the
/// baseline, so the operation named in the difference *is* the operation
/// that changed something — instead of a fatal error three phases later
/// with no way to tell which run introduced it.
pub(crate) fn digest_store(store: &Path) -> String {
    froe(&["digest", store.to_str().unwrap()])
}

/// What an operation is expected to change in the content digest.
///
/// Every phase declares one. `None` is by far the strongest and is what
/// most maintenance should be able to claim: the store was rewritten and
/// not one node, property, type, arity, value or binary moved.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ExpectedDigestDelta {
    /// Nothing changed anywhere, checkpoints included.
    None,
    /// Only checkpoint subtrees may change; the content tree must be
    /// byte-identical. Used where the operation retires checkpoints by
    /// design.
    CheckpointsOnly,
}

/// Splits a digest into `path -> properties`.
pub(crate) fn digest_nodes(digest: &str) -> std::collections::BTreeMap<&str, &str> {
    digest
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once('\t').unwrap_or((line, "")))
        .collect()
}

/// Assert the digest changed exactly as much as the phase declared.
pub(crate) fn assert_digest_delta(
    baseline: &str,
    current: &str,
    expected: ExpectedDigestDelta,
    phase: &str,
) {
    let before = digest_nodes(baseline);
    let after = digest_nodes(current);
    assert!(
        !before.is_empty() && !after.is_empty(),
        "{phase}: a digest is empty, so comparing them would be vacuous"
    );

    let mut differences: Vec<String> = Vec::new();
    for (path, properties) in &before {
        match after.get(path) {
            None => differences.push(format!("removed {path}")),
            Some(current_properties) if current_properties != properties => {
                differences.push(format!(
                    "changed {path}\n    before: {properties}\n    after:  {current_properties}"
                ));
            }
            Some(_) => {}
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            differences.push(format!("added {path}"));
        }
    }

    let offending: Vec<&String> = match expected {
        ExpectedDigestDelta::None => differences.iter().collect(),
        // The super-root's own line names its children, and retiring a
        // checkpoint rewrites the checkpoints container, so both move
        // legitimately when checkpoints do.
        ExpectedDigestDelta::CheckpointsOnly => differences
            .iter()
            .filter(|difference| {
                let path = difference
                    .split_once(' ')
                    .map_or("", |(_, rest)| rest.lines().next().unwrap_or(""));
                !path.starts_with("#checkpoint") && !path.starts_with("#super-root")
            })
            .collect(),
    };

    assert!(
        offending.is_empty(),
        "{phase}: the content digest changed in ways the phase did not declare.\n\
         This is the attribution signal: the operation this phase ran is the one that \
         changed content it was supposed to preserve.\n{} unexpected difference(s):\n{}",
        offending.len(),
        offending
            .iter()
            .take(20)
            .map(|difference| format!("  {difference}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Reported precisely rather than as "matches": a run that legitimately
    // retires a checkpoint moves tens of thousands of lines, and calling
    // that "matches the baseline" is the kind of claim-stronger-than-the-
    // evidence this whole comparison exists to eliminate.
    let declared = differences.len();
    if declared == 0 {
        eprintln!(
            "  content digest identical to the baseline ({} nodes, nothing changed)",
            after.len()
        );
    } else {
        eprintln!(
            "  content digest holds: {} nodes after, {declared} line(s) differ and every one \
             is within the declared scope",
            after.len()
        );
    }
}
