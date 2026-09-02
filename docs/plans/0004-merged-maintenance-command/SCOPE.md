# Scope — Plan 0004

> Fold `froe cleanup` into `froe compact` as the one maintenance command, reclaim every reclaimable archive without Oak's savings gate, retain one generation, retire journal history on every run, fix two write-path defects that produced stores passing every structural check, add `froe digest`, hold every interop phase to a declared content delta, and bring the tree under the craft standards that split it.

## Why this plan

A field report showed a `compact` followed by a `cleanup` leaving segments the journal could not reach, because the archive rewrite consulted Oak's savings gate and skipped partially dead archives. Two defects in the write path were found in the same period: a backup carried the content tree without its binary blocks whenever a copy crossed a store boundary, and two nodes differing only in a slot's arity could share a template record. None of these was caught by a structural check; only content comparison could see them, which is why `froe digest` and the declared-delta interop discipline were built in the same range.

The range is `v0.9.0` through `v0.10.0` and the fixes after it, 57 commits over 2026-08-17 and 2026-08-18, about fifty of them the refactors that brought every file under a thousand lines and every function under a hundred. It is high-risk on four counts named in the safety case, and it is the range whose review recorded the Windows break that fifty unpushed commits had hidden.

## In scope

- **One command.** `froe compact` plans once, certifies every reclaim source in parallel before the copy appends anything, copies, publishes the head exactly once, sweeps every reclaimable archive (`ArchiveRewritePolicy::EveryReclaimableArchive`), appends one `gc.log` line, retires the journal to the head line, verifies the applied state on a fresh reopen and retires residue.
- **One retained generation**, with `validate_reclaim_reference_invariant` replacing the margin two generations used to provide.
- **The write-path fixes.** `BulkBlockSharing` as a required argument of `copy_binary_value` with no default; `property_slot_tag` as the one computation deciding a slot's arity.
- **`froe digest`**, a canonical rendering of a repository's content, and the interop rule that every mutating phase declares the delta its before-and-after digests may show.
- **Craft standards.** `CONTRIBUTING.md`'s clean-code rules, `clippy.toml` bounds on branching and nesting, `scripts/oversized-files.sh` as a gate, and the module splits that satisfy them.
- **The safety case**, its adversarial review, and the Windows repairs that review's CI run forced.

## Out of scope

- A sweep-only mode: a run requires headroom equal to its live set, deliberately, and the plan prints the working-space proxy instead.
- A journal retention bound under a compaction: accepted by the library and ignored on that path, recorded as gap 1.
- `store.version=1` stores, external blob stores, native Windows execution and Adobe AEM itself.
