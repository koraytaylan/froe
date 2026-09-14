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

**The version string is `"4.7"`, not `"4.7.2"`.** `si.getVersion()` returns
what the index writer set, which is `Constants.LUCENE_MAIN_VERSION` —
major and minor only. The committed sample index Oak itself wrote carries
`03 34 2e 37` at that position, which is the three bytes `4.7`; the full
implementation version appears nowhere in a `.si`.

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

---

## 6. Postings — `.doc`, `.pos`, `.pay`

`codecs/lucene41/Lucene41PostingsWriter.java` and its format. This is the
format `oakCodec` **keeps** — the composition replaces nothing here — and it
is the largest single piece of the writer.

Four codec names and one version:

```java
final static String TERMS_CODEC = "Lucene41PostingsWriterTerms";
final static String DOC_CODEC = "Lucene41PostingsWriterDoc";
final static String POS_CODEC = "Lucene41PostingsWriterPos";
final static String PAY_CODEC = "Lucene41PostingsWriterPay";

final static int VERSION_START = 0;
final static int VERSION_META_ARRAY = 1;
final static int VERSION_CURRENT = VERSION_META_ARRAY;
```

Extensions `doc`, `pos`, `pay`; block size **128**
(`Lucene41PostingsFormat.BLOCK_SIZE`).

Which files exist depends on the field's index options: `.doc` always, `.pos`
when the field has positions, `.pay` when it has payloads or offsets.

### 6.1 The format table in `.doc`

Immediately after `.doc`'s codec header, `codecs/lucene41/ForUtil.java`
writes the table that tells a reader how each bit width is packed:

```java
ForUtil(float acceptableOverheadRatio, DataOutput out) throws IOException {
  out.writeVInt(PackedInts.VERSION_CURRENT);
  …
  for (int bpv = 1; bpv <= 32; ++bpv) {
    final FormatAndBits formatAndBits = PackedInts.fastestFormatAndBits(
        BLOCK_SIZE, bpv, acceptableOverheadRatio);
    …
    out.writeVInt(formatAndBits.format.getId() << 5 | (formatAndBits.bitsPerValue - 1));
  }
}
```

A `VInt` packed-integer version, then **32 `VInt`s**, one per bits-per-value
from 1 to 32, each the **format id shifted left five bits** with the
**bit width less one** beneath it. Five bits hold a width of 1..32 as 0..31,
and the id above.

**The encoded byte sizes are derived by the reader, never stored.** The
writer computes `encodedSizes` for itself and does not emit them; a reader
recomputes them from the format and width it just read. A writer that emits
sizes produces a table the reader misparses from the second entry on.

froe writes `PACKED` (id 0) for every width, so each entry is
`bitsPerValue - 1` — the table is the 32 values 0..31. Lucene's own
`fastestFormatAndBits` under the postings' overhead budget selects
`PACKED_SINGLE_BLOCK` for some widths, so froe's table differs from Lucene's
and both are valid: the reader honours whatever the table says. §9 records
it.

### 6.2 Blocks and the `VInt` tail

Doc deltas and frequencies accumulate into buffers of 128. A full buffer is
written as a packed block at the width the largest value needs. What remains
at the end of a term — fewer than 128 values — is written as a **`VInt`
tail**, and the two are distinguished by position, not by any marker: the
reader knows the document frequency and therefore knows how many full blocks
precede the tail.

```java
// docFreq == 1, don't write the single docid/freq to a separate file along with a pointer to it.
final int singletonDocID;
if (state.docFreq == 1) {
  // pulse the singleton docid into the term dictionary, freq is implicitly totalTermFreq
  singletonDocID = docDeltaBuffer[0];
} else {
  singletonDocID = -1;
  // vInt encode the remaining doc deltas and freqs:
  for(int i=0;i<docBufferUpto;i++) {
    final int docDelta = docDeltaBuffer[i];
    final int freq = freqBuffer[i];
    if (!fieldHasFreqs) {
      docOut.writeVInt(docDelta);
    } else if (freqBuffer[i] == 1) {
      docOut.writeVInt((docDelta<<1)|1);
    } else {
      docOut.writeVInt(docDelta<<1);
      docOut.writeVInt(freq);
    }
  }
}
```

In the tail, when the field has frequencies, **the delta and the frequency
share a `VInt`**: the delta is shifted left one and bit 0 means "frequency is
1". Any other frequency follows as a second `VInt`. A field without
frequencies writes the bare delta, unshifted — the same bytes meaning
different things depending on a field property the reader already knows.

### 6.3 Pulsing a single-document term

**A term whose document frequency is 1 writes nothing to `.doc` at all.** Its
single document id is carried in the term's metadata instead
(`singletonDocID`), and its frequency is implied by `totalTermFreq`, which
the terms dictionary already stores.

This happens **for every such term, whatever the index options** — it is not
conditional on frequencies being enabled. In a typical Oak index, where most
terms occur in one document, this is the majority of terms, and a writer that
emits a one-entry `VInt` tail for them instead produces a `.doc` that is both
larger and wrong: the reader will not look for it.

### 6.4 Term metadata

```java
public void encodeTerm(long[] longs, DataOutput out, FieldInfo fieldInfo, BlockTermState _state, boolean absolute) throws IOException {
  IntBlockTermState state = (IntBlockTermState)_state;
  if (absolute) {
    lastState = emptyState;
  }
  longs[0] = state.docStartFP - lastState.docStartFP;
  if (fieldHasPositions) {
    longs[1] = state.posStartFP - lastState.posStartFP;
    if (fieldHasPayloads || fieldHasOffsets) {
      longs[2] = state.payStartFP - lastState.payStartFP;
    }
  }
  if (state.singletonDocID != -1) {
    out.writeVInt(state.singletonDocID);
  }
  if (fieldHasPositions) {
    if (state.lastPosBlockOffset != -1) {
      out.writeVLong(state.lastPosBlockOffset);
    }
  }
  if (state.skipOffset != -1) {
    out.writeVLong(state.skipOffset);
  }
  lastState = state;
}
```

The metadata splits in two, and **both halves are written by the terms
dictionary, not here** — this method fills an array and appends to a byte
stream the terms writer owns (§7).

**The `longs`** — the three file pointers, as **deltas from the previous
term**, reset to absolute values at each block start (`absolute`, which sets
`lastState` to the empty state so the delta *is* the absolute value). How
many longs a field uses is `longsSize` in the terms directory: one without
positions, two with, three with payloads or offsets.

**The byte stream**, in exactly this source order:

1. `singletonDocID` as a **`VInt`**, only when not `-1`.
2. `lastPosBlockOffset` as a **`VLong`**, only for a field with positions and
   only when not `-1`.
3. `skipOffset` as a **`VLong`**, only when not `-1`.

Each is *omitted* rather than written as a sentinel, so the reader's ability
to parse the stream depends entirely on knowing the field's options and the
term's document frequency. The order is not negotiable and there is no
framing to recover from getting it wrong.

`lastPosBlockOffset` is set only when `totalTermFreq > BLOCK_SIZE` — the
offset of the last, partial position block, so a reader can jump to it
without walking the full ones.

### 6.5 Positions, offsets and payloads — `.pos` and `.pay`

`Lucene41PostingsWriter.addPosition` and the second half of `finishTerm`.

