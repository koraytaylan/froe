# The Lucene 4.7.2 codec, from the write side

Byte-exact write-side specification of every file a **fresh, single-segment**
index carries under the `oakCodec` composition: what froe must emit, in what
order, with what encoding, and which reader-side check rejects each mistake.

This is the specification plan 0009 implements and plan 0010 consumes. It is
the write-side counterpart of
[`index-lucene-storage.md`](index-lucene-storage.md) §8, which specifies the
*read* side of the table of contents — the commit file, the per-segment
descriptor and the compound file's directory. That section is **cited, not
restated**: where this document needs a structure §8 already pins, it says so
and moves on.

Java sources cited below are the Lucene sources `oak-lucene` vendors, under
`oak-lucene/src/main/java/org/apache/lucene/`, which the bundle exports as
version `4.7.2-oak2`. The package prefix `org/apache/lucene/` is elided, so
`store/DataOutput.java` is
`oak-lucene/src/main/java/org/apache/lucene/store/DataOutput.java`. The
revision read is the one [`README.md`](README.md) pins, Apache Jackrabbit Oak
commit `4984c4cf26a7ca58ae9ce12c63190b7f492bda78`.

**The reader is the specification.** Every claim below is stated so that a
reader-side check names it: Lucene's own `CheckIndex` is the acceptance test
plan 0009 is held to, and a format decision nothing checks is recorded in the
quirks register (§9) rather than asserted.

Builds on (does not repeat):

- [`index-lucene-storage.md`](index-lucene-storage.md) — how each file is
  stored as repository content, and §8's read side of the table of contents.
- [`index-definitions.md`](index-definitions.md) — which definitions are
  fulltext-enabled, and therefore which select `oakCodec`.

---

## 1. Primitives

Everything below is written through `store/DataOutput.java`. These are its
encodings, and every later section is expressed in them.

### 1.1 Integers

```java
public final void writeVInt(int i) throws IOException {
  while ((i & ~0x7F) != 0) {
    writeByte((byte)((i & 0x7F) | 0x80));
    i >>>= 7;
  }
  writeByte((byte)i);
}

public final void writeVLong(long i) throws IOException {
  assert i >= 0L;
  while ((i & ~0x7FL) != 0L) {
    writeByte((byte)((i & 0x7FL) | 0x80L));
    i >>>= 7;
  }
  writeByte((byte)i);
}
```

* **`VInt`** — seven bits per byte, little-endian groups, continuation in the
  high bit. One to five bytes. The shift is `>>>`, so a **negative int is
  written as five bytes** with the sign bit surviving into the last group:
  `-1` is `FF FF FF FF 0F`. Several formats rely on that — a `-1` sentinel is
  a legal `VInt` and reads back as `-1`.
* **`VLong`** — the same, one to nine bytes, and **asserted non-negative**.
  The assertion is disabled in a production JVM, so a negative `VLong` is not
  *rejected*; it is simply never written by Lucene. froe must not write one
  either: `writeVLong(-1)` would emit nine bytes where the reader's own
  `readVLong` stops after `1 << 63` and yields a value no caller expects.
* **`Int` and `Long`** are **big-endian**, high byte first:

```java
public void writeInt(int i) throws IOException {
  writeByte((byte)(i >> 24));
  writeByte((byte)(i >> 16));
  writeByte((byte)(i >>  8));
  writeByte((byte) i);
}

public void writeLong(long i) throws IOException {
  writeInt((int) (i >> 32));
  writeInt((int) i);
}
```

That the fixed-width forms are big-endian while the variable-length forms are
little-endian-by-groups is the first quirk worth stating plainly, because a
writer that gets it backwards produces files whose *lengths* are right.

### 1.2 Strings, maps and sets

