# Read-Only Tooling Specification (Check, Diff, Revisions, History, SearchNodes, Debug*)

Scope: the remaining **read-only** oak-run segment tools, specified to the level needed for
**output-compatible** (CLI-parity) Rust ports. Storage formats (tar, segment, record, node,
journal) are specified in the companion documents and are not repeated here:

- `tar-layer.md` — tar file layout, index, graph, binary references
- `segment-layer.md` — segment header, record table, references
- `record-layer.md` — record encodings, record ids
- `node-layer.md` — node/template/map semantics, `NodeState` comparison
- `filestore-layer.md` — `journal.log` line format, manifest, store opening
- `tooling-inventory.md` §3 — one-paragraph functional summaries of these tools

All Java paths are relative to
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/` unless prefixed with
`oak-run/` or another module.

---

## 0. Shared infrastructure

### 0.1 Store opening

Two distinct opening paths exist; they differ in defaults and must be reproduced per tool:

1. **`tool/Utils.java` — `openReadOnlyFileStore(File)`** (used by `DebugTars`,
   `DebugSegments`, `DebugStore`):
   ```java
   fileStoreBuilder(isValidFileStoreOrFail(path))
       .withSegmentCacheSize(TAR_SEGMENT_CACHE_SIZE)      // sysprop "cache", default 256 (MB)
       .withMemoryMapping(TAR_STORAGE_MEMORY_MAPPED)      // sysprop "tar.memoryMapped", default false
       .buildReadOnly();
   ```
   `isValidFileStore(File)` (`Utils.java`) requires: path exists, is a directory, and its
   direct listing contains a file named exactly `journal.log`. Otherwise
   `checkArgument` throws `IllegalArgumentException("Invalid FileStore directory " + store)`,
   which every caller catches and turns into a stack trace on stderr + exit code 1.

2. **Plain `fileStoreBuilder(dir).buildReadOnly()`** (used by `Check` — with
   `withCustomPersistence(new TarPersistence(path, journal))` and explicit
   `withMemoryMapping` — by `RevisionHistory`, by `SearchNodes.newFileStore()`, and by
   `FileStoreDiffCommand` — with `withBlobStore(newBasicReadOnlyBlobStore())`). No
   segment-cache/mmap system properties are honored on this path.

`ReadOnlyFileStore` (`file/ReadOnlyFileStore.java`, constructor): verifies the manifest via
`newManifestChecker(persistence, strictVersionCheck).checkManifest()` — **read-only check**
(`file/ManifestChecker.java` `checkManifest()` only reads/validates; only
`checkAndUpdateManifest()`, used by the read-write store, writes). It builds `TarFiles` with
`.withReadOnly()` and does **not** acquire the repository lock (`repo.lock` is only taken by
the read-write `FileStore`). Extra tooling API (`ReadOnlyFileStore.java` lines 105–165):

- `setRevision(String)` — `RecordId.fromString(tracker, revision)` then
  `revisions.setHead(currentHead, newHead)`; `ReadOnlyRevisions.setHead` (lines 88–92) is a
  pure in-memory CAS, nothing is persisted. `fromString` throws `IllegalArgumentException`
  on malformed input (`RecordId.java` line 68: `"Bad record identifier: " + id`).
- `getTarReaderIndex()` → `TarFiles.getIndices()` (`file/tar/TarFiles.java` line 924): map
  *tar reader file name* → set of segment UUIDs in that tar.
- `getTarGraph(String fileName)` → `TarFiles.getGraph` (line 898): for the reader whose file
  name equals the argument, map of every UUID in the tar → its graph edges.
- `getSegmentIds()` — all UUIDs from all tar indices, as `SegmentId`s.

**Read-only recovery caveat** (`file/tar/TarReader.java` `openRO`, lines 122–145): a
read-only open tries, per tar, only the **highest generation** file (`files.get(max(key))`,
"to prevent any rollback or rewrite"), with three strategies in order: `open` (valid index),
`forceOpen`, and `recoverAndOpen`. The last one *does write*: it collects entries without
touching the original file and writes them to a fresh archive named
`<name>.ro.bak` (or `<name>.2.ro.bak`, `<name>.3.ro.bak`, … — `findAvailGen`, lines
244–250), then opens that. So even Oak's "read-only" tools may create `*.ro.bak` files in
the store directory when a tar has no valid index and cannot be force-opened.

### 0.2 Journal iteration

`file/JournalReader.java`: iterates `journal.log` **in reverse order — newest entry first**
(class javadoc: "Iterator over the revisions in the journal in reverse order"). Per line
(`computeNext()`):

- lines without a space (`line.indexOf(' ') == -1`) are skipped with a log warning;
- otherwise split on single spaces (`line.split(" ", -1)`); token 0 = revision string,
  token 2 (if present) parsed as decimal long timestamp; parse failure or absence ⇒
  timestamp `-1L` (plus log warning). Result: `JournalEntry(revision, timestamp)`
  (`file/JournalEntry.java`).

`tool/Utils.readRevisions(File)` (lines 72–86): opens `<store>/journal.log` (hardcoded
name), maps each `JournalEntry` to its revision string, returns the list (newest first).
Missing journal or exception ⇒ empty list (exception also stack-traced to stderr).

### 0.3 Record-id string forms (`RecordId.java`)

- `toString()` (line 129): `String.format("%s.%08x", segmentId, offset)` —
  `<uuid>.<8-hex-digit record number>`. `SegmentId.toString()` is the canonical lowercase
  UUID (`msb`/`lsb`).
- `toString10()` (line 136): `String.format("%s:%d", segmentId, offset)` —
  `<uuid>:<decimal record number>` ("Oak 1.0" form; this is the form used in
  `journal.log`).
- `fromString(SegmentIdProvider, String)` (lines 43–70) accepts **both** forms via regex
  `([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(:(0|[1-9][0-9]*)|\.([0-9a-f]{8}))`
  (decimal after `:`, or exactly 8 hex digits after `.`). Note: uppercase hex/UUIDs are
  rejected; decimal must not have leading zeros.

### 0.4 Text-format helpers

- **`MessageFormat.format`** (used by `Check`/`CheckHelper` for *all* normal output):
  numeric arguments are rendered with the default locale's `NumberFormat`, i.e. **with
  grouping separators** ("Searched through 1,234 revisions" in an English locale). A
  byte-exact port must reproduce grouping (or pin the locale).
- **`IOUtils.humanReadableByteCount(long)`** (oak-commons `IOUtils.java`): negative ⇒ `"0"`;
  `< 1000` ⇒ `"<n> B"`; else `String.format(Locale.ENGLISH, "%.1f %sB", bytes/1000^exp, pre)`
  where `exp = (int)(log(bytes)/log(1000))` and `pre = "kMGTPE".charAt(exp-1)` — e.g.
  `"1.2 MB"` (decimal units, one fraction digit, English locale).
- **`FileUtils.byteCountToDisplaySize(long)`** (Apache commons-io; used by `PrintingDiff`,
  `DebugStore`, `RecordUsageAnalyser`): binary units with integer floor division —
  `"<n> bytes"` below 1024, then `"<n> KB"`, `"<n> MB"`, `"<n> GB"`, `"<n> TB"`, `"<n> EB"`
  (no fraction digits; e.g. 1535 bytes ⇒ `"1 KB"`).
- **`Type.toString()`** (oak-api `Type.java`): the fixed names `STRING`, `BINARY`, `LONG`,
  `DOUBLE`, `DATE`, `BOOLEAN`, `NAME`, `PATH`, `REFERENCE`, `WEAKREFERENCE`, `URI`,
  `DECIMAL` and plural forms `STRINGS`, `BINARIES`, `LONGS`, … for arrays.
- **`AbstractPropertyState.toString(PropertyState)`** (oak-store-spi): `BINARIES` ⇒
  `name + " = [" + count + " binaries]"`; `BINARY` ⇒ `name + " = {" + size + " bytes}"`;
  else `name + " = " + value` (array values render as Java list `[a, b]`). Its
  `getBinarySize` catches every exception from `PropertyState.size()` and returns `-1`, so a
  scalar external binary without a blob store or a corrupt value marker renders exactly
  `{-1 bytes}` rather than failing the diagnostic. By contrast, invalid LONG/DOUBLE text
  reaches `Conversions.convert(value, base)` and its numeric parser, whose exception escapes
  `DebugTars` and fails the command.
- **`AbstractNodeState.toString(NodeState)`** (oak-store-spi, line ~200): non-existent ⇒
  `"{N/A}"`; else `{ prop1, prop2, child1 : {...}, ... }` — properties first, then child
  entries capped at `CHILDREN_CAP` = sysprop `oak.children.cap`, default 100 (then
  `"..."`). Child entry (`AbstractChildNodeEntry.toString`): leaf child ⇒
  `name + " : " + state`, child with children ⇒ `name + " = { ... }"`.

---

## 1. Check — oak-run `check`

Wiring: `oak-run/src/main/java/org/apache/jackrabbit/oak/run/CheckCommand.java`; engine:
`tool/Check.java` + `tool/check/CheckHelper.java` +
`file/tooling/ConsistencyChecker.java`.

### 1.1 Option surface (CheckCommand.execute)

| Option | Arg | Default | Effect |
|---|---|---|---|
| *(non-option)* | path | required, exactly one | Store path. `az:` prefix routes to AzureCheck (out of scope). Zero paths ⇒ `"Segment Store path not specified"`; >1 ⇒ `"Too many Segment Store paths specified"`, then `"usage: check path/to/segmentstore <options>"` + jopt help, all on **stderr**, exit 1. |
| `--mmap` | optional boolean | `true` | Memory mapping. |
| `--journal` | required arg | `<path>/journal.log` (`Check.journalPath`) | Journal file. |
| `--notify` | seconds (long) | `Long.MAX_VALUE` | Debug print interval (`debugInterval`). |
| `--last` | optional int | *absent* ⇒ `Integer.MAX_VALUE` (`Check.revisionsToCheckCount`); `--last` with no value ⇒ `1`; `--last N` ⇒ N | Max revisions to check. |
| `--bin` | flag | off | Fully read binary property streams. |
| `--filter` | comma-separated | `"/"` | Content paths to check (`LinkedHashSet`, order preserved). |
| `--head` | flag | — | See interplay below. |
| `--checkpoints` | optional comma-separated | `"all"` | See interplay below. |
| `--io-stats` | flag | off | Print I/O statistics. |
| `--fail-fast` | optional boolean | `false` | Stop at first inconsistency. |

Head/checkpoint interplay (`CheckCommand.shouldCheckHead` / `toCheckpointsSet`):

- neither `--head` nor `--checkpoints`: head **and** all checkpoints (`{"all"}`);
- `--head` only: head, no checkpoints (empty set);
- `--checkpoints …` only: checkpoints only, **no head**;
- both: head + given checkpoints.

`Check.run()` builds the store with
`fileStoreBuilder(path).withMemoryMapping(mmap).withCustomPersistence(new TarPersistence(path, journal))`,
attaches a `StatisticsIOMonitor` when `--io-stats` (counts `afterSegmentRead` ops/bytes/ns),
then delegates to `CheckHelper.run(store, journalReader)`. Any exception ⇒ stack trace to
the error writer, return 1 (`Check.run` lines 394–415). Exit code = returned value via
`System.exit` in `CheckCommand`.

### 1.2 Algorithm

`CheckHelper.run` (`tool/check/CheckHelper.java` lines 228–272):

1. If the requested checkpoint set contains the literal `"all"`, it is replaced by the
   actual checkpoint ids: `SegmentNodeStoreBuilders.builder(store).build().checkpoints()`
   (read from the *current head* super-root's `checkpoints` node, in stored child order,
   into a `LinkedHashSet`).
2. Delegates to `ConsistencyChecker.checkConsistency(store, journal, checkHead,
   checkpoints, filterPaths, checkBinaries, revisionsCount, failFast)`.

`ConsistencyChecker.checkConsistency` (`file/tooling/ConsistencyChecker.java` lines
354–449), language-neutral:

```
headPaths      : list of PathToCheck, one per filter path (only if checkHead)
checkpointPaths: map checkpointId -> list of PathToCheck (one per filter path)
PathToCheck    : { path, journalEntry = null, corruptPaths = ordered set }

lastValid = null; checked = 0
for entry in journal (NEWEST first):                 # JournalReader order
    revision = entry.revision
    try:
        checked += 1
        store.setRevision(revision)                  # in-memory head switch
        emit "\nChecking revision {revision}"        # onCheckRevision
        overall = checkHeadConsistency(...)
        if any checkpoint PathToCheck still unassigned:
            emit "\nChecking checkpoints"            # onCheckChekpoints (sic)
            overall = overall && checkCheckpointsConsistency(...)
        if overall: lastValid = entry
        elif failFast: break
        if every PathToCheck has a journalEntry: break
        if checked == revisionsCount: break
    catch IllegalArgumentException | SegmentNotFoundException as e:
        emit-err "Skipping invalid record id {revision}: {e}"   # e rendered via toString
        if failFast: break
```

Per-revision head check (`checkHeadConsistency`, lines 260–269): skipped entirely (returns
true) when all head paths already have an assigned journal entry; otherwise emits
`"\nChecking head\n"` and checks each unassigned path against `store.getRoot()` (the
`root` child, i.e. normal content root).

Per-path check (`checkPathConsistency` → `checkTreeConsistency`, lines 218–246):

1. **Re-probe known corrupt paths first** (`findFirstCorruptedPathInSet`): for each path in
   `ptc.corruptPaths` (insertion order), resolve it under the current root
   (`NodeStateUtils.getNode`); a missing node emits `"Path {p} not found"` (stderr) and
   counts as still corrupt; an existing node is checked **shallowly** (`checkNode`: this
   node's properties only, no recursion). If any probe fails, the whole path is still
   inconsistent at this revision and the full traversal is skipped.
2. Otherwise emit `"Checking {path}"` (`onCheckTree`; resets per-tree node/property
   counters) and run the **full recursive traversal** (`findFirstCorruptedPathInTree` →
   `checkNodeAndDescendants`): depth-first, children in `getChildNodeEntries()` order,
   parent's properties checked before descending. First inconsistent path is returned;
   traversal errors emit `"Error while traversing {path}: {message}"` (tree level,
   `e.getMessage()`) or `"Error while traversing {path}: {e}"` (node level, exception
   `toString`) on stderr. Then `"Checked {n} nodes and {m} properties"` (`onCheckTreeEnd`).
3. On success: emit `"Path {p} is consistent"` and pin `ptc.journalEntry = entry` — this is
   the per-path **latest good revision** (first success wins because iteration is
   newest-first; the path is never re-checked). On failure: add the corrupt path to
   `ptc.corruptPaths`.

Node check (`checkNode`, lines 468–497): for each property, `getValue(type)` forces full
decode; `BINARY`/`BINARIES` values are traversed via `traverse(blob, checkBinaries)` —
only when `--bin` is set and the blob is not external (`SegmentBlob.isExternal()`), the
entire stream is read in 8 KiB chunks. Property count increments per checked property
(binaries only count when actually read). Any `RuntimeException`/`IOException` marks the
node's path corrupt.

Checkpoint check (`checkCheckpointConsistency`, lines 271–288): emits
`"\nChecking checkpoint {id}"`; root = `SegmentNodeStore.retrieve(id)` (the checkpoint's
`root` child); if null ⇒ `"Checkpoint {id} not found in this revision!"` (stderr) and
fails; otherwise same per-path logic with `head=false`.

**Semantics of "overall"**: `lastValid` is set at any revision where *that iteration's*
checks all passed — including short-circuited already-verified paths — so the final
`overallRevision` is the revision at which the last outstanding path was verified (the
oldest of the per-path latest-good revisions along the walk).

### 1.3 Result report (stdout unless noted)

`CheckHelper.run` lines 246–271; all via `MessageFormat` (§0.4):

```
\nSearched through {checkedRevisionsCount} revisions and {checkpoints.size} checkpoints
```

Then, if a good revision was found — `failFast` off: **any** head-path or checkpoint-path
got a revision; `failFast` on: **all** of them did (`isGoodRevisionFound`, lines 282–284,
377–406):

```
(blank)Head                                    | only if checkHead
{for each filter path, iteration order of HashMap<String,Revision>}
Latest good revision for path {path} is {revision} from {timestamp}
(blank)Checkpoints                             | only if checkpoints non-empty
- {checkpointId}
  Latest good revision for path {path} is {revision} from {timestamp}   | 2-space indent
(blank)Overall
Latest good revision for paths and checkpoints checked is {revision} from {timestamp}
```

`revision` falls back to `"none"` and timestamp to `"unknown time"` when null; timestamp
formatting is `DateFormat.getDateTimeInstance().format(new Date(ms))` — default locale
medium date+time (`CheckHelper.printRevision`/`timestampToString`, lines 408–430). Note
the "Head"/"Checkpoints" maps are `HashMap`s, so multi-path ordering is hash order (a port
may document insertion order instead — Oak's own order is unspecified). Return code 0.

Otherwise: `No good revision found`, return code 1.

If `--io-stats` (`Check.run` lines 400–404):

```
[I/O] Segment read: Number of operations: {ops}
[I/O] Segment read: Total size: {humanReadableByteCount} ({bytes} bytes)
[I/O] Segment read: Total time: {ns} ns
```

Debug traces (`--notify`): `"Traversing {path}"` per node and `"Checked {path}/{property}"`
per property, printed at most once per `debugInterval` seconds (`CheckHelper.debug()`,
lines 440–463; `--notify 0` prints always).

### 1.4 Exit codes

- 0 — good revision found (per §1.3 predicate);
- 1 — no good revision, any exception during run, or usage error.

---

## 2. Diff and Revisions — oak-run `tarmkdiff`

Wiring: `oak-run/.../run/FileStoreDiffCommand.java` (registered as `tarmkdiff` in
`AvailableModes.java` line 75); engines `tool/Diff.java`, `tool/PrintingDiff.java`,
`tool/Revisions.java`.

### 2.1 Option surface (FileStoreDiffCommand.execute)

| Option | Arg | Default | Effect |
|---|---|---|---|
| *(non-option)* | path | required | Store path; missing ⇒ help on stdout, exit 1. |
| `-h`/`-?`/`--help` | flag | — | Help on stdout, exit 0. |
| `--output` | file | `diff_<currentTimeMillis>.log` in cwd | Output file. |
| `--list` | flag | off | List revisions instead of diffing. |
| `--diff` | `R0..R1` | none | Revision interval; `head` (any case) = current journal head. Effectively required in diff mode: `Diff.Builder.build()` does `requireNonNull(interval)` ⇒ NPE. |
| `--incremental` | flag | off | Diff every intermediate journal revision pair. |
| `--path` | path | `"/"` | Subtree filter. |
| `--ignore-snfes` | flag | off | Continue an incremental diff run across `SegmentNotFoundException`s. |

Store is opened with `fileStoreBuilder(new File(path)).withBlobStore(newBasicReadOnlyBlobStore()).buildReadOnly()`
(a `BasicReadOnlyBlobStore` so external blob ids resolve to length-less stubs). Exit code =
tool return code via `System.exit`.

### 2.2 `--list` mode (`tool/Revisions.java`)

stdout: `Store {path}`, `Writing revisions to {out}`. Reads `Utils.readRevisions(path)`
(§0.2; **newest first**). Empty ⇒ stdout `No revisions found.` and return 0 without
touching the output file. Otherwise writes one revision string per line (as they appear in
`journal.log`, i.e. `uuid:decimal` form) to the output file. Exceptions ⇒ stack trace to
stderr, return 1.

### 2.3 Diff mode (`tool/Diff.java` `diff()`, lines 230–308)

1. stdout: `Store {path}`, `Writing diff to {out}`.
2. `interval.trim().split("\\.\\.")` must yield exactly 2 tokens, else stdout
   `Error parsing revision interval '{interval}'.` and **return 0** (quirk: parse errors
   exit successfully).
3. Each endpoint: literal `head` (case-insensitive) ⇒ `store.getRevisions().getHead()`;
   else `RecordId.fromString` (§0.3). On `IllegalArgumentException` stdout
   `Invalid left endpoint for interval {interval}` — **for both endpoints** (the
   right-endpoint branch repeats the "left" message verbatim, `Diff.java` line 264) — and
   return 0.
4. Non-incremental: stdout `Generating diff between {idL} and {idR}` (record ids in
   `toString()` dot-hex form), then one diff into the output file (see 2.4).
5. Incremental: `revs = processor.process(path)` (= `Utils.readRevisions`, newest first);
   stdout `Generating diff between {idL} and {idR} incrementally. Found {n} revisions.`
   Endpoints are located by `revs.indexOf(id.toString10())` — **exact string match** in
   `uuid:decimal` form against journal lines; not found ⇒ stdout
   `Unable to match input revisions with FileStore.`, return 0. Take the inclusive
   sublist between the two indices, reversing it when the left endpoint is newer, so
   diffs run left→right as given; fewer than 2 entries ⇒ stdout
   `Nothing to diff: {list}`, return 0. Then diff each consecutive pair; on a diff
   returning false (SNFE), stop unless `--ignore-snfes`.
6. stdout: `Finished in {ms} ms.` Return 0 (return 1 only if an exception escapes, with
   stack trace on stderr — `Diff.run`, lines 220–228).

### 2.4 Single diff into the output file (`Diff.diff(store, idL, idR, pw)`, lines 310–326)

```
rev {idL}..{idR}                    # RecordId.toString dot-hex forms
```

then `before = readNode(idL).getChildNode("root")`, `after = readNode(idR).getChildNode("root")`
(the ids are **super-root** records, as found in journal lines; the diff runs on the
content root), both descended by each element of the `--path` filter, then
`after.compareAgainstBaseState(before, new PrintingDiff(pw, filter))`. On
`SegmentNotFoundException`: `ex.getMessage()` to stdout and a line `#SNFE {segmentId}` to
the output file; the pair contributes false.

### 2.5 PrintingDiff output grammar (`tool/PrintingDiff.java`)

For each event (in `compareAgainstBaseState` callback order — property events of a node
before its child-node events):

| Event | Line(s) |
|---|---|
| property added | `    + {P}` |
| property changed | `    ^ {name}` then `      - {P_before}` then `      + {P_after}` |
| property deleted | `    - {P}` |
| child node added | `+ {path}` then full recursion of the added subtree against `EMPTY_NODE` (all its properties/descendants appear as additions) |
| child node changed | `^ {path}` then recursion |
| child node deleted | `- {path}` then recursion of `MISSING_NODE` vs. before (whole removed subtree appears as deletions) |

`path` is `concat(parentPath, name)` starting from the `--path` filter string. Indentation
is fixed (4 spaces for property lines, 6 for the changed-property value pair) regardless of
depth.

Property rendering `P` = `PrintingDiff.toString(PropertyState)` (lines 108–123). **Quirk to
reproduce byte-for-byte**: the method builds `val = name<TYPE>…` and then returns
`name + "<" + TYPE + ">" + val`, so *name and type appear twice*:

- single non-binary: `name<TYPE>name<TYPE> = value` (value = `getValue(STRING)` conversion)
- non-binary array: `name<TYPES>name<TYPES>[count] = [v1, v2, ...]`
  (`getValue(STRINGS)` iterable, Java collection toString)
- `BINARY`: `name<BINARY>name<BINARY> = {size}` where size =
  `byteCountToDisplaySize(blob.length())` (§0.4) or `[N/A]` if length is unavailable
  (`IllegalStateException`, e.g. missing blob store)
- `BINARIES`: `name<BINARIES>name<BINARIES>[count] = [size1, size2, ...]`

---

## 3. History — oak-run `history`

Wiring: `oak-run/.../run/HistoryCommand.java`; engines `tool/History.java`,
`file/tooling/RevisionHistory.java`.

### 3.1 Options

| Option | Arg | Default |
|---|---|---|
| *(non-option)* | store dir (File) | required; missing ⇒ stderr `Trace the history of a path. Usage: history [File] <options>` + help, `System.exit(-1)` (process status 255) |
| `--journal` | file name | `"journal.log"` — resolved as `new File(store, journalName)` after `FileStoreHelper.isValidFileStoreOrFail(directory)` |
| `--path` | node path | `"/"` |
| `--depth` | int ≥ 0 | `0` |

### 3.2 Algorithm

`RevisionHistory` opens `fileStoreBuilder(directory).buildReadOnly()` (plain defaults).
`getHistory(journal, path)` (lines 76–87): for each journal entry (**newest first**,
§0.2): `store.setRevision(entry.getRevision())`, take `store.getHead()` — the
**super-root** `SegmentNodeState` — and descend `path` elements via `getChildNode`
(non-existent path yields a non-existent node, serialized as `{}` — note `/root` prefix
is *not* implied; `--path /root/content` addresses the content tree, and the default `/`
shows the super-root with its `root`/`checkpoints` children).

`History.run` prints one line per entry to stdout: `HistoryElement.toString(depth)`
(`RevisionHistory.java` lines 124–129):

```
{revision}={json}
```

where `json` = `new JsonSerializer(depth, 0, Integer.MAX_VALUE, DEFAULT_FILTER_EXPRESSION,
new BlobSerializer())` serialization of the node.

### 3.3 JSON serialization semantics (oak-store-spi `json/JsonSerializer.java`)

- Compact JSOP-builder JSON: `{"key":value,...}`, no whitespace, keys/strings quoted with
  JSON escaping.
- Properties first (iteration order of `getProperties()`), then children. The filter
  `{"properties":["*", "-:childNodeCount"]}` includes every property except one literally
  named `:childNodeCount`.
- Child order: if the node has a `:childOrder` property, that order (missing children
  skipped); else `getChildNodeEntries()` order.
- Depth: a child at depth limit (`depth == 0` in the current serializer) is emitted as
  `"name":{}`; otherwise recursion with `depth - 1` (`serialize(NodeState)`, lines
  160–170). So `--depth 0` yields properties of the target node plus `{}` stubs for each
  child.
- Property values (`serialize(PropertyState, Type, index)`, lines 271–293):
  - `BOOLEAN` → `true`/`false`; `LONG` → unquoted decimal;
  - `DOUBLE` → unquoted `Double.toString` value, except NaN/±Infinity which become the
    type-coded string `"dou:NaN"` etc.;
  - `BINARY` → `":blobId:" + BlobSerializer.serialize(blob)"`; the default
    `BlobSerializer` (oak-store-spi `json/BlobSerializer.java` line 27) renders
    `"Blob{" + blob + "}"` (for a `SegmentBlob` the inner `toString` is its content
    identity string);
  - everything else → the `STRING` conversion, prefixed with the 3-letter lowercase type
    code + `:` when the type is not `STRING` (`TypeCodes.encode`: `nam:`, `pat:`, `dat:`,
    `ref:`, `wea:`, `uri:`, `dec:`, …) — and a plain `STRING` value is *also* prefixed
    with `str:` if it would otherwise itself parse as type-coded
    (`TypeCodes.split(value) != -1`, i.e. starts with `":blobId:"` or has `:` at index 3);
  - arrays → `[v1,v2,...]`; an empty non-STRING array → the single string
    `"[0]:<TypeName>"` (e.g. `"[0]:Name"`, via `TypeCodes.EMPTY_ARRAY` +
    `PropertyType.nameFromValue`).

### 3.4 Exit codes

0 on success; 1 if `History.run` catches an exception (stack trace to stderr); 255 (-1)
usage error.

---

## 4. SearchNodes — oak-run `search-nodes`

Wiring: `oak-run/.../run/SearchNodesCommand.java` (registered as `search-nodes`); engine
`tool/SearchNodes.java`.

### 4.1 Options

| Option | Arg | Default | Notes |
|---|---|---|---|
| *(non-option)* | path | required, exactly one | 0 ⇒ stderr `Segment Store path not specified`, exit 1; >1 ⇒ stderr `Too many Segment Store paths specified`, exit 1. |
| `-p`/`--property` | name | repeatable | node must have property `name`. |
| `-c`/`--child` | name | repeatable | node must have child `name`. |
| `-v`/`--value` | `name=value` | repeatable | split on `=`, must give exactly 2 parts else stderr `Invalid property value specified: {v}`, exit 1 (note: a value containing `=` is rejected). |
| `-o`/`--output` | `text` \| `journal` | `text` | anything else ⇒ stderr `Unrecognized output: {v}`, exit 1. |
| `-h`/`--help` | flag | — | help on stdout, exit 0. |

All matchers AND together; with no matchers every node record matches.

### 4.2 Algorithm (`SearchNodes.run`, lines 152–226)

Open `FileStoreBuilder.fileStoreBuilder(path).buildReadOnly()` (no `Utils` sysprops, no
journal-existence validation). For every `SegmentId` in the tar indices
(`fileStore.getSegmentIds()`):

1. Skip bulk segments.
2. `timestamp = parseSegmentInfoTimestamp(segmentId)` (`tool/Utils.java` lines 116–142):
   parse the segment-info JSON string and read property `"t"` as decimal long. Absent
   info/`t`/unparsable ⇒ stderr `No timestamp found in segment {segmentId}\n` and skip the
   segment.
3. For every entry in the segment's record table (`Segment.forEachRecord`, record-table
   order) with `type == NODE`: read `SegmentNodeState` at `RecordId(segmentId, number)`
   and evaluate matchers (value matcher: array property ⇒ any element string-equals;
   single ⇒ `getValue(STRING).equals(value)`; missing property ⇒ false).
4. Matching node, per output mode (`processRecord`, lines 209–219):
   - `TEXT`: `printf("%d\t%s\n", timestamp, recordId)` — timestamp, TAB, dot-hex record id;
   - `JOURNAL`: `printf("%s root %d\n", recordId.toString10(), timestamp)` — a
     syntactically valid `journal.log` line (this output can be used as a recovered
     journal).

`SegmentNotFoundException` during segment/record processing: stack trace to stderr **once
per distinct segment id** (`handle`, dedup via `notFoundSegments`), processing continues.
Return 0 unless a non-SNFE exception escapes (stack trace, return 1).

---

## 5. Debug tools — oak-run `debug`

Wiring: `oak-run/.../run/DebugCommand.java`. Dispatch (lines 48–83): first non-option is
the store path; remaining args ending in `.tar` go to `DebugTars`, all others to
`DebugSegments`; **no extra args** ⇒ `DebugStore`. No args at all ⇒ stderr
`usage: debug <path> [id...]`, exit 1. Exit code 1 if any sub-tool returned nonzero, else
0. All three open the store via `Utils.openReadOnlyFileStore(path)` (§0.1) and on any
exception print a stack trace to stderr and return 1.

### 5.1 DebugTars (`tool/DebugTars.java`)

Per tar name `t` (as given on the command line, must end `.tar`):

- If `new File(path, t)` doesn't exist: `file doesn't exist, skipping {t}`.
- Header: `Debug file {new File(path, t)}({length})`. `File.toString()` preserves the
  supplied path shape, so a relative store argument produces a relative header; this path is
  neither made absolute nor canonicalized.
- Find the tar reader index entry whose key `endsWith(t)` (keys are reader file names).
  If found: `SegmentNodeState references to {t}` followed by reference paths (below), each
  prefixed with two spaces; else `No references to {t}`.
- Reference scan (`filterNodeStates`, lines 188–240): DFS from `store.getHead()` (the
  **super-root**) with path `"/"`; per node collects into a per-node `TreeSet`
  (lexicographic within one node, DFS across nodes):
  - property whose value record lives in the tar:
    - STRING type: `{path}{name} = {display} [SegmentPropertyState<{TYPE}>@{recordId}]`,
      where `display` = first value only, truncated to sysprop `max.char.display`
      (default 60) Java UTF-16 code units using `String.length()` and `substring`, with
      `... ({len} chars)` carrying the full UTF-16 length, then Java-escaped
      (`StringEscapeUtils.escapeJava`) and double-quoted. A substring can therefore end
      between a supplementary character's two surrogate code units;
    - other types: `{path}{propertyToString} [SegmentPropertyState<{TYPE}>@{recordId}]`
      (property via `AbstractPropertyState.toString`, §0.4);
  - BINARY properties additionally match when any of the blob's bulk segment ids
    (`SegmentBlob.getBulkSegmentIds`) is in the tar (same non-string line format).
    `getBulkSegmentIds` inserts every long-value block record's segment id into a
    `HashSet` without requiring the segment kind to be bulk. The set is built eagerly,
    so every block-list entry is resolved even after an earlier segment would match;
    `DebugTars` then excludes the property record's own segment id before matching the tar;
  - node record in the tar: `{path} [SegmentNodeState@{recordId}]`;
  - template record in the tar: `{path}[Template@{recordId}]` (**no space** before `[` —
    quirk, line 231).
  Paths accumulate as `parentPath + name + "/"` (so every printed node path ends with a
  child-relative prefix ending in `/` for children; the root is `/`).
- Then a blank line, `Tar graph:`, and per index-map entry `"{uuid}={setOfUuids}"`
  (`java.util.UUID` toString and `Set` toString `[a, b]`; `HashMap` iteration order). A
  failure reading the graph prints `Error getting tar graph:` + stack trace to stderr.
  Stored graph parsing gives each row's targets set semantics and inserts rows with
  `Map.put(source, targets)`, so a duplicate source row replaces the preceding row
  (last row wins). `TarFiles.getGraph` emits an empty set for an indexed segment with
  no stored source row.

### 5.2 DebugSegments (`tool/DebugSegments.java`)

Each argument is matched against
`([0-9a-f-]+)|(([0-9a-f-]+:[0-9a-f]+)(-([0-9a-f-]+:[0-9a-f]+))?)?(/.*)?` (line 47); no
match ⇒ stderr `Unknown argument: {segment}`.

1. **Segment id** (group 1, a bare UUID): prints `id.getSegment().toString()` =
   `SegmentDump.dumpSegment` (`SegmentDump.java` lines 40–64):

   ```
   Segment {segmentId} ({length} bytes)
   Info: {segmentInfoJson}, Generation: {gcGeneration}      | only when info present
   --------------------------------------------------------------------------
   reference 01: {referencedSegmentId}                      | %02x counter from 1
   ...
       {TYPE} record {number:08x}: {offset:08x} @ {address:08x}   | "%10s record %08x: %08x @ %08x"
   ...
   --------------------------------------------------------------------------
   {hex dump of the raw segment data}
   --------------------------------------------------------------------------
   ```

   `address = length - (MAX_SEGMENT_SIZE - offset)` (the record's file position within the
   stored segment; `MAX_SEGMENT_SIZE` = 262144, see segment-layer.md). References and the
   record table are printed only for data segments. Line ends inside the dump use `%n`
   (platform separator).

2. **Record id, optional path** (`uuid:recno[/path]`, also `uuid.hex8` via
   `RecordId.fromString`): with no id given the head record id is used. Prints:

   ```
   / ({recordId}) -> {nodeToString}
     {name} ({childRecordIdOrNull}) -> {childToString}     | per path element, 2-space indent
   ```

   node rendering via `AbstractNodeState.toString` (§0.4); `childRecordIdOrNull` is the
   `SegmentNodeState` record id or `null` for non-segment nodes (e.g. missing).

3. **Record range** (`id1-id2[/path]`): reads both nodes, descends the path on both, prints
   `JsopBuilder.prettyPrint(JsopDiff.diffToJsop(node1, node2))` — the JSOP diff
   (oak-store-spi `json/JsopDiff.java` lines 92–137):
   `^"path":value` for added/changed properties (value serialized like §3.3 with default
   `BlobSerializer`), `^"path":null` for deleted properties, `+"path":{full JSON subtree}`
   for added nodes, `-"path"` for deleted nodes, recursion into changed nodes (paths are
   absolute, built from `/`). `prettyPrint` re-indents the JSOP with newlines/spacing.

### 5.3 DebugStore (`tool/DebugStore.java` `debugFileStore`, lines 119–171)

1. Iterate all segment ids; count data/bulk segments and their sizes; for each data
   segment run `RecordUsageAnalyser.analyseNode` on every NODE record (record-table
   order); per-node analysis errors print stderr `Error while processing node at {id}`
   (no newline — `System.err.format` without `%n`) + stack trace.
2. Output:

   ```
   Total size:
   {byteCountToDisplaySize(dataSize)} in {dataCount%6d} data segments
   {byteCountToDisplaySize(bulkSize)} in {bulkCount%6d} bulk segments
   {analyser.toString()}
   ```

   `RecordUsageAnalyser.toString()` (lines ~186–210):

   ```
   {size} in maps ({n} leaf and branch records)
   {size} in lists ({n} list and bucket records)
   {size} in values (value and block records of {n} properties, {a}/{b}/{c}/{d} small/medium/long/external blobs, {e}/{f}/{g} small/medium/long strings)
   {size} in templates ({n} template records)
   {size} in nodes ({n} node records)
   links to non existing segments: {n}
   ```

   (sizes via `byteCountToDisplaySize`; counts as plain longs; `%n` line endings; the last
   line has no trailing newline of its own — `System.out.println` adds it.)
3. Segment-level reachability: BFS from the head record's segment through
   `getReferencedSegmentId` edges of data segments (bulk segments have no outgoing edges);
   unreachable segments are "garbage":

   ```
   (blank line)Available for garbage collection:
   {byteCountToDisplaySize(dataSize)} in {dataCount%6d} data segments
   {byteCountToDisplaySize(bulkSize)} in {bulkCount%6d} bulk segments
   ```

   Note this is a *segment*-granularity estimate (an entire segment is retained if any
   record in it is reachable), and only the current head is treated as a root — journal
   history and checkpoints are reachable through the head super-root only.

---

## 6. AEM safety invariants

The Rust ports of these tools run against a **stopped** AEM segment store. To guarantee AEM
starts unharmed afterwards, the implementation must satisfy:

1. **Zero mutation of the store directory.** None of these tools may create, modify,
   rename, truncate, or delete: `*.tar`, `journal.log`, `manifest`, `gc.log`, `repo.lock`,
   or any other file under the segmentstore path. All Oak equivalents open the store with
   `buildReadOnly()`; `ReadOnlyFileStore.writeSegment` throws
   `UnsupportedOperationException` and `ReadOnlyRevisions.setHead` is an in-memory CAS
   only (`ReadOnlyFileStore.java` line 112; `ReadOnlyRevisions.java` lines 88–92).
2. **Do not take or touch the repository lock.** Oak's read-only path never calls
   `lockRepository()`; creating or locking `repo.lock` could collide with an operator's
   expectations or a subsequent AEM start.
3. **Read-only manifest handling.** Validate `manifest` (`store.version` in 1..2) but never
   rewrite/upgrade it (`ManifestChecker.checkManifest()` vs. `checkAndUpdateManifest()`,
   `ManifestChecker.java` lines 50–96). Upgrading the manifest is exclusively a read-write
   store concern.
4. **No recovery side effects.** Oak's read-only tar open (`TarReader.openRO`) opens only
   the highest-generation file per tar group and, at worst, writes a *new*
   `<name>.tar.ro.bak` file, never modifying existing files. A Rust port should prefer
   strictly zero writes: fail (or degrade) on an index-less tar rather than materialize
   recovery files. If `.ro.bak` behavior is mimicked, the name pattern must be
   `<file>.ro.bak`, `<file>.2.ro.bak`, … (`TarReader.findAvailGen`, lines 244–250) — AEM
   ignores such files on startup because they do not match the `data%05d%s.tar` pattern.
5. **Journal is consumed, never repaired.** Tolerate malformed journal lines exactly as
   `JournalReader` does (skip space-less lines, default timestamp `-1`); never rewrite
   `journal.log` (that is `RecoverJournal`'s job, specified elsewhere).
6. **Output files land outside the store's data set.** `tarmkdiff` writes
   `diff_<millis>.log` (or `--output`) in the working directory; nothing else writes files.
   Ensure output paths are never defaulted into the segmentstore directory.
7. **`setRevision` equivalents must be pure.** Rewinding the head for Check/History/Diff
   is an in-memory view switch; it must not leak into `journal.log` or any persisted head
   state.
8. **Exit codes are contract.** Check: 0 = good revision found, 1 = none/error — AEM
   runbooks branch on this. Other tools: 0 success, 1 error (History usage error: 255).
9. **Crash-safety trivially holds** — since no store file is ever opened for write, a
   crashed/killed read tool leaves the store byte-identical; AEM startup is unaffected.
   Any deviation from invariant 1 voids this property and must be treated as a bug.