Three buffers of 128 fill in step with the document buffer, and **all three
hold deltas**:

```java
posDeltaBuffer[posBufferUpto] = position - lastPosition;
…
offsetStartDeltaBuffer[posBufferUpto] = startOffset - lastStartOffset;
offsetLengthBuffer[posBufferUpto] = endOffset - startOffset;
lastStartOffset = startOffset;
…
posBufferUpto++;
lastPosition = position;
```

`lastPosition` and `lastStartOffset` are reset to zero by `startDoc`, so a
position delta is measured **within its document** while the buffer itself
runs across document boundaries — the 128 positions in one block routinely
belong to several documents, and the first position of each is its absolute
value. An offset *length* is `end - start` and is not a delta against
anything; only the start is.

When the buffer fills, three or four packed blocks go out, in this order and
no other:

```java
if (posBufferUpto == BLOCK_SIZE) {
  forUtil.writeBlock(posDeltaBuffer, encoded, posOut);
  if (fieldHasPayloads) {
    forUtil.writeBlock(payloadLengthBuffer, encoded, payOut);
    payOut.writeVInt(payloadByteUpto);
    payOut.writeBytes(payloadBytes, 0, payloadByteUpto);
    payloadByteUpto = 0;
  }
  if (fieldHasOffsets) {
    forUtil.writeBlock(offsetStartDeltaBuffer, encoded, payOut);
    forUtil.writeBlock(offsetLengthBuffer, encoded, payOut);
  }
  posBufferUpto = 0;
}
```

The position deltas go to `.pos`; everything else goes to `.pay`, payload
lengths before offsets. **The position buffer is cleared here**, unlike the
document buffer, which `finishDoc` clears (§6.2) — nothing downstream needs
to see that the position block was just filled.

#### The `VInt` tail

Whatever is left in the buffers at `finishTerm` goes to `.pos` **alone** —
payload lengths and offsets that would have gone to `.pay` in a full block
are interleaved into `.pos` instead, so the tail's shape is not the block's
shape with fewer entries:

```java
int lastPayloadLength = -1;  // force first payload length to be written
int lastOffsetLength = -1;   // force first offset length to be written
for(int i=0;i<posBufferUpto;i++) {
  final int posDelta = posDeltaBuffer[i];
  if (fieldHasPayloads) {
    final int payloadLength = payloadLengthBuffer[i];
    if (payloadLength != lastPayloadLength) {
      lastPayloadLength = payloadLength;
      posOut.writeVInt((posDelta<<1)|1);
      posOut.writeVInt(payloadLength);
    } else {
      posOut.writeVInt(posDelta<<1);
    }
    if (payloadLength != 0) {
      posOut.writeBytes(payloadBytes, payloadBytesReadUpto, payloadLength);
      payloadBytesReadUpto += payloadLength;
    }
  } else {
    posOut.writeVInt(posDelta);
  }

  if (fieldHasOffsets) {
    int delta = offsetStartDeltaBuffer[i];
    int length = offsetLengthBuffer[i];
    if (length == lastOffsetLength) {
      posOut.writeVInt(delta << 1);
    } else {
      posOut.writeVInt(delta << 1 | 1);
      posOut.writeVInt(length);
      lastOffsetLength = length;
    }
  }
}
```

Two independent low-bit flags, each meaning "a length follows", each against
its own running value, and **the position delta is shifted only when the
field has payloads**. A field with offsets and no payloads writes the
position delta bare and then a shifted offset delta — the same `VInt` stream
carrying two different conventions one after the other, distinguishable only
by a reader that knows the field's options.

Both running lengths start at `-1` and are local to this loop, so the first
entry of every tail carries its length explicitly. They are *not* carried
over from the preceding full blocks, where every length was written
unconditionally.

`lastPosBlockOffset` (§6.4) is read **before** the tail is written:

```java
if (state.totalTermFreq > BLOCK_SIZE) {
  lastPosBlockOffset = posOut.getFilePointer() - posStartFP;
} else {
  lastPosBlockOffset = -1;
}
```

so it points at the tail's first byte, and it is `-1` for a term whose total
term frequency is exactly 128 — the boundary is strict, and a term with 128
positions has no tail at all.

**A pulsed term still writes its positions.** §6.3 suppresses the `.doc`
entry for a single-document term and nothing else; the tail, the blocks and
`posStartFP` are all as they would be for any other term.

### 6.6 The skip list

`codecs/lucene41/Lucene41SkipWriter.java` over
`codecs/MultiLevelSkipListWriter.java`. The skip list lives **inside `.doc`**,
appended after the term's postings, and `skipOffset` (§6.4) locates it
relative to the term's start.

Configuration, from the postings writer's constructor:

```java
skipWriter = new Lucene41SkipWriter(maxSkipLevels,   // 10
                                    BLOCK_SIZE,      // 128
                                    state.segmentInfo.getDocCount(),
                                    docOut, posOut, payOut);
// → super(skipInterval = 128, skipMultiplier = 8, maxSkipLevels = 10, df)
```

```java
if (df <= skipInterval) {
  numberOfSkipLevels = 1;
} else {
  numberOfSkipLevels = 1+MathUtil.log(df/skipInterval, skipMultiplier);
}
if (numberOfSkipLevels > maxSkipLevels) {
  numberOfSkipLevels = maxSkipLevels;
}
```

The `df` here is the **segment's document count**, not any term's, because
the writer is built once per segment. It fixes how many buffers exist and
how deep `bufferSkip` may climb; it does **not** decide how many levels a
given term emits.

#### Buffering a point

`startDoc` buffers one, and the condition is the subtle part:

```java
if (lastBlockDocID != -1 && docBufferUpto == 0) {
  skipWriter.bufferSkip(lastBlockDocID, docCount, lastBlockPosFP, lastBlockPayFP,
                        lastBlockPosBufferUpto, lastBlockPayloadByteUpto);
}
```

A point is buffered on the *first document after* a filled block, never on
the document that filled it, and it describes the state at the block's end:
the last document id in the closed block, the document count at that moment
(always a multiple of 128), and the `.pos`/`.pay` file pointers and buffer
offsets `finishDoc` latched. So **a term whose document count is exactly a
multiple of 128 buffers no point for its final block** — there is no
following document to trigger one.

How high a point climbs:

```java
assert df % skipInterval == 0;
int numLevels = 1;
df /= skipInterval;
while ((df % skipMultiplier) == 0 && numLevels < numberOfSkipLevels) {
  numLevels++;
  df /= skipMultiplier;
}

long childPointer = 0;
for (int level = 0; level < numLevels; level++) {
  writeSkipData(level, skipBuffer[level]);
  long newChildPointer = skipBuffer[level].getFilePointer();
  if (level != 0) {
    skipBuffer[level].writeVLong(childPointer);
  }
  childPointer = newChildPointer;
}
```

The point at document count 128 reaches level 0 only; the point at 1,024
reaches level 1; the point at 8,192 reaches level 2 — one more level per
factor of eight, capped at ten.

The child pointer written after a level-*n* entry is the offset **just past**
the level-(*n*−1) entry buffered in the same call, measured within that
level's own buffer. The reader rebases it onto where that level's region
landed in the file (`childPointer[level] = readVLong() + skipPointer[level-1]`),
which is why it is a buffer-relative offset and not a file pointer.

#### The payload of one point