```java
public void writeString(String s) throws IOException {
  final BytesRef utf8Result = new BytesRef(10);
  UnicodeUtil.UTF16toUTF8(s, 0, s.length(), utf8Result);
  writeVInt(utf8Result.length);
  writeBytes(utf8Result.bytes, 0, utf8Result.length);
}

public void writeStringStringMap(Map<String,String> map) throws IOException {
  if (map == null) {
    writeInt(0);
  } else {
    writeInt(map.size());
    for(final Map.Entry<String, String> entry: map.entrySet()) {
      writeString(entry.getKey());
      writeString(entry.getValue());
    }
  }
}

public void writeStringSet(Set<String> set) throws IOException {
  if (set == null) {
    writeInt(0);
  } else {
    writeInt(set.size());
    for(String value : set) {
      writeString(value);
    }
  }
}
```

* A **string** is a `VInt` byte length then UTF-8 bytes. The length is in
  *bytes*, not characters.
* A **map** and a **set** open with a **fixed four-byte `Int`** count, not a
  `VInt` — the one place a count is not variable-length, and a writer that
  reaches for `writeVInt` here produces a file that parses for small counts
  and diverges at 128.
* **Iteration order is the collection's**, and Lucene does not sort. Two
  writers that disagree on order produce different bytes for the same content.
  Where froe must match Lucene byte for byte — the conformance comparison of
  task 0911 — the order is part of the claim, and froe writes the order the
  format's own producer would.

### 1.3 The codec header

`codecs/CodecUtil.java`:

* `CODEC_MAGIC = 0x3fd76c17`, written as a big-endian `Int`.
* `writeHeader(out, codec, version)` writes the magic, then the codec name as
  a **string** (ASCII, under 128 characters, or `IllegalArgumentException`),
  then the version as a big-endian `Int`.
* `headerLength(codec) = 9 + codec.length()` — four magic, one length byte,
  the name's bytes, four version. The `+ 1` for the length byte is why the
  name must stay under 128 characters: a longer name would take a two-byte
  `VInt` and the arithmetic would be wrong.

The reader's `checkHeader` refuses a wrong magic with `CorruptIndexException`,
a wrong name with `CorruptIndexException`, and a version outside
`[minVersion, maxVersion]` with `IndexFormatTooOldException` or
`IndexFormatTooNewException`. froe's own reader already implements this;
[`index-lucene-storage.md`](index-lucene-storage.md) §8.1 is the read side.

### 1.4 The norm byte

A norm is one byte, produced by the default similarity and quantized by
`util/SmallFloat.java`.

```java
public static byte floatToByte315(float f) {
    int bits = Float.floatToRawIntBits(f);
    int smallfloat = bits >> (24-3);
    if (smallfloat <= ((63-15)<<3)) {
      return (bits<=0) ? (byte)0 : (byte)1;
    }
    if (smallfloat >= ((63-15)<<3) + 0x100) {
      return -1;
    }
    return (byte)(smallfloat - ((63-15)<<3));
}
```

Three mantissa bits, five exponent bits, zero exponent 15. Underflow to a
positive value gives `1`, a non-positive input gives `0`, and overflow gives
`-1` — that is `0xFF`, not an error.

The value quantized is the length norm, from
`search/similarities/DefaultSimilarity.java`:

```java
public float lengthNorm(FieldInvertState state) {
  final int numTerms;
  if (discountOverlaps)
    numTerms = state.getLength() - state.getNumOverlap();
  else
    numTerms = state.getLength();
  return state.getBoost() * ((float) (1.0 / Math.sqrt(numTerms)));
}
```

**The arithmetic is not float throughout, and this is load-bearing.**
`1.0 / Math.sqrt(numTerms)` is evaluated in **double**, narrowed to float by
the explicit cast, and only then multiplied by the float boost. Computing the
reciprocal square root in float, or narrowing after the multiplication,
changes the last mantissa bit for some term counts — and since the result is
then quantized to three mantissa bits, most of those differences vanish and a
few do not. A writer that gets this wrong produces an index whose norms are
right almost everywhere, which is the hardest kind of wrong to find.

`discountOverlaps` defaults to **true**, so overlapping tokens — a synonym
filter's output at the same position — do not count toward the length.

---

## 2. Packed integers

