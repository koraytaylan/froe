# TarMK File Store Layer: On-Disk Layout, Journal, Manifest, GC Journal, Locking, Store Opening

Byte-exact specification of the *file store* subsystem of Apache Jackrabbit Oak
`oak-segment-tar` (trunk, ~Oak 1.6x-era code), derived exclusively from the Java sources
under `org/apache/jackrabbit/oak/segment/file` and
`org/apache/jackrabbit/oak/segment/spi/persistence`. All citations are relative to
`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/`.

This document covers: the repository directory layout, `journal.log`, `manifest`,
`gc.log`, `repo.lock`, TAR file naming and generation ("a"/"b") selection, head-state
resolution, and the exact open procedures of `FileStore` (read-write) and
`ReadOnlyFileStore`. The interior byte format of TAR archives, segment index,
graph/binary-reference indices, and segments themselves are covered by companion
specifications; this document specifies everything *around* them.

---

## 1. Repository directory layout

A TarMK repository is a single flat directory (the "segmentstore" directory). The local
(default) persistence is implemented by `file/tar/TarPersistence.java`. File names are
constants in that class:

| File name         | Constant (`TarPersistence.java`)       | Role |
|-------------------|----------------------------------------|------|
| `data%05d%s.tar`  | `TarConstants.FILE_NAME_FORMAT` = `"data%05d%s.tar"` | Segment archives. `%05d` = zero-padded archive index (decimal, 5+ digits), `%s` = single lowercase generation letter, always written as `"a"` (`TarWriter` ctor: `format(FILE_NAME_FORMAT, writeIndex, "a")`). Cleanup/GC rewrites a file with the next letter (`b`, `c`, ..., capped at `z`: `TarReader.sweep` refuses to rewrite a generation-`z` file). Examples: `data00000a.tar`, `data00012b.tar`. |
| `journal.log`     | `JOURNAL_FILE_NAME = "journal.log"`    | Append-only text file of head-state revisions ("root record ids"). |
| `gc.log`          | `GC_JOURNAL = "gc.log"`                | Append-only text file with one line per successful compaction+cleanup cycle. |
| `manifest`        | `MANIFEST_FILE_NAME = "manifest"`      | `java.util.Properties` text file carrying the store format version. |
| `repo.lock`       | `LOCK_FILE_NAME = "repo.lock"`         | Zero-length file used only as an OS-level advisory `FileLock` target. |
| `data00000a.tar.bak`, `data00000a.tar.2.bak`, ... | built by `TarReader.findAvailGen(name, ".bak")` | Backup of a corrupt TAR made during read-write recovery. First candidate is `<name>.bak`; if it exists, `<name>.2.bak`, `<name>.3.bak`, ... (counter starts at 2). |
| `data00000a.tar.ro.bak`, `data00000a.tar.2.ro.bak`, ... | `TarReader.openRO`, ext `".ro.bak"` | Artificial recovered archive written by a *read-only* open when no valid index exists; the original file is never modified. |
| `journal.log.bak.XXX` | oak-run `recover-journal` tool (documented in `oak-doc .../overview.md`) | Backup of the journal made by tooling; not read by the store. |

There are no subdirectories. Auxiliary/unknown files are ignored: TAR discovery lists
only names ending in `.tar` (`SegmentTarManager.listArchives()` uses
`SuffixFileFilter(".tar")`) and then filters by the pattern in §6.1, so `*.bak` files are
never picked up (they don't end in `.tar` — note `foo.tar.bak` fails the suffix filter,
and `foo.2.bak` too).

`TarPersistence.segmentFilesExist()` = "directory contains at least one `*.tar` file
(non-recursive)". This drives the manifest-existence check (§3).

---

## 2. Repository lock — `repo.lock`

Source: `TarPersistence.lockRepository()`.

* Read-write `FileStore` **must** acquire the lock before anything else
  (`FileStore.java` constructor: `repositoryLock = persistence.lockRepository();` is the
  first statement after reading the builder).
* Mechanism: open `repo.lock` as `RandomAccessFile(file, "rw")` and call
  `FileChannel.lock()` — an **exclusive, blocking** lock over the entire file
  (position 0, size `Long.MAX_VALUE` per Java semantics). The file content is never
  written; it stays 0 bytes.
