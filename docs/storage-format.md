# The segment-tar storage format

The on-disk format of Apache Jackrabbit Oak's TarMK, as implemented by
this workspace. Derived from the `oak-segment-tar` Java sources and the
official format documentation, then cross-verified; where the two
disagree, the code is authoritative and the discrepancy is noted.

Conventions: every multi-byte integer is **big-endian**. Java `int`
arithmetic wraps at 32 bits, `>>` is an arithmetic shift, and shift
distances are taken modulo the operand width — three behaviors that are
load-bearing in this format and reproduced by `froe`.

## 1. Repository directory

A repository is one flat directory:

| File | Role |
| --- | --- |
| `data00000a.tar`, `data00001a.tar`, … | Segment archives. The five-plus digits are the archive number; the trailing letter is the *file generation*, bumped (`a` → `b` → …) when cleanup rewrites an archive. A reader opens only the highest letter of each number. |
| `journal.log` | Append-only text file of head revisions. |
| `manifest` | Java properties file; key `store.version` is `1` (Oak 1.6) or `2` (Oak 1.8 and later). Archives without a manifest mean the legacy pre-tar format. |
| `gc.log` | One comma-separated line per completed garbage collection cycle. |
| `repo.lock` | Zero-byte advisory lock target. Writers lock it; readers never touch it. |

### Journal lines

```
<segment-uuid>:<record-number-decimal> root <milliseconds-since-epoch>
```

The last line is the newest head. Readers scan backwards, skipping
malformed lines and revisions whose segment no longer exists (the crash
recovery mechanism). The revision points at the **super-root** node,
whose children are `root` (the content tree) and optionally
`checkpoints` (one child per checkpoint, each holding `created` and
`timestamp` properties, a `properties` child, and a full `root`
snapshot that shares records with the live tree).

## 2. Archive layout

An archive is a standard tar file (512-byte blocks, two zero blocks at
the end). Entry order: segments, then three trailer entries — binary
references (`.brf`), graph (`.gph`), index (`.idx`) — each padded so its
payload **ends** on a block boundary. All three end with the same
16-byte footer, read backwards from known anchors:

```
+----------+----------+----------+----------+
| CRC32    | count    | size     | magic    |   4 bytes each
+----------+----------+----------+----------+
```

The magic is therefore in the *last* four bytes of each structure. The
index footer ends exactly 1024 bytes before the end of the file.

| Structure | Magic (int) | Bytes in file |
| --- | --- | --- |
| Index version 1 | `0x0A304B0A` | `0A 30 4B 0A` |
| Index version 2 | `0x0A314B0A` | `0A 31 4B 0A` |
| Graph | `0x0A30470A` | `0A 30 47 0A` |
| Binary references version 1 | `0x0A30420A` | `0A 30 42 0A` |
| Binary references version 2 | `0x0A31420A` | `0A 31 42 0A` |

### Segment entries

Entry name: `<uuid>.<crc32 of the segment bytes as exactly 8 lowercase
hex digits>`. The index records the file position of the first *data*
byte (past the 512-byte tar header), always block-aligned.

### The index (required)

Entries sorted ascending by UUID compared as **signed** 64-bit halves.
Version 1 entries are 28 bytes: `msb(8) lsb(8) position(4) size(4)
generation(4)`. Version 2 entries are 33 bytes, appending
`full_generation(4)` and a `compacted` byte. Validation covers the
checksum (CRC32 over the entries only), footer consistency, sort order,
duplicates, alignment, and sizes. Kept quirk: the version 2 loader's
minimum-size check uses the version *1* entry size.

An archive without a valid index (for example, the archive a live
repository is currently writing — the index is only written on close)
is opened by the **recovery scan**: walk the tar headers, accept
segment entries whose name-embedded CRC32 matches their content, stop
at truncation. The Java implementation persists recovery results to a
`.ro.bak` file even when read-only; `froe` keeps them in memory and
never writes.

### The graph and binary references (optional)

The graph stores per data segment the list of referenced segment UUIDs
(`msb lsb count [msb lsb]…` per source). The binary references catalog
stores, per garbage collection generation and per segment, the
identifiers of externally stored binaries. Both are diagnostics/garbage
collection aids; a corrupt or missing one is tolerated.

## 3. Segments

A segment holds up to 256 KiB and is identified by a UUID whose lower
half's top nibble encodes its kind: `0xA` = *data*, `0xB` = *bulk*.