Three writers, layered. Every later format embeds at least one of them, and
they are the only place in the codec where a *bit* width rather than a byte
width decides the layout.

### 2.1 The header-less packed writer

`util/packed/PackedInts.java`. Two formats, distinguished by an id the
enclosing format records:

| Format | id | Layout |
| --- | --- | --- |
| `PACKED` | 0 | every bit contiguous, no padding |
| `PACKED_SINGLE_BLOCK` | 1 | values padded so none straddles a 64-bit block |

Versions: `VERSION_START = 0`, `VERSION_BYTE_ALIGNED = 1`,
`VERSION_CURRENT = VERSION_BYTE_ALIGNED`. The byte count depends on the
version, and only the current one matters here:

```java
public long byteCount(int packedIntsVersion, int valueCount, int bitsPerValue) {
  if (packedIntsVersion < VERSION_BYTE_ALIGNED) {
    return 8L *  (long) Math.ceil((double) valueCount * bitsPerValue / 64);
  } else {
    return (long) Math.ceil((double) valueCount * bitsPerValue / 8);
  }
}
```

So at `VERSION_CURRENT` a `PACKED` run occupies
`ceil(valueCount * bitsPerValue / 8)` bytes — **byte**-aligned, not
long-aligned. A writer that pads to eight bytes produces a file whose every
subsequent offset is wrong.

Two helpers the enclosing formats use directly:

```java
public static int bitsRequired(long maxValue) {
  if (maxValue < 0) {
    throw new IllegalArgumentException("maxValue must be non-negative (got: " + maxValue + ")");
  }
  return Math.max(1, 64 - Long.numberOfLeadingZeros(maxValue));
}

public static long maxValue(int bitsPerValue) {
  return bitsPerValue == 64 ? Long.MAX_VALUE : ~(~0L << bitsPerValue);
}
```

`bitsRequired` never returns 0 — it is `max(1, …)`, so a run of zeros still
takes one bit per value. The *block* writers below have their own zero cases
and do not call it for an all-equal block.

**froe writes `PACKED` only.** `PACKED_SINGLE_BLOCK` is what Lucene's own
`fastestFormatAndBits` selects for 1, 2 and 4 bits under a `FASTEST`
overhead budget, and the reader dispatches on the id the enclosing format
recorded, so a `PACKED`-only writer is valid everywhere. It is a recorded
choice, not an oversight — see §9.

### 2.2 The block-packed writer

`util/packed/BlockPackedWriter.java`, over
`util/packed/AbstractBlockPackedWriter.java`. Values are buffered a block at
a time and each block is self-describing.

```java
static final int MIN_BLOCK_SIZE = 64;
static final int MAX_BLOCK_SIZE = 1 << (30 - 3);
static final int MIN_VALUE_EQUALS_0 = 1 << 0;
static final int BPV_SHIFT = 1;

static long zigZagEncode(long n) {
  return (n >> 63) ^ (n << 1);
}

// same as DataOutput.writeVLong but accepts negative values
static void writeVLong(DataOutput out, long i) throws IOException {
  int k = 0;
  while ((i & ~0x7FL) != 0L && k++ < 8) {
    out.writeByte((byte)((i & 0x7FL) | 0x80L));
    i >>>= 7;
  }
  out.writeByte((byte) i);
}
```

The class carries **its own `writeVLong`**, capped at nine bytes and
tolerating a negative value, because §1.1's asserts non-negative. A writer
that reuses the `DataOutput` one here is wrong for a negative minimum.

Per block:

```java
protected void flush() throws IOException {
  assert off > 0;
  long min = Long.MAX_VALUE, max = Long.MIN_VALUE;
  for (int i = 0; i < off; ++i) {
    min = Math.min(values[i], min);
    max = Math.max(values[i], max);
  }

  final long delta = max - min;
  final int bitsRequired = delta < 0 ? 64 : delta == 0L ? 0 : PackedInts.bitsRequired(delta);
  if (bitsRequired == 64) {
    // no need to delta-encode
    min = 0L;
  } else if (min > 0L) {
    // make min as small as possible so that writeVLong requires fewer bytes
    min = Math.max(0L, max - PackedInts.maxValue(bitsRequired));
  }

  final int token = (bitsRequired << BPV_SHIFT) | (min == 0 ? MIN_VALUE_EQUALS_0 : 0);
  out.writeByte((byte) token);

  if (min != 0) {
    writeVLong(out, zigZagEncode(min) - 1);
  }

  if (bitsRequired > 0) {
    if (min != 0) {
      for (int i = 0; i < off; ++i) {
        values[i] -= min;
      }
    }
    writeValues(bitsRequired);
  }

  off = 0;
}
```

Reading that off as a wire format, per block:

1. **A token byte**: `bitsRequired << 1`, with bit 0 set when the minimum is
   zero.
2. **The minimum**, only when it is *not* zero, as `zigZagEncode(min) - 1`
   through the negative-tolerant `writeVLong`. The `- 1` is not decoration:
   the zero case is already carried by the token bit, so the encoded range
   starts at one and the reader adds it back.
3. **The values**, only when `bitsRequired > 0`, delta-encoded against the
   minimum and packed at that width, occupying
   `PACKED.byteCount(VERSION_CURRENT, off, bitsRequired)` bytes.

Three cases produce **no value bytes at all**: an all-equal block
(`delta == 0`, so `bitsRequired == 0`), which is the common case for a run of
identical values; and the two shapes where the token alone says everything.

The minimum is *lowered* when possible — `max(0, max - maxValue(bits))` —
purely so its `VLong` is shorter. A writer that emits the true minimum
produces a longer but still readable block, and therefore a file that differs
from Lucene's byte for byte. froe reproduces the lowering.

`delta < 0` is signed overflow, not an error: it means the block spans more
than `Long.MAX_VALUE` and the writer gives up on delta-encoding, sets 64 bits
and a zero minimum.

### 2.3 The monotonic block-packed writer

`util/packed/MonotonicBlockPackedWriter.java`, for a stream that only ever
increases — an address or offset stream.

```java
protected void flush() throws IOException {
  assert off > 0;

  // TODO: perform a true linear regression?
  final long min = values[0];
  final float avg = off == 1 ? 0f : (float) (values[off - 1] - min) / (off - 1);

  long maxZigZagDelta = 0;
  for (int i = 0; i < off; ++i) {
    values[i] = zigZagEncode(values[i] - min - (long) (avg * i));
    maxZigZagDelta = Math.max(maxZigZagDelta, values[i]);
  }

  out.writeVLong(min);
  out.writeInt(Float.floatToIntBits(avg));
  if (maxZigZagDelta == 0) {
    out.writeVInt(0);
  } else {
    final int bitsRequired = PackedInts.bitsRequired(maxZigZagDelta);
    out.writeVInt(bitsRequired);
    writeValues(bitsRequired);
  }

  off = 0;
}
```

Per block:

1. **The minimum** — the block's *first* value, not its smallest — as a
   `VLong`, through `DataOutput`'s own (so it must be non-negative, which for
   an address stream it is).
2. **The average**, as the four bytes of `Float.floatToIntBits(avg)`, a
   big-endian `Int`. For a single-value block the average is **exactly
   `0f`**, not the value.
3. **A `VInt` bit width**, and the packed deltas — or **`VInt` 0 and nothing
   at all** when every delta is zero, which is what a perfectly linear
   address stream produces and is therefore the common case.

**The reader replays the float arithmetic, and froe must too.** The stored
delta is against `min + (long)(avg * i)`: a float multiply, then a truncation
toward zero. Computing that in double, or rounding instead of truncating,
reconstructs different values for large `i`. The average is a float precisely
so that both sides agree on the rounding, and its four bytes are in the file
so that neither side has to re-derive it.

`zigZagEncode(n) = (n >> 63) ^ (n << 1)` maps a signed delta onto the
non-negative range, small magnitudes to small values, which is what makes the
bit width small.

---

## 3. Segment descriptors and the commit file

