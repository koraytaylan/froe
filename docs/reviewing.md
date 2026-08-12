# Reviewing a change

[`../CONTRIBUTING.md`](../CONTRIBUTING.md) tells an author what a change must
prove. [`high-risk-changes.md`](high-risk-changes.md) is authoritative for
freezing and reviewing high-risk evidence. This document is about the
reviewer's own output, at every tier.

A review finding is a claim, and it costs the author a cycle whether or not it
is true. A false finding costs more than silence: it spends the author's
attention, and it teaches them to discount the next one. So findings carry the
same evidence standard as the code they are about.

Every rule below was earned by a finding that turned out to be wrong.

## A finding that asserts Oak behaviour cites the Java

Not an analysis document. [`analysis/README.md`](analysis/README.md) records the
commit those documents were read from and how to obtain a blob-filtered clone;
the specifications are derived, and the prime directive makes the Java ground
truth over them.

Two findings in one review asserted what Oak prints — one that it renders
malformed numeric text rather than failing, one that it absolutises a path in a
header. Both were refuted by reading two lines of Java
(`AbstractPropertyState.toString`, `DebugTars.java`). Both were written while a
checkout sat on the same machine. Cite the file and method, and quote the line
that decides it.

Remember that the behaviour may not be in `oak-segment-tar`: property rendering
and value conversion live in `oak-store-spi`.

## A finding that asserts a language or API fact quotes the definition

A finding claimed a `#[non_exhaustive]` configuration struct could not be
constructed by a downstream caller, and therefore that a documented limit was
unreachable. `#[non_exhaustive]` blocks struct-literal construction only; an
`impl Default` with public fields sat a few lines below the struct, so
`default()` followed by field assignment had always worked. Reading the
definition would have cost nothing.

Claims about visibility, trait availability, semantic versioning impact, or
what a lint does are all in this category.

## Validating a claim about a committed range isolates that range

Check out the range into a clean tree rather than reviewing in a worktree that
holds anything else. Two mechanisms make a dirty tree actively misleading:
`--all-features` silently enables features that untracked manifest edits have
added, and diff and formatting checks cover only tracked paths.

A validation run once reported that formatting and Clippy failed on a range
where the author claimed they passed. Every failure was in an untracked
work-in-progress test file that a newly added feature flag had pulled into the
build, and that file also contributed eight ignored tests, changing the test
inventory the author had reported. The author's claim was correct; the review
was contaminated. The same discipline the author owes — commit the candidate,
use a clean worktree — applies to whoever checks it.

## Severity and confidence are part of the claim

Separate what was verified from what was inferred, and say which commands were
run, on which platform and toolchain. Distinguish execution from
cross-compilation. Where a finding's severity depends on reachability, state
the reachability you established rather than the worst case you can imagine.

When a finding is partly right, report the narrower accurate version instead of
the original framing. Several findings in this project's history were correct
about a mechanism and wrong about its consequence; the useful output is the
mechanism plus the corrected consequence.

Record what the review did not cover, in the same place as what it found.

## Adversarial verification is for untrue claims, not minor ones

Independent verification of findings before reporting them is worth the cost —
it has killed most of the false findings this project has seen. It needs
calibrating in one specific way.

Refutation is for a claim that is untrue, unreachable, or already guarded. It is
not for a claim that is true but minor: that one keeps its place and gets an
honest severity. A verification pass instructed both to default to refutation
and to treat added process as a cost once rejected all twelve findings put to
it, while conceding the facts of most of them. Verdict and severity are separate
judgements, and collapsing them discards true findings.