* If the same JVM already holds it (`OverlappingFileLockException`), an
  `IllegalStateException` `"<dir> is in use by another store."` is thrown.
* The lock is released on close (`FileStore.registerCloseables`:
  `closer.register(repositoryLock::unlock)`), which releases the `FileLock` and closes
  the file.
* **`ReadOnlyFileStore` never touches `repo.lock`.** Its constructor
  (`ReadOnlyFileStore.java`) does not call `lockRepository()`. A read-only Rust port
  must therefore open the store without any locking (and must tolerate a concurrent
  writer, which is what the "only latest generation" rule in §6.3 is for).

---

## 3. Manifest — `manifest`

Sources: `file/Manifest.java`, `file/ManifestChecker.java`,
`file/LocalManifestFile.java`, `file/AbstractFileStore.java`.

### 3.1 Encoding

`java.util.Properties` text format, read/written through `FileReader`/`FileWriter`
(platform default charset; the content is ASCII in practice). A typical file:

```
#Mon Aug 03 12:00:00 UTC 2026
store.version=2
```

Rules a reader must implement (Java `Properties.load(Reader)` subset sufficient here):
lines of `key=value` (also `key:value` or `key value` are legal Properties syntax);
lines starting with `#` or `!` are comments; leading whitespace stripped; backslash
escapes/line continuations are legal but never produced for this file. `Properties.store`
always emits one comment line containing the current date, then `key=value` lines
terminated by the platform line separator.

### 3.2 Keys

Only one key is defined:

| Key             | Constant | Values |
|-----------------|----------|--------|
| `store.version` | `Manifest.STORE_VERSION = "store.version"` | Decimal integer, parsed with `Integer.parseInt`. Non-numeric or absent → default (see below). |

### 3.3 Version constants and semantics

From `AbstractFileStore.java`:

```java
private static final int MIN_STORE_VERSION = 1;
private static final int MAX_STORE_VERSION = 2;
```

`AbstractFileStore.newManifestChecker(persistence, strictVersionCheck)` builds
`ManifestChecker.newManifestChecker(manifestFile, shouldExist = persistence.segmentFilesExist(), minStoreVersion = strict ? 2 : 1, maxStoreVersion = 2)`.

`ManifestChecker` logic (exact, `ManifestChecker.java`):

```
openManifest():
    if manifest file exists: load it
    elif shouldExist (i.e. *.tar files present):
        throw InvalidFileStoreVersionException("Using oak-segment-tar, but oak-segment should be used")
    else: empty manifest

checkManifest(m):
    v = m.getStoreVersion(default = maxStoreVersion)   # absent/invalid -> maxStoreVersion (=2)
    if v <= 0:                throw IllegalStateException("Invalid store version")   # unrecoverable
    if v <  minStoreVersion:  throw InvalidFileStoreVersionException("Using a too recent version of oak-segment-tar")
    if v >  maxStoreVersion:  throw InvalidFileStoreVersionException("Using a too old version of oak-segment tar")
```

* **Read-write** open (`FileStore` ctor): `checkAndUpdateManifest()` — after a
  successful check it unconditionally rewrites the manifest with
  `store.version=2` (`ManifestChecker.updateManifest`).
* **Read-only** open (`ReadOnlyFileStore` ctor): `checkManifest()` only — the file is
  never written. A Rust read-only port must do the same: check, never write.
* If the directory is empty (no tar files, no manifest), read-only open proceeds past
  the manifest check with an empty manifest (accepted, since default = 2), and then
  fails later at journal binding (§5.6).

**Version ↔ release mapping** (the Java code itself does not state this; the mapping
below is inferred from `SegmentVersion.java`, `GCJournal.java` javadoc "since Oak 1.8",
and `oak-doc/.../changes.md`, and should be treated as documentation, not code):

| `store.version` | Oak release line | Segment format | TAR index |
|---|---|---|---|
| 1 | Oak 1.6.x (oak-segment-tar 0.0.x/1.6) | segment version 12 | index V1 |
| 2 | Oak ≥ 1.8 (incl. current) | segment version 13 (12 still readable) | index V2 (V1 still readable) |
| (no manifest, tars present) | Oak ≤ 1.4 legacy `oak-segment` (segment versions 10/11) | **rejected** | — |