```java
skipBuffer.writeVInt(curDoc - lastSkipDoc[level]);          lastSkipDoc[level] = curDoc;
skipBuffer.writeVInt((int)(curDocPointer - lastSkipDocPointer[level]));
                                                            lastSkipDocPointer[level] = curDocPointer;
if (fieldHasPositions) {
  skipBuffer.writeVInt((int)(curPosPointer - lastSkipPosPointer[level]));
                                                            lastSkipPosPointer[level] = curPosPointer;
  skipBuffer.writeVInt(curPosBufferUpto);
  if (fieldHasPayloads) {
    skipBuffer.writeVInt(curPayloadByteUpto);
  }
  if (fieldHasOffsets || fieldHasPayloads) {
    skipBuffer.writeVInt((int)(curPayPointer - lastSkipPayPointer[level]));
                                                            lastSkipPayPointer[level] = curPayPointer;
  }
}
```

Every delta is **per level**: each level keeps its own previous document and
its own previous pointers, so a level-1 entry's deltas span the eight
level-0 entries beneath it. `curPosBufferUpto` is an absolute offset into the
position block, not a delta.

`resetSkip`, called by `startTerm`, sets each level's previous document to
zero and each level's previous pointers to the **current** file pointers —
which are the term's start pointers — so the first point of every term is
measured from the term's own beginning.

#### Emitting the list

```java
long skipPointer = output.getFilePointer();
for (int level = numberOfSkipLevels - 1; level > 0; level--) {
  long length = skipBuffer[level].getFilePointer();
  if (length > 0) {
    output.writeVLong(length);
    skipBuffer[level].writeTo(output);
  }
}
skipBuffer[0].writeTo(output);
return skipPointer;
```

Highest level first, each above level 0 prefixed by its own `VLong` byte
length, level 0 last and unprefixed — its length is implied by the end of the
skip packet. An empty level is omitted entirely, **length prefix and all**.

And the list is written at all only for a term above the block size:

```java
if (docCount > BLOCK_SIZE) {
  skipOffset = skipWriter.writeSkip(docOut) - docStartFP;
} else {
  skipOffset = -1;
}
```

Strictly greater, so a term of exactly 128 documents records no skip offset —
consistent with its having buffered no point.

#### Why the missing final point is safe

The reader derives how many levels to expect from the term's own document
frequency, and would read one `VLong` length too many for a term whose
document count is a multiple of 1,024 — the writer buffers no point for a
final full block, so such a term never reaches the level the count implies.
`Lucene41SkipReader` closes the gap before the arithmetic happens:

```java
protected int trim(int df) {
  return df % blockSize == 0? df - 1: df;
}

public void init(long skipPointer, long docBasePointer, long posBasePointer, long payBasePointer, int df) {
  super.init(skipPointer, trim(df));
```

so `1+MathUtil.log(trim(df)/skipInterval, skipMultiplier)` is exactly the
highest level the writer reached. Confirmed against the pinned image:
terms of 129, 1,023, 1,024, 1,025, 8,191, 8,192 and 8,193 documents all
advance correctly through Lucene's own reader.

---

## 7. Terms — `.tim` and `.tip`

`codecs/BlockTreeTermsWriter.java`. Two files: the dictionary and the index
over it.

```java
public final static int DEFAULT_MIN_BLOCK_SIZE = 25;
public final static int DEFAULT_MAX_BLOCK_SIZE = 48;

static final int OUTPUT_FLAGS_NUM_BITS = 2;
static final int OUTPUT_FLAGS_MASK = 0x3;
static final int OUTPUT_FLAG_IS_FLOOR = 0x1;
static final int OUTPUT_FLAG_HAS_TERMS = 0x2;

static final String TERMS_EXTENSION = "tim";
final static String TERMS_CODEC_NAME = "BLOCK_TREE_TERMS_DICT";
public static final int TERMS_VERSION_CURRENT = TERMS_VERSION_META_ARRAY;   // 2

static final String TERMS_INDEX_EXTENSION = "tip";
final static String TERMS_INDEX_CODEC_NAME = "BLOCK_TREE_TERMS_INDEX";
public static final int TERMS_INDEX_VERSION_CURRENT = TERMS_INDEX_VERSION_META_ARRAY;   // 2
```

### 7.1 What `.tim` opens with

The block-tree header, and then **the postings writer's own header nested
inside it**: `Lucene41PostingsWriterTerms` at its `VERSION_CURRENT`, followed
by the block size 128 as a `VInt`. Two codec headers in one file, the second
belonging to a different format — a reader that stops after the first finds
the block size where it expects a field count.

### 7.2 Floor blocks, and why the partition is not free

A group of terms sharing a prefix that exceeds the maximum block size is
split into **floor blocks**. The rule: entries are grouped by the **byte
following the shared prefix**, a new floor block begins once the accumulated
count reaches the minimum, and that byte is recorded as the block's **lead
byte**.

**This is not a free choice.** The `.tip` floor payload keys each following
block by its lead byte, so a partition that splits one lead byte across two
floor blocks produces an index Lucene reads *wrongly* rather than refusing —
the second block becomes unreachable through the index and its terms
disappear from a seek. A writer may choose different block boundaries from
Lucene's and still be correct, but it may not split a lead byte.

### 7.3 The two flag bits, and where each lives

The two bits in the block's header **are not both in the block**:

```java
out.writeVInt((length<<1)|(isLastInFloor ? 1:0));
…
out.writeVInt((int) (suffixWriter.getFilePointer() << 1) | (isLeafBlock ? 1:0));
```

* **`isLastInFloor`** rides the block's **entry-count `VInt`**:
  `(entryCount << 1) | isLastInFloor`. It is **always set for a non-floor
  block**, which is the case a writer forgets.
* **`isLeafBlock`** rides the **suffix-section length `VInt`**:
  `(suffixBytes << 1) | isLeafBlock`.

Then the stats section and the metadata section (§6.4's `longs` and byte
stream). A leaf block is one with no sub-blocks, and its entries need no
per-entry sub-block pointer.

The two *other* flags — `OUTPUT_FLAG_IS_FLOOR` and `OUTPUT_FLAG_HAS_TERMS` —
live in the `.tip` transducer's outputs, not in the block at all (§7.5).

### 7.4 The field directory `.tim` closes with

```java
final long dirStart = out.getFilePointer();
final long indexDirStart = indexOut.getFilePointer();

out.writeVInt(fields.size());

for(FieldMetaData field : fields) {
  out.writeVInt(field.fieldInfo.number);
  out.writeVLong(field.numTerms);
  out.writeVInt(field.rootCode.length);
  out.writeBytes(field.rootCode.bytes, field.rootCode.offset, field.rootCode.length);
  if (field.fieldInfo.getIndexOptions() != IndexOptions.DOCS_ONLY) {
    out.writeVLong(field.sumTotalTermFreq);
  }
  out.writeVLong(field.sumDocFreq);
  out.writeVInt(field.docCount);
  if (TERMS_VERSION_CURRENT >= TERMS_VERSION_META_ARRAY) {
    out.writeVInt(field.longsSize);
  }
  indexOut.writeVLong(field.indexStartFP);
}
writeTrailer(out, dirStart);
writeIndexTrailer(indexOut, indexDirStart);
```