The **read** side of all three structures is
[`index-lucene-storage.md`](index-lucene-storage.md) §8, which pins the
commit file's field order, the `.si`'s, the compound directory's and the
base-36 generation naming. This section adds only what a *writer* needs: the
order things are produced in, and the two places where the write side does
something the read side cannot see.

### 3.1 The `.si` file

`codecs/lucene46/Lucene46SegmentInfoWriter.java`. Codec name
`Lucene46SegmentInfo`, `VERSION_CURRENT = VERSION_START = 0`, extension `.si`.

```java
public void write(Directory dir, SegmentInfo si, FieldInfos fis, IOContext ioContext) throws IOException {
  final String fileName = IndexFileNames.segmentFileName(si.name, "", Lucene46SegmentInfoFormat.SI_EXTENSION);
  si.addFile(fileName);

  final IndexOutput output = dir.createOutput(fileName, ioContext);
  …
    CodecUtil.writeHeader(output, Lucene46SegmentInfoFormat.CODEC_NAME, Lucene46SegmentInfoFormat.VERSION_CURRENT);
    // Write the Lucene version that created this segment, since 3.1
    output.writeString(si.getVersion());
    output.writeInt(si.getDocCount());

    output.writeByte((byte) (si.getUseCompoundFile() ? SegmentInfo.YES : SegmentInfo.NO));
    output.writeStringStringMap(si.getDiagnostics());
    output.writeStringSet(si.files());
```

So: header, version string, document count as a fixed `Int`, the compound
flag as **one byte** (`YES = 1`, `NO = -1` — not 0), the diagnostics map, the
file set.

**Two facts about the file set, both invisible from the read side.**

1. **The writer adds its own name first.** `si.addFile(fileName)` runs
   *before* the file is written, so the `.si` lists itself. A writer that
   omits its own name produces a `.si` whose file set is one short, and
   nothing rejects it until a reader enumerates the segment's files and finds
   the descriptor missing.
2. **The compound names are already in place.** By the time the `.si` is
   written the compound-file creator has replaced the individual file names
   with `_0.cfs` and `_0.cfe`. The `.si` therefore lists exactly
   `_0.cfs`, `_0.cfe` and `_0.si` for a compound segment — three names, not
   the dozen files the index actually contains.

### 3.2 The commit file

`index/SegmentInfos.java`. The codec name is the bare string `segments`, and
the version written is `VERSION_46 = 1` (`VERSION_40 = 0` is the older one the
reader still accepts).

```java
CodecUtil.writeHeader(segnOutput, "segments", VERSION_46);
segnOutput.writeLong(version);
segnOutput.writeInt(counter); // write counter
segnOutput.writeInt(size()); // write infos
for (SegmentCommitInfo siPerCommit : this) {
  SegmentInfo si = siPerCommit.info;
  segnOutput.writeString(si.name);
  segnOutput.writeString(si.getCodec().getName());
  segnOutput.writeLong(siPerCommit.getDelGen());
  segnOutput.writeInt(siPerCommit.getDelCount());
  segnOutput.writeLong(siPerCommit.getFieldInfosGen());
  final Map<Long,Set<String>> genUpdatesFiles = siPerCommit.getUpdatesFiles();
  segnOutput.writeInt(genUpdatesFiles.size());
  for (Entry<Long,Set<String>> e : genUpdatesFiles.entrySet()) {
    segnOutput.writeLong(e.getKey());
    segnOutput.writeStringSet(e.getValue());
  }
  …
}
segnOutput.writeStringStringMap(userData);
```

then the checksum, which `ChecksumIndexOutput` appends on close.

For a fresh single-segment index froe writes: `version = 0`, `counter = 1`
(the next segment name would be `_1`), one info, `delGen = -1`,
`delCount = 0`, `fieldInfosGen = -1`, zero update-file entries, and an empty
user-data map.

**Neither `version` nor `counter` is unread**, and the quirks register
classifies both as *consumer-read*, never as unread:

* `version` is a change counter a directory reader compares against its own
  to decide whether an open reader is still current.