**Bulk segments** have no header: they are raw 4 KiB block records, and
a record number in a bulk segment *is* the virtual offset.

**Data segments** start with a 32-byte header:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 3 | Magic `"0aK"` |
| 3 | 1 | Format version: 12 (`0x0C`, Oak 1.6) or 13 (`0x0D`, Oak 1.8+) |
| 4 | 4 | Version 13 only: full generation; bit 31 = compacted flag. Version 12 readers report `full_generation = generation` and `compacted = true`. |
| 10 | 4 | Generation |
| 14 | 4 | Referenced segment count (at most 65533) |
| 18 | 4 | Record count |
| 32 | 16 each | Referenced segment UUIDs (`msb`, `lsb`) |
| … | 9 each | Record table: `record_number(4) type(1) offset(4)`, sorted ascending by record number |

Record type bytes: 0 map leaf, 1 map branch, 2 list bucket, 3 list,
4 value, 5 block, 6 template, 7 node, 8 external blob identifier.

### Virtual addressing

Record offsets live in a virtual 256 KiB segment whose end coincides
with the stored buffer's end. The buffer position of virtual offset `v`
in a segment of `size` bytes is `size - 262144 + v`.

### Record identifiers

Inside record data, a reference to another record is 6 bytes: a 16-bit
segment reference (0 = this segment, `n` = entry `n - 1` of the
segment reference table) and a 32-bit record number, resolved through
the *target* segment's record table.

## 4. Record encodings

### Values (strings and binaries)

The high bits of the first byte select the class:

| Pattern | Class | Layout |
| --- | --- | --- |
| `0xxxxxxx` | small, 0–127 bytes | length byte, then data |
| `10xxxxxx` | medium, 128–16511 bytes | `u16 = 0x8000 \| (length - 128)`, then data |
| `110xxxxx` | long, ≥ 16512 bytes | `u64 = (length - 16512) \| (0b11 << 62)`, then the record identifier of a block list; blocks are 4096 bytes, the last one short |
| `1110xxxx` | external binary, short identifier | `u16 = 0xE000 \| identifier_length` (12 bits), then the identifier in UTF-8 |
| `11110xxx` | external binary, long identifier | one marker byte, then (unaligned, at offset 1) the record identifier of a string record |
| `11111xxx` | invalid | — |

Strings are UTF-8; malformed sequences decode to replacement characters
(matching Java). All non-binary property values — longs, doubles,
booleans, dates, decimals — are stored as their string forms.

### Lists

An *uncounted* list's meaning depends on a size known from context:
size 1 means the pointer is the element itself; otherwise it points at
a bucket of up to 255 record identifiers, nested recursively (at most
255³ elements). A *counted* list (multi-valued properties) prefixes a
32-bit size and omits the pointer when empty.

### Maps (child nodes)

A hash array mapped trie keyed by
`hash = (utf16_string_hash(name) ^ 0xDEECE66D) * 0xDEECE66D + 0xB`
with wrapping 32-bit arithmetic over UTF-16 code units. The head word:
`size` in the low **29** bits, `level` in the top **3** (the Java
source comments claim 28/4; the computed constants — and the official
documentation's 2²⁹−1 capacity — say otherwise).

* **Diff** (`head == 0xFFFFFFFF`): `hash(4) key_identifier(6)
  value_identifier(6) base_map_identifier(6)` — one overlaid entry on a
  base map.
* **Branch** (size > 32 and level < 7): `head(4) bitmap(4)` then one
  identifier per set bit in ascending bit order. The bucket index at
  level `L` is `(hash >> ((32 - (L + 1) * 5) & 31)) & 0x1F` with an
  arithmetic shift — at level 6 the masked distance is 29, re-reading
  the hash's top bits.
* **Leaf** (everything else): `head(4)`, `size` hashes sorted as
  *unsigned* values (ties by name in UTF-16 order), then interleaved
  key/value identifier pairs.

### Templates

`head(4)`: bit 31 has-primary-type, bit 30 has-mixin-types, bit 29 zero
children, bit 28 many children, bits 27–18 mixin count, bits 17–0
property count. Then, in order: the primary type name identifier, the
mixin name identifiers, the single child's name identifier (both arity
bits clear), the property-name list identifier, and one signed type
byte per property — the JCR type tag 1–12, negative for multi-valued.
`jcr:primaryType` and `jcr:mixinTypes` live here, not in the property
list.

