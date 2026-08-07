# Plan authoring contract

Makina executes validated plan directories, identified by their repository-relative directory path.

## Layout

```text
docs/plans/
├── STATUS.md
└── NNNN-ASCII-Slug/
    ├── SCOPE.md
    ├── ARCHITECTURE.md
    ├── STATUS.md
    └── tasks/
        └── WWSS-task-id.md
```

## Task document contract

Each task file has closed YAML frontmatter (`id`, `title`, four-digit `workstream`, `kind`, `depends_on`, `gated`, `touches`, `status`, and `merged_as`) followed by an exact title, ordered `**Steps:**`, and one falsifiable `- **Done when:**` criterion. Filenames, IDs, workstreams, dependencies, and repository-relative mutation footprints are validated as one DAG.

Task lifecycle fields, landing OIDs, plan progress/integration evidence, and the root roll-up are coordinator-owned. Volatile checkpoints live outside the repository and cannot establish completion. A committed bundle is `Unregistered` until exact Phase R binds its validation base; after registration, dependency-ready ungated tasks are `Ready`. Working-tree-only bundles are `AwaitingCommit`.

Historical monolithic task-list plans are inert records, not executable inputs. Do not create a fallback or mixed-format plan.