* `counter` is what an index writer reopening this index consumes to derive
  its next segment name.

A writer that emits arbitrary values for either produces an index that reads
correctly today and misbehaves the first time Oak reopens it to add a
segment.

**The file is written under a pending name and published by rename.** The
name comes from the generation:

```java
public String getNextSegmentFileName() {
  long nextGeneration;
  if (generation == -1) {
    nextGeneration = 1;
  } else {
    nextGeneration = generation+1;
  }
  return IndexFileNames.fileNameFromGeneration(IndexFileNames.SEGMENTS, "", nextGeneration);
}
```

`fileNameFromGeneration` is the base-36 rule
[`index-lucene-storage.md`](index-lucene-storage.md) §8.8 quotes, so a fresh
index's first commit is `segments_1` and the generation is 1.

### 3.3 `segments.gen`

Its format is [`index-lucene-storage.md`](index-lucene-storage.md) §8.2: the
format `-2`, then the generation written **twice** as a `Long`, so a torn
write is detectable. It is a hint: no commit's aggregate file set names it,
its absence is never a finding, and a reader that disagrees with it falls
back to the directory listing.

### 3.4 Which codec a definition selects

Oak's rule, not Lucene's, and specified in
[`index-definitions.md`](index-definitions.md): an explicit `codec` property
wins; otherwise a **fulltext-enabled** definition takes `oakCodec` and every
other takes `Lucene46`. Plan 0009 writes the `oakCodec` composition, which is
`Lucene46` with the postings format replaced — the composition itself is §4
onward.

---

## 4. Field infos — `.fnm`

`codecs/lucene46/Lucene46FieldInfosFormat.java` and its writer. Codec name
`Lucene46FieldInfos`, `FORMAT_CURRENT = FORMAT_START = 0`, extension `fnm`.

### 4.1 The bits

```java
static final byte IS_INDEXED = 0x1;
static final byte STORE_TERMVECTOR = 0x2;
static final byte STORE_OFFSETS_IN_POSTINGS = 0x4;
static final byte OMIT_NORMS = 0x10;
static final byte STORE_PAYLOADS = 0x20;
static final byte OMIT_TERM_FREQ_AND_POSITIONS = 0x40;
static final byte OMIT_POSITIONS = -128;
```

`OMIT_POSITIONS` is `-128`, that is `0x80` — the sign bit of a signed Java
byte. Written as a byte it is the same bit pattern either way, but a reader
implemented with an unsigned type must compare against `0x80` and not
against `-128`. `0x8` is **unused**, and there is no bit for
`DOCS_AND_FREQS_AND_POSITIONS`: that is the *absence* of the three
index-option bits.

### 4.2 The per-field record

```java
CodecUtil.writeHeader(output, Lucene46FieldInfosFormat.CODEC_NAME, Lucene46FieldInfosFormat.FORMAT_CURRENT);
output.writeVInt(infos.size());
for (FieldInfo fi : infos) {
  IndexOptions indexOptions = fi.getIndexOptions();
  byte bits = 0x0;
  if (fi.hasVectors()) bits |= Lucene46FieldInfosFormat.STORE_TERMVECTOR;
  if (fi.omitsNorms()) bits |= Lucene46FieldInfosFormat.OMIT_NORMS;
  if (fi.hasPayloads()) bits |= Lucene46FieldInfosFormat.STORE_PAYLOADS;
  if (fi.isIndexed()) {
    bits |= Lucene46FieldInfosFormat.IS_INDEXED;
    assert indexOptions.compareTo(IndexOptions.DOCS_AND_FREQS_AND_POSITIONS) >= 0 || !fi.hasPayloads();
    if (indexOptions == IndexOptions.DOCS_ONLY) {
      bits |= Lucene46FieldInfosFormat.OMIT_TERM_FREQ_AND_POSITIONS;
    } else if (indexOptions == IndexOptions.DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS) {
      bits |= Lucene46FieldInfosFormat.STORE_OFFSETS_IN_POSTINGS;
    } else if (indexOptions == IndexOptions.DOCS_AND_FREQS) {
      bits |= Lucene46FieldInfosFormat.OMIT_POSITIONS;
    }
  }
  output.writeString(fi.name);
  output.writeVInt(fi.number);
  output.writeByte(bits);

  // pack the DV types in one byte
  final byte dv = docValuesByte(fi.getDocValuesType());
  final byte nrm = docValuesByte(fi.getNormType());
  assert (dv & (~0xF)) == 0 && (nrm & (~0x0F)) == 0;
  byte val = (byte) (0xff & ((nrm << 4) | dv));
  output.writeByte(val);
  output.writeLong(fi.getDocValuesGen());
  output.writeStringStringMap(fi.attributes());
}
```