Per field: number, `numTerms`, the root code as a length and its bytes,
`sumTotalTermFreq` — **omitted entirely for a `DOCS_ONLY` field**, not
written as zero — `sumDocFreq`, `docCount`, and `longsSize`.

The same loop writes `.tip`'s `IndexStartFP` values, one `VLong` per field,
so the two files' field orders are the same by construction.

Both files then close with an **eight-byte `Long`** giving their directory's
start:

```java
protected void writeTrailer(IndexOutput out, long dirStart) throws IOException {
  out.writeLong(dirStart);
}
```

**The reader finds both directories by seeking to length minus eight.** That
is the only way in: nothing earlier in either file points at them.

### 7.5 `.tip` — the index

After the header, one transducer per field, then the `IndexStartFP` values
and the trailer above.

A transducer's output is a **`VLong`** carrying the block's file pointer with
the two flags beneath it: `(fp << 2) | hasTerms | isFloor`, using
`OUTPUT_FLAG_HAS_TERMS = 0x2` and `OUTPUT_FLAG_IS_FLOOR = 0x1`.

When `isFloor` is set the output continues with a **`VInt` count of following
blocks**, then per block a **lead byte** and
`((subFp - fp) << 1) | subHasTerms` as a **`VLong`** — the sub-block's
pointer as a delta from the floor head, with its own has-terms bit beneath.

This is where §7.2's constraint bites: the lead byte is the key, so one lead
byte must name exactly one following block.

### 7.6 The serialized transducer

`util/fst/FST.java`. The parts a writer emits, in order: the format version,
the **packed flag**, the empty output, the input type, the start node, the
node and arc counts, the **reversed byte store**, and then the arcs
themselves with their flag bytes (`BIT_FINAL_ARC` and the rest) and
byte-string outputs.

**The byte store opens with one pad byte**, written by the constructor
before any node:

```java
bytes = new BytesStore(bytesPageBits);
// pad: ensure no node gets address 0 which is reserved to mean
// the stop state w/ no arcs
bytes.writeByte((byte) 0);
```

A node's address is the index of its last byte, so the pad shifts every
address by one and keeps a real node from colliding with
`NON_FINAL_END_NODE`, which is the address 0. It also means **no
transducer's store is ever empty**: one carrying only the empty key has a
one-byte store, and Lucene's reading `BytesStore` — which indexes the last
block of the store it just read — cannot be constructed over a store of no
bytes at all. §10.4 records what omitting the pad costs.

Two choices froe makes, both recorded in §9 and both honoured by the reader:

* **Unpacked.** Lucene's own terms writer builds unpacked transducers, so
  unpacked is Lucene's own choice here and not a concession.
* **Linear arcs only.** Lucene emits the fixed-array form —
  an `ARCS_AS_FIXED_ARRAY` flags byte, a `VInt` arc count and a `VInt`
  bytes-per-arc — for a node with at least five arcs at depth three or less,
  or ten deeper. The reader dispatches on that flag **per node**, so a writer
  that only ever emits linear arcs produces a transducer Lucene reads
  correctly and seeks through more slowly.

### 7.7 How terms become blocks

`BlockTreeTermsWriter.TermsWriter.FindBlocks`, which replaces the
transducer builder's `freezeTail`. **The block partition is a side effect of
building a transducer that is then thrown away**: the writer feeds every
term into a `Builder<Object>` whose outputs are `NoOutputs`, and keeps only
what its frontier tells it.

The frontier is one node per depth. Adding a term walks it:

```java
int pos1 = 0;
int pos2 = input.offset;
final int pos1Stop = Math.min(lastInput.length, input.length);
while(true) {
  frontier[pos1].inputCount++;
  if (pos1 >= pos1Stop || lastInput.ints[pos1] != input.ints[pos2]) break;
  pos1++; pos2++;
}
final int prefixLenPlus1 = pos1+1;

freezeTail(prefixLenPlus1);

for(int idx=prefixLenPlus1;idx<=input.length;idx++) {
  frontier[idx-1].addArc(input.ints[input.offset+idx-1], frontier[idx]);
  frontier[idx].inputCount++;
}
…
lastNode.isFinal = true;
```

so every node from the root down to the term's own depth counts the term,
the nodes below the shared prefix are frozen, and the node at the term's
length is marked final. An **empty term** takes a separate path that only
marks the root final and counts it.

Freezing is where blocks are decided:

```java
for(int idx=lastInput.length; idx >= prefixLenPlus1; idx--) {
  final Builder.UnCompiledNode<Object> node = frontier[idx];
  long totCount = 0;
  if (node.isFinal) {
    totCount++;
  }
  for(int arcIdx=0;arcIdx<node.numArcs;arcIdx++) {
    final Builder.UnCompiledNode<Object> target = (…) node.arcs[arcIdx].target;
    totCount += target.inputCount;
    target.clear();
    node.arcs[arcIdx].target = null;
  }
  node.numArcs = 0;

  if (totCount >= minItemsInBlock || idx == 0) {
    writeBlocks(lastInput, idx, (int) totCount);
    node.inputCount = 1;
  } else {
    // stragglers!  carry count upwards
    node.inputCount = totCount;
  }
  frontier[idx] = new Builder.UnCompiledNode<Object>(blockBuilder, idx);
}
```

Deepest first. A node's count is **one per term ending exactly there plus
the counts its frozen children carry**, so the unit being counted is
"entries" — terms *and* the sub-blocks already written beneath. A node that
reaches the minimum writes its blocks and then reports **1** upward, because
what remains of it is a single sub-block entry; a node that does not reports
its whole count, and those stragglers are counted again by its parent.

`idx == 0` forces a block whatever the count, so **every field ends with
exactly one root block at the empty prefix**, however few entries it holds —
and, by §7.8's first condition, never a floored one. Finishing the field is
`freezeTail(0)`, which freezes the last term's whole path.

The assignment `node.inputCount = 1` is read by the *parent* on the next
turn of the same loop, through the arc that still points at the node; the
fresh node that replaces it at `frontier[idx]` is for the terms still to
come. A writer that replaces the node before its parent has read it loses
every count.

### 7.8 The floor partition, exactly

```java
void writeBlocks(IntsRef prevTerm, int prefixLength, int count) throws IOException {
  if (prefixLength == 0 || count <= maxItemsInBlock) {
    final PendingBlock nonFloorBlock = writeBlock(prevTerm, prefixLength, prefixLength, count, count, 0, false, -1, true);
    nonFloorBlock.compileIndex(null, scratchBytes);
    pending.add(nonFloorBlock);
  } else {
    …
  }
  lastBlockIndex = pending.size()-1;
}
```

**`prefixLength == 0` short-circuits first**, so the root block is a single
block however many entries it holds. Only a non-empty prefix with more than
`maxItemsInBlock` entries is floored.

The entries are then grouped by the byte following the prefix:

```java
int lastSuffixLeadLabel = -1;
int termCount = 0, subCount = 0, numSubs = 0;
for(PendingEntry ent : slice) {
  final int suffixLeadLabel = …;   // -1 when the term *is* the prefix
  if (suffixLeadLabel != lastSuffixLeadLabel && (termCount + subCount) != 0) {
    subBytes[numSubs] = lastSuffixLeadLabel;
    lastSuffixLeadLabel = suffixLeadLabel;
    subTermCounts[numSubs] = termCount;
    subSubCounts[numSubs] = subCount;
    termCount = subCount = 0;
    numSubs++;
  }
  if (ent.isTerm) termCount++; else subCount++;
}
subBytes[numSubs] = lastSuffixLeadLabel;
subTermCounts[numSubs] = termCount;
subSubCounts[numSubs] = subCount;
numSubs++;
```