### Nodes

A sequence of record identifiers: stable identifier, template, then —
per the template — the child map (many) or single child node (one),
then the property value list (only when properties exist; entry `i`
belongs to template property `i`). Multi-valued properties point at a
counted list of value records. The stable identifier slot is either a
self-reference (uncompacted node) or points at a 20-byte block holding
`msb(8) lsb(8) record_number(4)` of the original pre-compaction record.

## 5. Version history

| Store version | Oak releases | Segment version | Index version |
| --- | --- | --- | --- |
| 1 | 1.6 | 12 | 1 |
| 2 | 1.8 and later | 13 (12 remains readable) | 2 (1 remains readable) |

Record encodings are byte-identical across segment versions 12 and 13;
the differences are confined to the segment header and index entries
(full generation and compacted flag).

## 6. Load-bearing quirks

Behaviors that look like bugs but are part of the format contract, all
reproduced by `froe`:

1. Index entries sort by *signed* UUID half comparison.
2. The version 2 index loader validates sizes with version 1 constants.
3. The graph's minimum-size check (`4 + count * 34`) matches no actual
   record size — a holdover from an older format.
4. Map records use 29 size bits and 3 level bits; the Java source
   comments are stale.
5. Branch bucket selection at trie level 6 relies on Java masking shift
   distances modulo 32.
6. The recovery scanner does not skip the `.idx` entry's payload,
   treats a tar-header checksum mismatch as a warning rather than a
   rejection, and parses entry sizes with *wrapping* 32-bit arithmetic —
   an oversized octal size field wraps (possibly negative) and the scan
   continues past it.
7. A one-element list has no bucket record — the pointer is the
   element.
8. The long-value length mask differs between the string reader
   (62 bits) and the blob reader (61 bits); both agree for every length
   a writer can produce.

## 7. Writing

`froe` writes stores byte-for-byte compatible with what Oak produces
(one documented rendering residue: extreme-subnormal doubles re-render
during compaction to a different — equally round-tripping — shortest
form; see `double_to_text`). The write path preserves every invariant a
subsequent Oak (or AEM) start depends on:

* **Locking.** A write session takes an exclusive lock on `repo.lock`
  before anything else — a POSIX `fcntl` record lock, the same lock space
  Oak's `FileChannel.lock()` uses, so it genuinely excludes a running
  instance. The lock file is never written or deleted.
* **Manifest.** The manifest is rewritten with `store.version=2` on every
  write open; a directory with archives but no manifest is rejected.
* **Durability ordering.** Segment bytes are appended and fsynced before
  the journal line referencing them is appended and fdatasynced, and a
  journal line is written only when the head actually moved. A crash
  therefore never leaves the journal pointing at unwritten segments.
* **Segment building.** Segments carry the `0aK` magic, version 13, the
  correct generation triple in the header, a segment-info string
  (`{"wid":…,"sno":…,"t":…}`) as record 0, referenced-segment and
  record-reference tables, and 16-byte-aligned length — everything the
  reader validates.
* **Archives.** New archives are named `data%05da.tar`; each holds its
  segments followed by the `.brf`, `.gph`, and `.idx` trailers and two
  zero blocks, with index entries sorted by signed UUID halves. The graph
  lists every data segment's references and the binary references catalog
  lists every external binary — cleanup and blob garbage collection trust
  these.
* **Generations.** Normal writes carry the head's generation with the
  compacted flag cleared; full compaction advances generation and full
  generation; tail compaction advances only the generation; bulk segments
  are stamped `(0, 0, false)`.
* **Bootstrap.** A fresh store is created with the exact initial node —
  a super-root whose only child is an empty `root` — at generation
  `(0, 0, false)`, matching `FileStore.initialNode`.
* **Compaction.** Offline compaction deep-copies the reachable tree
  (content root and every checkpoint) into a fresh generation, reclaims
  the old generations with Oak's reclaim predicate, and rewrites the
  journal to a single line. Checkpoints and their shared root snapshots
  survive.
* **Never in place.** Existing archives with an index are immutable;
  every rewrite goes to a new file, and `journal.log` and `gc.log` are
  append-only (compaction's journal rewrite writes a fresh file and
  renames it into place). Damaged archives are backed up to `.bak` names,
  never truncated.

The full write-path contract, with the AEM safety invariant checklist for
each subsystem, is in [`analysis/`](analysis/) (`write-*.md`).