Per field, in order: **name** (string), **number** (`VInt`), **bits** (one
byte), **the packed types** (one byte), **`dvGen`** (a fixed eight-byte
`Long`), **attributes** (a string map).

The index-option bits are set **only when the field is indexed**, and only
three of the five options have a bit at all. The mapping is therefore:

| `IndexOptions` | bits set beyond `IS_INDEXED` |
| --- | --- |
| `DOCS_ONLY` | `OMIT_TERM_FREQ_AND_POSITIONS` (`0x40`) |
| `DOCS_AND_FREQS` | `OMIT_POSITIONS` (`0x80`) |
| `DOCS_AND_FREQS_AND_POSITIONS` | none |
| `DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS` | `STORE_OFFSETS_IN_POSTINGS` (`0x4`) |

A non-indexed field carries none of them whatever its options say.

### 4.3 The packed type byte

```java
private static byte docValuesByte(DocValuesType type) {
  if (type == null) {
    return 0;
  } else if (type == DocValuesType.NUMERIC) {
    return 1;
  } else if (type == DocValuesType.BINARY) {
    return 2;
  } else if (type == DocValuesType.SORTED) {
    return 3;
  } else if (type == DocValuesType.SORTED_SET) {
    return 4;
  } else {
    throw new AssertionError();
  }
}
```

**Norms type in the high nibble, doc-values type in the low**:
`(nrm << 4) | dv`. `0` means *absent*, which is why the enum starts at 1 and
why a field with neither writes a zero byte.

A norms type is always `NUMERIC` (1) when present, so the high nibble is `0`
or `1` and never anything else in an index froe writes.

### 4.4 Composition across documents

A field's `FieldInfo` is created by the first document that carries it and
then *reconciled* with each later one. The rules, which froe applies while
building the segment rather than while writing the file:

* **Index options downgrade to the lesser.** A field indexed with positions
  in one document and without in another ends as the weaker of the two, for
  the whole segment.
* **`omitNorms` is sticky once true.** A single document that omits norms
  omits them for every document of that field, and the norms already
  computed are discarded.
* **A doc-values type change is refused**, not reconciled: a field cannot be
  `NUMERIC` in one document and `BINARY` in another.

### 4.5 The over-long term rule

A term longer than the maximum term length is **skipped, and its document is
kept**. The document is indexed without that term rather than rejected. This
is a rule about what reaches the terms writer at all, and it is stated here
because `.fnm` is where a field first exists: a field whose every term is
over-long still gets a `FieldInfo`, and its postings are empty.

---

## 5. Stored fields — `.fdx` and `.fdt`

`codecs/lucene40/Lucene40StoredFieldsWriter.java`. Two files, two codec
headers, both at version 0:

```java
static final String CODEC_NAME_IDX = "Lucene40StoredFieldsIndex";
static final String CODEC_NAME_DAT = "Lucene40StoredFieldsData";
static final int VERSION_START = 0;
static final int VERSION_CURRENT = VERSION_START;
static final long HEADER_LENGTH_IDX = CodecUtil.headerLength(CODEC_NAME_IDX);
static final long HEADER_LENGTH_DAT = CodecUtil.headerLength(CODEC_NAME_DAT);
public static final String FIELDS_EXTENSION = "fdt";
public static final String FIELDS_INDEX_EXTENSION = "fdx";
```