**`subBytes[0]` is always `-1`.** The guard `(termCount + subCount) != 0`
suppresses the flush for the first entry, so `lastSuffixLeadLabel` is still
its initial `-1` when group 0 is closed — even when that entry's suffix
starts with an ordinary byte. Group 0 therefore holds exactly **one** entry
whenever the first entry's suffix is non-empty, and it carries a label that
is not its own.

That is harmless, and it is harmless for one reason: the segmenter below
cuts only when a group's running total reaches `minItemsInBlock`, which is
25, and a one-entry group never does. So group 0 always merges into the
first floor block, the lead byte it borrowed is never used as a key, and no
lead byte is split across two floor blocks — the thing §7.2 says a writer
must not do. Lucene's own `assert minItemsInBlock == 1 || subCount > 1`
says the same in the other direction.

The consequence that *is* used: the first floor block's `startLabel` is
`subBytes[0]`, which is `-1`, so its index prefix is the plain prefix and
not the prefix plus a lead byte. The `.tip` entry for a floored prefix is
therefore keyed exactly where an unfloored one would be.

```java
int pendingCount = 0;
int startLabel = subBytes[0];
int curStart = count;
for(int sub=0;sub<numSubs;sub++) {
  pendingCount += subTermCounts[sub] + subSubCounts[sub];
  if (pendingCount >= minItemsInBlock) {
    final int curPrefixLength = startLabel == -1 ? prefixLength : 1+prefixLength;
    if (startLabel != -1) prevTerm.ints[prevTerm.offset + prefixLength] = startLabel;
    final PendingBlock floorBlock = writeBlock(prevTerm, prefixLength, curPrefixLength, curStart, pendingCount,
                                               subTermCountSums[1+sub], true, startLabel, curStart == pendingCount);
    if (firstBlock == null) firstBlock = floorBlock; else floorBlocks.add(floorBlock);
    curStart -= pendingCount;
    pendingCount = 0;
    startLabel = subBytes[sub+1];
    if (curStart == 0) break;
    if (curStart <= maxItemsInBlock) {
      prevTerm.ints[prevTerm.offset + prefixLength] = startLabel;
      floorBlocks.add(writeBlock(prevTerm, prefixLength, prefixLength+1, curStart, curStart, 0, true, startLabel, true));
      break;
    }
  }
}
```

Greedy and forward: a block closes at the first group boundary that reaches
the minimum, and the remainder becomes one last block as soon as it fits
under the maximum — which may leave it **below** the minimum, a shape the
reader accepts and Lucene's own comment calls out. `isLastInFloor` is
`curStart == pendingCount`: this block consumes everything left.

Only `firstBlock` is added to `pending` and only it is indexed; the rest ride
in its floor payload (§7.5).

### 7.9 A block's body

```java
out.writeVInt((length<<1)|(isLastInFloor ? 1:0));
…
out.writeVInt((int) (suffixWriter.getFilePointer() << 1) | (isLeafBlock ? 1:0));
suffixWriter.writeTo(out);
out.writeVInt((int) statsWriter.getFilePointer());
statsWriter.writeTo(out);
out.writeVInt((int) metaWriter.getFilePointer());
metaWriter.writeTo(out);
```

Three length-prefixed sections after the two flagged counts of §7.3. The
sections are built in scratch buffers because each length precedes its
bytes.

**A block is a leaf when no entry in its slice is a sub-block.** Lucene
decides it from `lastBlockIndex`, the index in `pending` of the most
recently written block, with two short-circuits — a slice entirely below
that index has no sub-block, and a non-floor slice, which always runs to the
end of `pending`, necessarily contains it. Both reduce to the same
predicate.

**The suffix section**, per entry, with the prefix stripped:

| | leaf block | non-leaf block |
| --- | --- | --- |
| term | `VInt` suffix length, then the bytes | `VInt` `length << 1`, then the bytes |
| sub-block | — | `VInt` `(length << 1) \| 1`, the bytes, then a `VLong` of `startFP - block.fp` |

A leaf block spends no bit on the distinction because it has nothing to
distinguish. The sub-block pointer is a **backward** delta: the sub-block
was written before its parent, so the reader subtracts.

**The stats section**, terms only, sub-blocks contributing nothing:

```java
statsWriter.writeVInt(state.docFreq);
if (fieldInfo.getIndexOptions() != IndexOptions.DOCS_ONLY) {
  statsWriter.writeVLong(state.totalTermFreq - state.docFreq);
}
```

The total term frequency is stored as its **excess over the document
frequency**, which is never negative, and is omitted entirely for a
`DOCS_ONLY` field — the same omission as the directory's `sumTotalTermFreq`
in §7.4.

**The metadata section**, terms only, is §6.4's two halves interleaved per
term: `longsSize` `VLong`s of the pointer deltas, then that term's byte
stream. `absolute` is true for the **first term of each block** and false
after, and setting it resets the postings writer's previous state to the
empty one — so each block's first term carries absolute pointers and the
reader can start anywhere.

### 7.10 Building `.tip`, and the root code

`PendingBlock.compileIndex` builds one transducer per block: the block's own
prefix mapped to the §7.5 output bytes, then **every key of every sub-block's
transducer copied in**, enumerated with `BytesRefFSTEnum`. A floor head also
copies the keys of each following floor block's sub-indices.

Because a block's prefix is a prefix of all its sub-blocks' prefixes, and
sub-indices are merged in the order their entries appeared, the keys arrive
sorted and the merge is a concatenation. The field's root block therefore
ends up holding **one entry per indexed block in the field**, and that is
the transducer saved to `.tip`.

The root block's prefix is empty, so its output is the transducer's **empty
output** — and that is `rootCode` in §7.4's directory. It is written twice:
once inside the transducer and once in the directory, and a reader uses the
directory's copy to find the root block without consulting the index.

`indexStartFP` is where the field's transducer begins in `.tip`; the
directory loop of §7.4 writes one `VLong` per field, in the same field order
as `.tim`'s directory.

---

## 8. Doc values and norms

Two formats, deliberately similar and **not sharing their constants**. Using
one's numeric-format codes in the other is the mistake this section exists to
prevent.

### 8.1 Doc values — `.dvm` and `.dvd`

`codecs/lucene45/Lucene45DocValuesFormat.java` and its consumer.

```java
static final String DATA_CODEC = "Lucene45DocValuesData";
static final String DATA_EXTENSION = "dvd";
static final String META_CODEC = "Lucene45ValuesMetadata";
static final String META_EXTENSION = "dvm";
static final int VERSION_START = 0;
static final int VERSION_SORTED_SET_SINGLE_VALUE_OPTIMIZED = 1;
static final int VERSION_CURRENT = VERSION_SORTED_SET_SINGLE_VALUE_OPTIMIZED;
static final byte NUMERIC = 0;
static final byte BINARY = 1;
static final byte SORTED = 2;
static final byte SORTED_SET = 3;
```

and from the consumer:

```java
static final int BLOCK_SIZE = 16384;
static final int ADDRESS_INTERVAL = 16;

public static final int DELTA_COMPRESSED = 0;
public static final int GCD_COMPRESSED = 1;
public static final int TABLE_COMPRESSED = 2;

public static final int BINARY_FIXED_UNCOMPRESSED = 0;
public static final int BINARY_VARIABLE_UNCOMPRESSED = 1;
public static final int BINARY_PREFIX_COMPRESSED = 2;

public static final int SORTED_SET_WITH_ADDRESSES = 0;
public static final int SORTED_SET_SINGLE_VALUED_SORTED = 1;
```

**`.dvm` is the metadata and `.dvd` the payload.** Each `.dvm` field entry
opens with the field number and the type byte above, then a format-specific
body. Both `.dvm` and `.nvm` **close with a `VInt` `-1`**, which the
producers read as the loop terminator — a `-1` `VInt` is the five-byte form
of §1.1, and a writer that omits it leaves the reader consuming whatever
follows as another field number.

**`missingOffset`** is written after the format for numeric and binary
fields: the `.dvd` position of a one-bit-per-document missing bitset when any
document lacks the field, and **`-1`** otherwise.

**A terms dictionary** — used for `SORTED`, for a sorted-set's dictionary,
and for binary fields — takes `BINARY_FIXED_UNCOMPRESSED` when every value
has one length and `BINARY_PREFIX_COMPRESSED` otherwise, the latter with
`ADDRESS_INTERVAL = 16`.

**The sorted-set shapes.** Under
`VERSION_SORTED_SET_SINGLE_VALUE_OPTIMIZED` the consumer writes a format
`VInt`:

* `SORTED_SET_SINGLE_VALUED_SORTED` (1) when **no document carries more than
  one ordinal** — encoded exactly as `SORTED`, with `MISSING_ORD = -1` for a
  document that carries none.
* `SORTED_SET_WITH_ADDRESSES` (0) otherwise, which writes three further
  entries: the dictionary as a terms dictionary; the flat `ords` stream
  through the numeric path **with storage optimization off**; and a
  doc-to-ordinal index.

**That last entry's metadata is hard-coded and its declared format is
inert.** It writes `NUMERIC`, `DELTA_COMPRESSED`, `-1`, the packed-integer
version, the data pointer, `maxDoc` and the block size — but its *payload* is
a **monotonic** block-packed cumulative sum of the per-document ordinal
counts (§2.3), not a delta-compressed block-packed stream. The declared
format is never consulted for it. A writer that honours the declared format
here produces a file that parses and yields wrong ordinals.

#### 8.1.1 The numeric entry

`addNumericField`, which every numeric stream in this format goes through —
a `NUMERIC` field, a `SORTED` field's ordinals, and a sorted set's flat
`ords`.

The statistics pass runs **only under `optimizeStorage`**, and decides the
format:

```java
if (gcd != 1) {
  if (v < Long.MIN_VALUE / 2 || v > Long.MAX_VALUE / 2) {
    gcd = 1;                       // v - minValue might overflow
  } else if (count != 0) {         // minValue needs to be set first
    gcd = MathUtil.gcd(gcd, v - minValue);
  }
}
minValue = Math.min(minValue, v);
maxValue = Math.max(maxValue, v);
if (uniqueValues != null && uniqueValues.add(v) && uniqueValues.size() > 256) {
  uniqueValues = null;
}
```

```java
final long delta = maxValue - minValue;
if (uniqueValues != null
    && (delta < 0L || PackedInts.bitsRequired(uniqueValues.size() - 1) < PackedInts.bitsRequired(delta))
    && count <= Integer.MAX_VALUE) {
  format = TABLE_COMPRESSED;
} else if (gcd != 0 && gcd != 1) {
  format = GCD_COMPRESSED;
} else {
  format = DELTA_COMPRESSED;
}
```

Three things about that decision are easy to get wrong:

* **The table is not chosen just for having few distinct values.** It needs
  at most 256 of them *and* either a delta that **overflowed into the
  negative** — which one negative and one positive double's raw bits
  produce — or an ordinal narrower than the delta. Ten values 0..9 have
  four distinct-value bits against four delta bits, and land in
  `DELTA_COMPRESSED`.
* **The gcd is computed against `minValue` as it stands at the time**, which
  is the running minimum and not the final one, and it is skipped for the
  first value because there is no minimum yet.
* **One extreme value disables the gcd for the whole field.** Anything
  outside ±`Long.MAX_VALUE / 2` sets the divisor to 1 and never recovers, so
  `GCD_COMPRESSED` is unreachable for a double field of magnitude two or
  more, whose raw bits exceed that bound.

Then the entry, in this order:

```java
meta.writeVInt(field.number);
meta.writeByte(NUMERIC);
meta.writeVInt(format);
if (missing) { meta.writeLong(data.getFilePointer()); writeMissingBitset(values); }
else         { meta.writeLong(-1L); }
meta.writeVInt(PackedInts.VERSION_CURRENT);
meta.writeLong(data.getFilePointer());
meta.writeVLong(count);
meta.writeVInt(BLOCK_SIZE);
```

**The missing bitset is written into `.dvd` between two `.dvm` positions**,
so the data pointer that follows it is read after it has moved. One bit per
document, least significant bit first, a byte at a time:

```java
byte bits = 0;
int count = 0;
for (Object v : values) {
  if (count == 8) { data.writeByte(bits); count = 0; bits = 0; }
  if (v != null) { bits |= 1 << (count & 7); }
  count++;
}
if (count > 0) { data.writeByte(bits); }
```

A **set** bit means the document *has* the field, and the bitset exists only
when at least one document lacks it. Oak's `:dv<name>` fields are sparse, so
a writer that always wrote `-1` here would pass Lucene's own index checker
and still misreport which documents have the field.

The payload, per format:

| format | `.dvm` tail | `.dvd` |
| --- | --- | --- |
| `DELTA_COMPRESSED` | — | block-packed values, `BLOCK_SIZE` 16,384 |
| `GCD_COMPRESSED` | `Long` minimum, `Long` gcd | block-packed `(value - min) / gcd` |
| `TABLE_COMPRESSED` | `VInt` table length, then that many `Long`s | header-less packed ordinals at `bitsRequired(size - 1)` |

A missing value counts as **zero** in every branch, including the
statistics.

**The table's order is the iteration order of a `HashSet<Long>`**, which is
neither sorted nor specified. The reader uses the table as a lookup by
ordinal, so any order is valid; froe writes the values in ascending order
because a writer whose output depends on hash iteration has no vectors.
§10.3 records it.

#### 8.1.2 The binary entry and the terms dictionary

`addBinaryField` writes the values to `.dvd` as it measures them, so the
`.dvm` entry is written afterwards and its `startFP` points back:

```java
meta.writeVInt(minLength == maxLength ? BINARY_FIXED_UNCOMPRESSED : BINARY_VARIABLE_UNCOMPRESSED);
if (missing) { meta.writeLong(data.getFilePointer()); writeMissingBitset(values); }
else         { meta.writeLong(-1L); }
meta.writeVInt(minLength);
meta.writeVInt(maxLength);
meta.writeVLong(count);
meta.writeLong(startFP);
if (minLength != maxLength) {
  meta.writeLong(data.getFilePointer());
  meta.writeVInt(PackedInts.VERSION_CURRENT);
  meta.writeVInt(BLOCK_SIZE);
  // one monotonic address per value: the cumulative length
}
```

