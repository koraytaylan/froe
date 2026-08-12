#!/usr/bin/env bash
# Thin wrapper around the Rust interop test suite.
#
# The tests live in crates/froe-cli/tests/interop.rs and run under
# `cargo test` behind the `interop` feature flag. This script is a
# convenience entrypoint so the CI workflow and developers don't need
# to remember the cargo invocation.
#
# Usage:
#   scripts/interop-fixture.sh             # run all phases in order
#   scripts/interop-fixture.sh read        # run a single phase
#   scripts/interop-fixture.sh compact     # (phases: generate, read,
#   scripts/interop-fixture.sh cleanup     #  checkpoint, compact, cleanup,
#   scripts/interop-fixture.sh backup      #  backup, recover)
#   scripts/interop-fixture.sh recover
#
# Prerequisites:
#   - podman installed and runnable by the current user
#   - network access to pull docker.io/apache/sling:14 once
#   - froe built: cargo build --release
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
    cargo test -p froe-cli --features interop -- --ignored --test-threads=1 --nocapture interop_full
else
    # Run a single phase.
    phase="$1"
    case "$phase" in
        generate|read|checkpoint|commit|compact|cleanup|backup|recover)
            # generate must run first for all other phases.
            if [[ "$phase" != "generate" ]]; then
                echo "Running 'generate' first (required by all phases)..."
                cargo test -p froe-cli --features interop -- --ignored --test-threads=1 --nocapture generate
            fi
            cargo test -p froe-cli --features interop -- --ignored --test-threads=1 --nocapture "$phase"
            ;;
        *)
            echo "Unknown phase: $phase" >&2
            echo "Phases: generate, read, checkpoint, commit, compact, cleanup, backup, recover" >&2
            exit 1
            ;;
    esac
fi