A reader targeting "current format" must accept `store.version` ∈ {1, 2}, reject ≤ 0
and ≥ 3.

---

## 4. Record-id string grammar (used by `journal.log` and `gc.log`)

Source: `RecordId.java` (`PATTERN`, `toString`, `toString10`), `SegmentId.toString()`
(= `new UUID(msb, lsb).toString()`, i.e. canonical lowercase hyphenated UUID).

Parse pattern (`RecordId.PATTERN`, anchored via `matches()` — the entire string must
match):

```
([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})   # group 1: segment UUID, lowercase hex only
(
  :(0|[1-9][0-9]*)          # group 3: record number, decimal, no leading zeros ("Oak 1.0" form)
  |
  \.([0-9a-f]{8})           # group 4: record number, exactly 8 lowercase hex digits
)
```

* `toString10()` produces `"%s:%d"` → e.g.
  `f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976`. **This is the form written to
  `journal.log` and `gc.log`.**
* `toString()` produces `"%s.%08x"` → e.g. `f81a...303.00042280` (diagnostic form,
  also accepted by the parser).
* The record number is a Java `int` parsed with `Integer.parseInt` (decimal) or
  `Integer.parseInt(s, 16)` (hex). It is a *record number* (logical id resolved via the
  segment's record-number table), not a byte offset.
* Uppercase hex UUIDs do **not** match; parsing then throws
  `IllegalArgumentException("Bad record identifier: ...")`.
* The null record id (`RecordId.NULL`) is segment UUID
  `00000000-0000-0000-0000-000000000000`, record number 0; its `toString10()` is
  `00000000-0000-0000-0000-000000000000:0`.

---

## 5. Journal — `journal.log`

Sources: `file/tar/LocalJournalFile.java` (I/O), `file/TarRevisions.java` (write),
`file/JournalReader.java` + `file/JournalEntry.java` (read),
`file/FileStoreUtil.java` (head resolution), `file/ReadOnlyRevisions.java`.

### 5.1 Physical format

A plain text file; a sequence of lines appended over the life of the repository, oldest
first. Each line written by the current implementation is exactly:

```
<recordid-toString10> root <millis>\n
```

produced by `TarRevisions.doFlush()`:

```java
journalFileWriter.writeLine(after.toString10() + " root " + System.currentTimeMillis());
```

* Field separator: single ASCII space `0x20`.
* Field 0: record id in the `uuid:decimal` form of §4.
* Field 1: literal string `root` (never inspected by the reader; historical tag).
* Field 2: `System.currentTimeMillis()` — decimal milliseconds since Unix epoch
  (e.g. `1754556000123`).
* Line terminator: exactly one `\n` (`0x0A`); `LocalJournalFileWriter.writeLine` does
  `journalFile.writeBytes(line + "\n")` on a `RandomAccessFile` opened `"rw"` and seeked
  to EOF, then `getChannel().force(false)` (fsync data, not metadata) — the journal is
  durable line-by-line.
* `RandomAccessFile.writeBytes` writes the **low byte of each char** — effectively
  ISO-8859-1; content is pure ASCII.
* Historical files may contain lines with only two fields (`<recordid> root`) — the
  reader tolerates that (timestamp −1, see below).
* `JournalFileWriter.truncate()` (`setLength(0)`) and `batchWriteLines` exist for
  tooling/recovery; the normal store only ever appends.

Example file:

```
1c2ba7b1-4bb8-47a5-ac93-379b3b53c8f0:262336 root 1754555000001
f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976 root 1754556000123
```

The **last** line is the most recent head.

### 5.2 Reading order

`LocalJournalFile.openJournalReader()` returns a reader backed by Apache commons-io
`ReversedLinesFileReader(file, defaultCharset())`: it yields lines **from the end of the
file toward the beginning**, without line terminators. Semantics the Rust port must
match:

* Recognized terminators: `\n`, `\r\n`, `\r`.
* The file's single trailing terminator (if any) does not produce an empty first line;
  every other empty line in the file *is* yielded as an empty string.
* Default platform charset (content is ASCII in practice).

### 5.3 Line validation (`JournalReader.computeNext`)

For each line, scanning backwards:

```
if line contains no ' ' (0x20):    log warn "Skipping invalid journal entry", continue to previous line
splits = line.split(" ", -1)       # split on single spaces, keep trailing empties
revision  = splits[0]              # NOT validated here
timestamp = -1
if splits.length > 2:
    timestamp = parse splits[2] as signed 64-bit decimal; on failure keep -1 (warn)
else: warn "Timestamp information is missing"
yield JournalEntry(revision, timestamp)
```

Notes:

* A truncated/garbage final line (e.g. a partially-written record id with no space) is
  skipped, not fatal.
* A line like `garbage stuff` *is* yielded (it has a space); revision validity is only
  checked by the consumer (§5.4).
* Any `IOException` while reading ends iteration silently (logged, treated as
  end-of-data) — i.e. a journal that becomes unreadable mid-scan just yields fewer
  entries.

### 5.4 Head resolution (`FileStoreUtil.findPersistedRecordId`)

```
for each JournalEntry, newest -> oldest:
    try:
        id = RecordId.fromString(entry.revision)      # grammar of §4
    except IllegalArgumentException:
        warn "Skipping invalid record id", continue
    if store.containsSegment(id.segmentId):           # segment UUID present in any tar index / writer
        return id
    warn "Unable to access revision ..., rewinding..."
return null
```

* "Contains" is a **shallow** check: only the head record's own segment UUID must be
  present in some TAR index (`TarFiles.containsSegment(msb, lsb)`). No deep consistency
  check is done; if the head node's children live in missing segments, reads later throw
  `SegmentNotFoundException`.
* Entries whose segment is missing cause a *rewind* to the previous (older) journal
  line — this is the crash-recovery mechanism after TAR truncation/recovery.

### 5.5 From record id to node state

`AbstractFileStore.getHead()` = `segmentReader.readHeadState(revisions)` which builds a
`SegmentNodeState(reader, writer, blobStore, revisions.getHead())` — i.e. the journal
record id **is** the record id of the root `NodeState` record (a node record, segment
format spec). There is no additional indirection.

### 5.6 Binding behavior

* Read-write (`TarRevisions.bind`): if `findPersistedRecordId` returns null (empty or
  fully-invalid journal), a fresh initial node is **written**: an empty node with a
  single child `root` (empty) (`FileStore.initialNode()`), and its record id becomes the
  head (not persisted to the journal until the first flush).
* Read-only (`ReadOnlyRevisions.bind`): if null →
  `IllegalStateException("Cannot start readonly store from empty journal")`. **A
  read-only port must fail on an empty/unusable journal.**
* `ReadOnlyFileStore.setRevision(String)` allows time travel to any revision string
  (parsed by §4 grammar) with a compare-and-set on the in-memory head; nothing on disk
  changes.

---

## 6. TAR archive discovery, naming, and generation selection

Sources: `file/tar/TarFiles.java`, `file/tar/TarReader.java`,
`file/tar/SegmentTarManager.java`, `file/tar/TarConstants.java`.

### 6.1 Name pattern

`TarFiles.FILE_NAME_PATTERN`:

```
(data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar
```

* Group 2 (index): a decimal number with no leading zeros in its head part followed by
  exactly 4 more digits — i.e. at least 5 digits total, zero-padded to 5
  (`data00000a.tar` → index 0; `data123456a.tar` → index 123456). Parsed with
  `Integer.parseInt` (Java int).
* Group 4 (generation): optional single letter `a`–`z`; **absent means `'a'`**
  (`collectFiles`: `Character generation = 'a'; if (matcher.group(4) != null) ...`).
  So legacy `data00000.tar` ≡ generation `a`.
* Note the regex's `.` before `tar` is unescaped in the Java source, so `data00000aXtar`
  would technically match; combined with the `.tar` suffix pre-filter of
  `listArchives()` this is unreachable in practice. Implement as literal `".tar"`.
* Two files mapping to the same (index, generation) is a fatal state error
  (`Validate.checkState(files.put(generation, file) == null)`).

`collectFiles` produces `Map<index, Map<generationChar, fileName>>`.

### 6.2 Reader list construction (`TarFiles.init`)

* Sort indices ascending; open one `TarReader` per index (in parallel in current code);
  maintain the reader list in **descending index order** (newest archive first).
* Segment lookup (`TarFiles.readSegment`/`containsSegment`) probes the (write-mode)
  current writer first, then readers newest→oldest, returning the first hit. Duplicated
  segment UUIDs across archives therefore resolve to the **newest** archive's copy.
* Read-write mode also computes the next writer index = `max(indices) + 1` (0 for an
  empty directory) and instantiates a `TarWriter` named
  `format("data%05d%s.tar", writeIndex, "a")`. The file itself is created **lazily**
  on the first segment write (`SegmentTarWriter`: "Initialized lazily ... to avoid
  creating an extra empty file when just reading from the repository"), so an
  otherwise-idle store does not necessarily leave an empty writer archive on disk.
* If any `TarReader` fails to open with `IOException`, `init` fails (exception is
  rethrown) — opening the store fails.

### 6.3 Generation selection per index — read-write (`TarReader.open(Map,...)`)

Given all generations of one index (e.g. `{a: data00001a.tar, b: data00001b.tar}`):

1. Sort generation letters ascending, then iterate the file list in **reverse**
   (highest letter first).
2. `openFirstFileWithValidIndex`: first file whose TAR index loads and validates wins
   (see index spec for validation). **All other generations of that index are deleted
   from disk.** `IOException` on a candidate → warn + try next.
3. If none has a valid index: full recovery —
   * scan every generation *in ascending letter order* with the raw tar scan of §6.5,
     accumulating entries into one ordered map (an entry from a later file replaces an
     earlier duplicate only when its TAR entry name carries a checksum suffix and the
     CRC matches; an entry *without* a checksum suffix is read only if its UUID is not
     yet in the map — §6.5 exact rule `if checksum != null or id not already in entries`);
   * back up each scanned file to `<name>.bak` (rename, or copy+delete on failure)
     (`backupSafely`);
   * rewrite the **lowest-letter** file name (e.g. `data00001a.tar`) from the recovered
     entries via a fresh `TarWriter` (regenerating graph/binary-ref/index structures via
     `TarRecovery`/`AbstractFileStore.writeSegment`, which re-parses each data segment);
   * reopen; failure now is a fatal `IOException`.

### 6.4 Generation selection per index — read-only (`TarReader.openRO`)

**Only the highest generation letter is ever considered**:
`String file = files.get(Collections.max(files.keySet()));` — "for readonly store only
try the latest generation of a given tar file to prevent any rollback or rewrite".
Lower generations are neither opened nor deleted.

Then three strategies are tried in order; first success wins:

1. `open` — normal open with index validation.
2. `forceOpen` — for local `SegmentTarManager` this is identical to `open` (it simply
   delegates), so locally it is a second identical attempt; it differs only for remote
   persistences.
3. `recoverAndOpen` — raw-scan the file (no backup, original untouched), write the
   recovered entries to a new artificial archive named `<file>.ro.bak`
   (or `<file>.N.ro.bak`, N = 2, 3, ... first non-existing), and open that.

If all fail → `IOException("Failed to open tar file ...")` → store open fails.

**Note for a pure-reader Rust port**: strategy 3 *writes* a `.ro.bak` file into the
repository directory. A strictly non-writing implementation may instead fail, or keep
the recovered entries in memory; behavioral parity with Oak requires the `.ro.bak`
file.

### 6.5 Raw TAR recovery scan (`SegmentTarManager.recoverEntries`)

Used when no valid index exists. Byte-exact algorithm over the file:

```
BLOCK_SIZE = 512                       # TarConstants.BLOCK_SIZE
# "filePointer" below is the live file position, exactly as in Java's
# RandomAccessFile.getFilePointer(): it has ALREADY advanced past whatever
# was just read.
while filePointer + 512 <= fileLength:
    header = read 512 bytes             # filePointer advances by 512
    sum = Σ (header[i] & 0xff) for i in 0..511          # int arithmetic
    if sum == 0 and filePointer + 2*512 == fileLength:
        # Fires only when the all-zero block just read is followed by exactly
        # 1024 bytes to EOF, i.e. the zero block starts at fileLength - 1536.
        # (Java comment: "found the zero blocks at the end of the file".)
        return
    # replace checksum field with spaces:
    for i in 148..155: sum -= header[i] & 0xff; sum += 0x20
    checkbytes = bytes of String.format("%06o\0 ", sum) # 6 octal digits, NUL, space
    compare checkbytes against header[148..155]; on mismatch only WARN (not skip!)
    name = NUL-terminated string from header[0..99] (UTF-8)
    size = octal number parsed from header[124..135]    # stops at first non-'0'..'7'
    if filePointer + size > fileLength:
        warn "Partial entry", return                    # truncated tail ignored
    if name matches NAME_PATTERN:
        # NAME_PATTERN = uuid ( "." 8-lowercase-hex-digits )? ( "." anything )?
        id = UUID from group 1
        checksum = group 3 (8 hex digits after first '.') or null
        if checksum != null or id not already in entries:
            data = read exactly `size` bytes
            skip padding to next 512 boundary
            if checksum != null:
                crc = CRC32(data)                       # standard zlib CRC-32
                if crc != parseLong(checksum, 16): warn, continue (entry dropped)
            entries[id] = data                          # insertion-ordered map; re-put overwrites value, keeps order
    elif name != "<tarFileName>.idx":
        warn "Unexpected entry", seek past size + padding
```

TAR entry names for segments are written by `SegmentTarWriter` as
`<uuid>.<crc32-as-8-hex-digits>` (`String.format("%s.%08x", uuid, crc)`; see the tar
spec); legacy entries may lack the checksum suffix.

Two easy-to-miss consequences of the pseudocode above (both verified against
`SegmentTarManager.recoverEntries`, bug-compatible behavior):

* Metadata entries `<tarFileName>.gph` and `<tarFileName>.brf` do not match
  `NAME_PATTERN` and take the `elif` branch: they produce the "Unexpected entry"
  **warning** and their payload is seeked past. An entry named exactly
  `<tarFileName>.idx` is exempted from *both* the warning *and* the seek — its payload
  bytes are then themselves scanned as if they were tar headers (they generally fail
  the checksum comparison, which only warns, and fall into further "Unexpected entry"
  skips until the loop terminates).
* If a name matches `NAME_PATTERN` but has **no** checksum group and its UUID is
  already in `entries`, *nothing* is read or seeked for that entry: the scan continues
  with the entry's payload bytes interpreted as the next header.

Recovered *data* segments (UUID lsb top nibble == 0xA, `SegmentId.isDataSegmentId(lsb)`:
`(lsb >>> 60) == 0xA`) get their `GCGeneration` re-read from the segment header
(`AbstractFileStore.writeSegment` → `Segment.getGcGeneration`); bulk segments get
`GCGeneration.NULL` = (generation 0, fullGeneration 0, isCompacted false). Graph edges
and binary references for data segments are reconstructed by parsing each segment
(`populateTarGraph`, `populateTarBinaryReferences`).

### 6.6 Per-archive open and memory mapping (`SegmentTarManager.open`)

* Open `RandomAccessFile(file, "r")`; `SegmentTarReader.loadAndValidateIndex` reads the
  index from the end of the file (index spec). If there is **no valid index**, `open`
  returns `null` ("No index found in tar file, skipping") — distinct from an
  `IOException`.
* If `memoryMapping` is enabled: map the **whole file** read-only
  (`FileAccess.Mapped`); on mmap failure fall back (warn) to pread-style access.
* Otherwise `FileAccess.Random` (heap buffers) or `FileAccess.RandomOffHeap`
  (direct buffers) if `offHeapAccess`.
* Defaults (`FileStoreBuilder.java`): `memoryMapping` defaults to `true` iff system
  property `sun.arch.data.model == "64"` (`MEMORY_MAPPING_DEFAULT`); `offHeapAccess`
  defaults to system property `access.off.heap` (false); max tar file size
  `DEFAULT_MAX_FILE_SIZE = 256` MB (write path only).

---

## 7. GC journal — `gc.log`

Sources: `file/GCJournal.java` (format), `file/LocalGCJournalFile.java` (I/O).

Not needed to resolve the head, but read by the garbage collector and by
`ReadOnlyFileStore.collectBlobReferences` indirectly (via GC generation math). A pure
reader can treat it as informational.

### 7.1 Line format

One line per successful compaction+cleanup, comma-separated, **no spaces**
(`GCJournalEntry.toString()`, `String.join(",", ...)`):

```
<repoSize>,<reclaimedSize>,<timestamp>,<generation>,<fullGeneration>,<nodes>,<root>
```

| Field | Type | Meaning |
|---|---|---|
| `repoSize` | signed 64-bit decimal | repository size after cleanup, bytes |
| `reclaimedSize` | signed 64-bit decimal | bytes reclaimed by cleanup |
| `timestamp` | signed 64-bit decimal | `System.currentTimeMillis()` |
| `generation` | signed 32-bit decimal | `GCGeneration.getGeneration()` of the compacted head |
| `fullGeneration` | signed 32-bit decimal | `GCGeneration.getFullGeneration()` — **present only since Oak 1.8** |
| `nodes` | signed 64-bit decimal | number of compacted nodes |
| `root` | record-id string, `toString10` form (§4) | root record written by the compactor; may be `00000000-0000-0000-0000-000000000000:0` |

Example:

```
127469568,60295168,1754556010042,2,2,180042,f81ad1ac-e73e-4db0-a4b6-b1c8aa5cf303:270976
```

### 7.2 Parsing (`GCJournalEntry.fromString`)

```
items = in.split(",")            # Java split: trailing empty strings dropped
repoSize      = parseLong(items[0])   # any parse failure or missing item -> -1 (warn)
reclaimedSize = parseLong(items[1])
ts            = parseLong(items[2])
generation    = parseInt (items[3])   # failure -> -1
if items.length == 7:                 # Oak >= 1.8 format
    fullGeneration = parseInt(items[4]); next = 5
else:                                 # Oak 1.6 format (6 fields)
    fullGeneration = generation;       next = 4
nodes = parseLong(items[next]); root = items[next+1] or NULL-record-id string if absent
resulting GCGeneration = newGCGeneration(generation, fullGeneration, isCompacted=false)
```

`GCJournal.read()` uses only the **last** line; `readAll()` parses every line. An
unreadable file yields the EMPTY entry `(-1, -1, -1, GCGeneration.NULL, -1, NULL-id)`.

### 7.3 I/O discipline (`LocalGCJournalFile`)

* Read: `Files.readAllLines(path, UTF_8)`; missing file → empty list.
* Write: open with `WRITE|APPEND|CREATE|DSYNC`, UTF-8, write line +
  `BufferedWriter.newLine()` — i.e. the **platform line separator** (may be `\r\n` on
  Windows); `readAllLines` strips both `\n` and `\r\n`, so readers must accept both.
* `truncate()` deletes the file.
* New entries are only appended when the GC generation actually advanced
  (`GCJournal.persist` no-ops if the latest entry has the same `GCGeneration` —
  comparison includes generation, fullGeneration *and* isCompacted).

---

## 8. Store opening sequences (normative order of operations)

### 8.1 Read-write `FileStore` (`FileStoreBuilder.build()` → `FileStore` ctor → `bind`)

1. `directory.mkdirs()`.
2. Create `TarRevisions` — opens `journal.log` for append ("rw", seek to EOF); the file
   is created here if absent.
3. `FileStore` ctor:
   a. **Acquire `repo.lock`** (blocking exclusive lock).
   b. Build segment writer (irrelevant to disk layout).
   c. Manifest: `checkAndUpdateManifest()` — reject legacy/no-manifest-with-tars,
      reject version > 2 or ≤ 0, then rewrite manifest with `store.version=2`.
   d. `TarFiles.init()` — discover `data*.tar`, per-index generation selection with
      destructive cleanup/recovery (§6.3), then set up the writer archive
      `data<max+1, %05d>a.tar` (file created lazily on first segment write, §6.2).
4. `bind(revisions)` → `TarRevisions.bind`: scan journal backwards for the newest
   revision whose segment exists (§5.4); if none, write the initial node.
5. Background: flush every 5 s (`journal.log` gets a new line only when the head
   changed and after segments+tar data were fsynced first — write ordering guarantee:
   *segments are always durable before the journal line referencing them*,
   `TarRevisions.doFlush` calls `flusher.flush()` before `writeLine`).

### 8.2 `ReadOnlyFileStore` (`FileStoreBuilder.buildReadOnly()`)

1. Precondition: directory exists and is a directory (`checkState`), **no mkdir**.
2. Create `ReadOnlyRevisions` (holds `journal.log` reference; nothing opened yet).
3. `ReadOnlyFileStore` ctor:
   a. **No repository lock.**
   b. Manifest: `checkManifest()` only (no write).
   c. `TarFiles` built with `withReadOnly()` and initialized: discovery as §6.2, but
      per-index selection per §6.4 (highest letter only, no deletion; `.ro.bak`
      creation is the only possible write). No writer archive is created.
      In read-only mode `maxFileSize` is not required and no `FileStoreMonitor` is
      needed (`TarFiles.Builder.build` checks `readOnly || ...`).
4. `bind(revisions)` → `ReadOnlyRevisions.bind`: resolve head per §5.4; **fatal
   `IllegalStateException` if nothing resolvable**.
5. `getHead()` → `SegmentNodeState` at the head record id; traversal from there uses
   `readSegment` → segment cache → `TarFiles.readSegment` (writer=null, newest reader
   first); a miss throws `SegmentNotFoundException` (per-segment, at access time).

---

## 9. Error / recovery matrix for a read-only reader

| Condition | Behavior (Java) |
|---|---|
| `manifest` missing, no `*.tar` files | Accepted (empty store) — but read-only bind then fails on empty journal. |
| `manifest` missing, `*.tar` present | Fatal: `InvalidFileStoreVersionException` ("oak-segment should be used"). |
| `store.version` ≤ 0 | Fatal `IllegalStateException`. |
| `store.version` = 1 or 2 | OK (1 rejected only with `strictVersionCheck`). |
| `store.version` ≥ 3 | Fatal: "too old version of oak-segment tar" (i.e. reader too old). |
| `journal.log` missing | `ReversedLinesFileReader` ctor throws `IOException` at bind → open fails. |
| Journal line without a space | Skipped (warn), scan continues backwards. |
| Journal line with unparseable record id | Skipped (warn). |
| Journal line whose segment is absent from all tar indices | Skipped ("rewinding"), older lines tried. |
| No journal line resolvable (read-only) | Fatal `IllegalStateException("Cannot start readonly store from empty journal")`. |
| Malformed / missing timestamp field | Tolerated; timestamp = −1. |
| I/O error while scanning journal | Treated as end of data (fewer entries), not fatal per se. |
| Tar file with unreadable/invalid index (read-only) | open → forceOpen → raw-scan into `.ro.bak`; only if all three fail is the store open fatal. |
| Tar entry with CRC mismatch during raw scan | Entry dropped (warn), scan continues. |
| Truncated tar tail during raw scan | Remainder ignored (warn), entries so far kept. |
| Segment missing during node traversal (after successful open) | `SegmentNotFoundException` for that read only. |
| gc.log missing/corrupt | Non-fatal; parsed fields default to −1 / NULL ids. |

---

## 10. Miscellaneous constants relevant to this layer

| Constant | Value | Source |
|---|---|---|
| `TarConstants.BLOCK_SIZE` | 512 | `file/tar/TarConstants.java` |
| `TarConstants.FILE_NAME_FORMAT` | `"data%05d%s.tar"` | same |
| `MIN_STORE_VERSION` / `MAX_STORE_VERSION` | 1 / 2 | `file/AbstractFileStore.java` |
| Data-segment UUID discriminator | `(lsb >>> 60) == 0xA` | `SegmentId.isDataSegmentId` |
| `RecordId.SERIALIZED_RECORD_ID_BYTES` | 20 (msb u64 BE, lsb u64 BE, recordNumber u32 BE — in-segment serialization, not used in text files) | `RecordId.getBytes` |
| Default max tar size | 256 MB (`maxFileSize * 1024 * 1024`) | `FileStoreBuilder.DEFAULT_MAX_FILE_SIZE`, `FileStore` ctor `MB = 1024*1024` |
| Flush interval | 5 seconds | `FileStore` ctor |
| Journal flush trigger | only when head record id changed since last persisted | `TarRevisions.doFlush` |