A fixed-length field needs no addresses at all — the reader multiplies.

`addTermsDict` is the dictionary a `SORTED` field and a sorted set share.
**When every value has the same length it is an ordinary binary field**, and
`addBinaryField` then writes its *own* field number and `BINARY` type byte —
so a `SORTED` field's `.dvm` carries the field number three times, once for
the `SORTED` entry, once for the dictionary and once for the ordinals.

Otherwise it is prefix-compressed:

```java
meta.writeVInt(field.number);
meta.writeByte(BINARY);
meta.writeVInt(BINARY_PREFIX_COMPRESSED);
meta.writeLong(-1L);
…
for (BytesRef v : values) {
  if (count % ADDRESS_INTERVAL == 0) {
    termAddresses.add(data.getFilePointer() - startFP);
    lastTerm.length = 0;               // force the first term of a block absolute
  }
  int sharedPrefix = StringHelper.bytesDifference(lastTerm, v);
  data.writeVInt(sharedPrefix);
  data.writeVInt(v.length - sharedPrefix);
  data.writeBytes(v.bytes, v.offset + sharedPrefix, v.length - sharedPrefix);
  lastTerm.copyBytes(v);
  count++;
}
final long indexStartFP = data.getFilePointer();
termAddresses.finish();
addressBuffer.writeTo(data);
meta.writeVInt(minLength);
meta.writeVInt(maxLength);
meta.writeVLong(count);
meta.writeLong(startFP);
meta.writeVInt(ADDRESS_INTERVAL);
meta.writeLong(indexStartFP);
meta.writeVInt(PackedInts.VERSION_CURRENT);
meta.writeVInt(BLOCK_SIZE);
```

Every sixteenth term is absolute — its shared prefix is forced to zero by
emptying the running term — and its offset is recorded in a **monotonic**
block-packed stream built in memory and appended after the terms. The
missing offset is a hard `-1`: a dictionary has no missing values.

#### 8.1.3 Sorted and sorted set

```java
public void addSortedField(FieldInfo field, Iterable<BytesRef> values, Iterable<Number> docToOrd) {
  meta.writeVInt(field.number);
  meta.writeByte(SORTED);
  addTermsDict(field, values);
  addNumericField(field, docToOrd, false);
}
```

**`optimizeStorage` is false for the ordinal stream**, which is why a
`SORTED` field's ordinals are always `DELTA_COMPRESSED`, carry no missing
bitset — `missingOffset` is `-1` because the statistics pass that would set
`missing` never runs — and use `-1` for a document with no value. The three
entries share one field number.

A sorted set opens with its own `SORTED_SET` entry and then a format `VInt`:

```java
if (isSingleValued(docToOrdCount)) {
  meta.writeVInt(SORTED_SET_SINGLE_VALUED_SORTED);
  addSortedField(field, values, /* ordCount == 0 ? MISSING_ORD : the ord */);
  return;
}
meta.writeVInt(SORTED_SET_WITH_ADDRESSES);
addTermsDict(field, values);
addNumericField(field, ords, false);
meta.writeVInt(field.number);
meta.writeByte(NUMERIC);
meta.writeVInt(DELTA_COMPRESSED);
meta.writeLong(-1L);
meta.writeVInt(PackedInts.VERSION_CURRENT);
meta.writeLong(data.getFilePointer());
meta.writeVLong(maxDoc);
meta.writeVInt(BLOCK_SIZE);
// a MONOTONIC block-packed cumulative sum of docToOrdCount
```

`isSingleValued` is "no document carries **more than one** ordinal", so a
set where some documents carry none and the rest exactly one still takes the
optimized form, with `MISSING_ORD = -1` for the empty ones.

The addresses form's third entry is the one §8.1 flags: **its declared
format is inert.** The metadata says `DELTA_COMPRESSED` and its count is
`maxDoc` rather than the stream's own length, but the payload is a
*monotonic* cumulative sum, and the reader decodes it monotonically without
consulting the declaration. The `ords` entry before it is an ordinary
numeric entry whose count is the **number of ordinals**, not the number of
documents — the two counts in one field's metadata mean different things.

### 8.2 Norms — `.nvm` and `.nvd`

`codecs/lucene42/Lucene42NormsConsumer.java`. **The `Lucene42` format, not
`Lucene45`**, with its own version and its own numeric-format codes:

```java
static final int VERSION_START = 0;
static final int VERSION_GCD_COMPRESSION = 1;
static final int VERSION_CURRENT = VERSION_GCD_COMPRESSION;

static final byte NUMBER = 0;

static final int BLOCK_SIZE = 4096;

static final byte DELTA_COMPRESSED = 0;
static final byte TABLE_COMPRESSED = 1;
static final byte UNCOMPRESSED = 2;
static final byte GCD_COMPRESSED = 3;
```

Note the codes **differ from §8.1's**: here `TABLE_COMPRESSED` is 1 and
`GCD_COMPRESSED` is 3, where the doc-values format has them 2 and 1. The
block size is 4,096, not 16,384. Two formats, four names in common, none of
the values shared.

**A norms field always takes `UNCOMPRESSED`.** The norms format asks packed
integers for the fastest decode, and a norm byte needs all eight bits, so the
selection lands on the uncompressed form every time — one byte per document,
straight through.

The codec names the `Lucene42` norms format writes are
**`Lucene41NormsData`** and **`Lucene41NormsMetadata`** — *41*, not 42. The
format was renamed and the header strings were not. §9 records it; a writer
that "corrects" them produces files the reader refuses with
`CorruptIndexException`.

---

## 9. Compound files — `.cfs` and `.cfe`

`store/CompoundFileWriter.java`.

```java
static final String DATA_CODEC = "CompoundFileWriterData";
static final int VERSION_CURRENT = VERSION_START;   // 0
static final String ENTRY_CODEC = "CompoundFileWriterEntries";
```

`.cfs` is its codec header followed by the concatenated file contents. `.cfe`
is the directory:

```java
protected void writeEntryTable(Collection<FileEntry> entries,
    IndexOutput entryOut) throws IOException {
  CodecUtil.writeHeader(entryOut, ENTRY_CODEC, VERSION_CURRENT);
  entryOut.writeVInt(entries.size());
  for (FileEntry fe : entries) {
    entryOut.writeString(IndexFileNames.stripSegmentName(fe.file));
    entryOut.writeLong(fe.offset);
    entryOut.writeLong(fe.length);
  }
}
```

A `VInt` entry count, then per entry the **segment-stripped name** — `.fdt`,
never `_0.fdt` — and two **eight-byte `Long`s**, offset then length. That
namespace is the one
[`index-lucene-storage.md`](index-lucene-storage.md) §8.5 reads back, and
froe's `strip_segment_name` already implements the stripping.

**What stays outside the compound file**: `.si` and `segments_N`. Everything
else a segment owns goes in.

---

## 10. The quirks register

Every constant copied unchanged across versions, every field written but
unread, and every place a valid alternative encoding exists with the one froe
chooses. A reader of this document who needs to know "why is it like that"
should find the answer here rather than in a commit message.

### 10.1 Constants that survived a rename