`oakCodec` does **not** replace the stored-fields format, so this is the
`Lucene40` one — an older format than the `Lucene46` the rest of the
composition uses, and the version numbers restart at 0 for it.

### 5.1 `.fdx` — the index

One **eight-byte `Long`** per document, the offset of that document's record
in `.fdt`, written before the document's data:

```java
public void startDocument(int numStoredFields) throws IOException {
  indexStream.writeLong(fieldsStream.getFilePointer());
  fieldsStream.writeVInt(numStoredFields);
}
```

Fixed width, so document *n* is found at `HEADER_LENGTH_IDX + 8n` without
any search. That invariant is asserted when the writer finishes:

```java
public void finish(FieldInfos fis, int numDocs) {
  if (HEADER_LENGTH_IDX+((long) numDocs)*8 != indexStream.getFilePointer())
    throw new RuntimeException("fdx size mismatch: docCount is " + numDocs + " but fdx file size is " + …);
}
```

froe asserts the same thing at the same point. The comment in Lucene
attributes the check to a JRE bug; the reason to keep it is simpler — it is
the one cheap check that catches a document written without its index entry,
or an index entry written twice.

### 5.2 `.fdt` — the data

Per document: a `VInt` **stored-field count**, then that many field records.
Per field: a `VInt` **field number**, a **bits byte**, then the value.

```java
static final int FIELD_IS_BINARY = 1 << 1;

private static final int _NUMERIC_BIT_SHIFT = 3;
static final int FIELD_IS_NUMERIC_MASK = 0x07 << _NUMERIC_BIT_SHIFT;

static final int FIELD_IS_NUMERIC_INT = 1 << _NUMERIC_BIT_SHIFT;
static final int FIELD_IS_NUMERIC_LONG = 2 << _NUMERIC_BIT_SHIFT;
static final int FIELD_IS_NUMERIC_FLOAT = 3 << _NUMERIC_BIT_SHIFT;
static final int FIELD_IS_NUMERIC_DOUBLE = 4 << _NUMERIC_BIT_SHIFT;
```

So the bits byte is: bit 1 for binary, and a **three-bit numeric code at
shift 3**. Codes 5 and 6 (short and byte) exist in the source as comments and
are never written. **Bit 0 is unused** — the numeric mask starts at bit 3,
leaving bit 2 unused as well.

A string carries **no bit at all**: it is the case where neither the binary
bit nor any numeric code is set.

The values:

```java
if (bytes != null) {
  fieldsStream.writeVInt(bytes.length);
  fieldsStream.writeBytes(bytes.bytes, bytes.offset, bytes.length);
} else if (string != null) {
  fieldsStream.writeString(field.stringValue());
} else {
  if (number instanceof Byte || number instanceof Short || number instanceof Integer) {
    fieldsStream.writeInt(number.intValue());
  } else if (number instanceof Long) {
    fieldsStream.writeLong(number.longValue());
  } else if (number instanceof Float) {
    fieldsStream.writeInt(Float.floatToIntBits(number.floatValue()));
  } else if (number instanceof Double) {
    fieldsStream.writeLong(Double.doubleToLongBits(number.doubleValue()));
  }
}
```

| Value | Encoding |
| --- | --- |
| binary | `VInt` length, then the bytes |
| string | `writeString` — `VInt` UTF-8 byte length, then the bytes |
| int (or byte, or short) | **fixed four-byte big-endian `Int`** |
| long | **fixed eight-byte big-endian `Long`** |
| float | **`Float.floatToIntBits`, as a four-byte `Int`** |
| double | **`Double.doubleToLongBits`, as an eight-byte `Long`** |

**The floating types are stored as their raw bits, not as text and not
through any float-specific encoding** — the quirk §9 records, because nothing
else in this document pins it and a writer that formats a float as a string
produces a `.fdt` that parses and yields nonsense. A byte and a short are
**widened to four bytes** and read back as ints; the format has no narrower
form.

A stored field with no binary, string or numeric value is an
`IllegalArgumentException`, not an empty record.
