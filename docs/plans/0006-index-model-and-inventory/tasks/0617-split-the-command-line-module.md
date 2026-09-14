---
id: split-the-command-line-module
title: Split The Command-Line Module Before The Index Commands
workstream: "0006"
kind: task
depends_on: []
gated: false
touches:
  - crates/froe-cli/src/command_line.rs
  - crates/froe-cli/src/command_line/tests.rs
status: planned
merged_as: ""
---
# Split The Command-Line Module Before The Index Commands

`crates/froe-cli/src/command_line.rs` is 980 lines, twenty short of the thousand-line gate `scripts/oversized-files.sh` enforces in CI, and its `mod tests` starts at line 528. Two tasks of this plan add to it (0613, 0611), the first of which would trip the gate; the four later command tasks (0710, 0803, 0807, 1008) extend `command_line/index.rs`, which 0611 creates in the room this split makes. `CONTRIBUTING.md` keeps such a move in a `refactor:` commit apart from the behaviour change it serves, so this task does only the move: the module's tests go to `crates/froe-cli/src/command_line/tests.rs`, declared `#[cfg(test)] mod tests;` in `command_line.rs` (a file module with a directory of the same name beside it, which the 2018 edition allows), leaving about 530 lines of production code and the headroom the `Index` variant of task 0611 and the flag task 0613 adds need. No behaviour changes; every command-line test passes unmodified.

**Steps:**

1. Move `mod tests` from `command_line.rs` into `command_line/tests.rs` with `use super::*;`, declared `#[cfg(test)] mod tests;` at the old position.
2. Run the command-line tests and the workspace gate unchanged.

- **Done when:** `command_line.rs` is under 600 lines, every test that ran before runs unchanged and passes, `scripts/oversized-files.sh` reports no offender, and the stable host gate passes.