| Where | What |
| --- | --- |
| `Lucene42` norms | writes the codec names **`Lucene41NormsData`** and **`Lucene41NormsMetadata`**. The format was renamed; the header strings were not. Correcting them makes the reader refuse the file. |
| `Lucene40` stored fields | `oakCodec` keeps the *40* stored-fields format inside an otherwise *46* composition, and its versions restart at 0. |
| `.si` compound flag | `SegmentInfo.NO` is **`-1`**, not 0. |
| `OMIT_POSITIONS` | is `-128`, the sign bit of a signed Java byte, where every other field-info bit is a small positive mask. |

### 10.2 Written, and read by whom

Nothing in this composition is written and *never* read. Two fields look
unread and are not, and the register classifies them as **consumer-read**:

| Field | Read by |
| --- | --- |
| `segments_N`'s `version` | a directory reader, comparing it against its own to decide whether an open reader is still current |
| `segments_N`'s `counter` | an index writer reopening the index, to derive its next segment name |

A writer that emits arbitrary values for either produces an index that reads
correctly today and misbehaves the first time Oak reopens it to add a
segment. Neither belongs in an "unread, so anything goes" list.

### 10.3 Valid alternatives, and froe's choice

| Decision | Lucene | froe | Why both are valid |
| --- | --- | --- | --- |
| packed format per bit width | `fastestFormatAndBits` selects `PACKED_SINGLE_BLOCK` for 1, 2 and 4 bits | `PACKED` everywhere | the reader honours the id the `.doc` format table records, per width |
| all-equal block | emits the `ALL_VALUES_EQUAL` escape — bits-per-value 0, then one `VInt` | the same | not an alternative; the block writers' `bitsRequired == 0` path is this escape |
| transducer arcs | the fixed-array form (`ARCS_AS_FIXED_ARRAY`, a `VInt` arc count, a `VInt` bytes-per-arc) for a node with ≥5 arcs at depth ≤3 or ≥10 deeper | linear arcs only | the reader dispatches on the flags byte **per node**; linear is slower to seek and correct everywhere |
| transducer packing | unpacked, from its own terms writer | unpacked | Lucene's own choice here, not a concession |
| the `TABLE_COMPRESSED` value table (§8.1.1) | the iteration order of a `HashSet<Long>` | ascending | the reader uses the table as a lookup by ordinal; a writer whose output depends on hash iteration has no vectors |

### 10.4 The transducer pad, and what omitting it costs

**A transducer's byte store opens with one zero byte** (§7.6), so no node
gets the address 0 that `NON_FINAL_END_NODE` already means.

Task 0903 omitted it, and the omission looked harmless: every address was
one lower than Lucene's, consistently, and the reader follows addresses
rather than checking them. The one shape it broke was the transducer
carrying **only the empty key**, whose store was then empty — and
`BytesStore`'s reading constructor indexes the last block of a store with no
blocks. Running Lucene's own reader over froe's output found it, and task
0903 recorded the shape as write-only on the reasoning that a `.tip` never
holds one.

**That reasoning was wrong, and task 0905 is where it showed.** A field
whose terms all fit in one root block — every field of fewer than 49 terms,
which is most of them — produces exactly that transducer: the root block's
prefix is empty, so its code becomes the empty output and there is no other
key. froe would have written an unreadable `.tip` for the common case.

The pad is the whole fix. With it, froe's node addresses are Lucene's, the
empty-key-only transducer has a one-byte store, and Lucene's reader
enumerates it like any other — which the corpus now includes it to check.

### 10.5 The raw-bits rule

**A stored float is `Float.floatToIntBits` as a four-byte big-endian `Int`,
and a stored double is `Double.doubleToLongBits` as an eight-byte
big-endian `Long`.** No other section of this document pins it and no other
task in plan 0009 owns it, so it is stated here as its own rule: a writer
that formats either as text, or that uses a float-specific encoding,
produces a `.fdt` that parses cleanly and yields nonsense.

A stored `byte` or `short` is **widened to four bytes** and read back as an
int; the format has no narrower numeric form, and codes 5 and 6 exist only as
comments in the source.

---

## 11. Feasibility verdict

Per module, against the thousand-line limit `scripts/oversized-files.sh`
enforces, with the sources read to make the estimate.

| Module | Sources read | Estimate | Notes |
| --- | --- | --- | --- |
| primitives (`VInt`/`VLong`/string/map/set, codec header, small float, norm) | `store/DataOutput.java`, `codecs/CodecUtil.java`, `util/SmallFloat.java`, `search/similarities/DefaultSimilarity.java` | ~250 | froe already has the *read* side of the header from plan 0008; this is its mirror |
| packed integers (header-less, block-packed, monotonic) | `util/packed/PackedInts.java`, `AbstractBlockPackedWriter.java`, `BlockPackedWriter.java`, `MonotonicBlockPackedWriter.java` | ~450 | the bit-packing encoder is the bulk; one file |
| transducer builder and serializer | `util/fst/FST.java`, `util/fst/Builder.java` | ~900 | **the tightest fit.** Linear arcs only (§10.3) is what keeps it under the limit; the fixed-array form would add ~200 |
| postings (`.doc`/`.pos`/`.pay`, skip list) | `codecs/lucene41/Lucene41PostingsWriter.java`, `ForUtil.java`, `Lucene41SkipWriter.java`, `codecs/MultiLevelSkipListWriter.java` | ~800 | splits naturally at the skip writer if it grows |
| block-tree terms (`.tim`/`.tip`) | `codecs/BlockTreeTermsWriter.java` | ~700 | the floor-block partition (§7.2) is the subtle part, not the bulk |
| stored fields (`.fdx`/`.fdt`) | `codecs/lucene40/Lucene40StoredFieldsWriter.java` | ~200 | |
| doc values (`.dvm`/`.dvd`) | `codecs/lucene45/Lucene45DocValuesConsumer.java`, its format | ~600 | the sorted-set shapes (§8.1) dominate |
| norms (`.nvm`/`.nvd`) | `codecs/lucene42/Lucene42NormsConsumer.java` | ~150 | one format, always `UNCOMPRESSED` |
| field infos (`.fnm`) | `codecs/lucene46/Lucene46FieldInfosWriter.java`, its format | ~200 | |
| segment descriptor and commit file | `codecs/lucene46/Lucene46SegmentInfoWriter.java`, `index/SegmentInfos.java` | ~250 | the read side is plan 0008's |
| compound file (`.cfs`/`.cfe`) | `store/CompoundFileWriter.java` | ~200 | froe already reads both |

**No format feature the consumer needs is unwritable.** Everything Oak's
`oakCodec` composition produces for a fresh single-segment index from
pre-tokenized documents is specified above, and each format's reader accepts
the subset froe chooses: `PACKED`-only packing, linear transducer arcs, and
the `UNCOMPRESSED` norms form are all honoured by the reader's own dispatch.

Two things are deliberately **out of scope** and neither is needed:
**merging** — froe writes one segment and never merges — and **deletions**,
which plan 0008's transport already handles as a file the commit references
rather than a file froe writes.

**Verdict: go.** The largest single module is the transducer serializer at
roughly 900 lines, which fits under the limit only because froe emits linear
arcs; if it does not fit in practice, the split is at the builder/serializer
seam and is recorded here in advance so that the split is a planned one
rather than a surprise during task 0903.
