#!/usr/bin/env bash
# Thin wrapper around the Rust interop test suite.
#
# The tests live in crates/froe-cli/tests/interop/ and run under
# `cargo test` behind the `interop` feature flag. This script is a
# convenience entrypoint so the CI workflow and developers don't need
# to remember the cargo invocation.
#
# Usage:
#   scripts/interop-fixture.sh             # run all phases in order
#   scripts/interop-fixture.sh read        # run a single phase
#
# The phases, in the order interop_full runs them. This list, the accept
# pattern below and the error text below it are the same list three times
# over; all three are kept in step with `interop_full` deliberately, because
# a phase the script will not accept is a phase nobody can re-run on its own,
# and a failure that cannot be reproduced in isolation cannot be attributed
# to anything.
#
#   generate, read, judge_smoke, index_inventory, property_reindex,
#   lucene_dump, lucene_import, commit, checkpoint, compact, compact_tail,
#   checkpoint_removal, cleanup, journal_retention, compact_convergence,
#   version_history_purge, repair, backup, recover
#
# Prerequisites:
#   - podman installed and runnable by the current user
#   - network access to pull docker.io/apache/sling:14 once
#   - nothing prebuilt: the suite runs under `cargo test --release`, so the
#     binary it exercises is the release binary cargo builds for the tests
#
# Everything in the loop is Apache-2.0 (Apache Sling + Apache Jackrabbit
# Oak); no Adobe license is involved at any point.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Build froe if the binary is missing.
if [[ ! -x target/release/froe ]]; then
    echo "Building froe (release)..."
    cargo build --release
fi

if [[ $# -eq 0 ]]; then
    # Run all phases in dependency order.
    #
    # The output is teed and then checked for the suite's completion sentinel,
    # because `cargo test` exits zero when a filter matches nothing: a renamed
    # test, a dropped `interop` feature, or a typo in the filter would otherwise
    # produce a green run that executed no interop phase at all. Requiring the
    # sentinel means green implies the chain actually ran to the end.
    log="$(mktemp)"
    trap 'rm -f "$log"' EXIT
    set -o pipefail
    cargo test --release -p froe-cli --features interop -- \
        --ignored --test-threads=1 --nocapture interop_full 2>&1 | tee "$log"
    if ! grep -q "all interop phases passed" "$log"; then
        echo "interop: the suite reported success but never printed its completion" \
             "sentinel, so no phase ran; refusing to report a pass" >&2
        exit 1
    fi
else
    # Run a single phase.
    phase="$1"
    case "$phase" in
        generate|read|judge_smoke|index_inventory|property_reindex|lucene_dump|lucene_import|commit|checkpoint|compact|compact_tail|checkpoint_removal|cleanup|journal_retention|compact_convergence|version_history_purge|repair|backup|recover)
            # generate must run first for all other phases.
            if [[ "$phase" != "generate" ]]; then
                echo "Running 'generate' first (required by all phases)..."
                cargo test --release -p froe-cli --features interop -- --ignored --test-threads=1 --nocapture generate
            fi
            cargo test --release -p froe-cli --features interop -- --ignored --test-threads=1 --nocapture "$phase"
            ;;
        *)
            echo "Unknown phase: $phase" >&2
            echo "Phases: generate, read, judge_smoke, index_inventory," >&2
            echo "        property_reindex, lucene_dump, lucene_import," >&2
            echo "        commit, checkpoint, compact, compact_tail," >&2
            echo "        checkpoint_removal, cleanup, journal_retention," >&2
            echo "        compact_convergence, version_history_purge," >&2
            echo "        repair, backup, recover" >&2
            exit 1
            ;;
    esac
fi