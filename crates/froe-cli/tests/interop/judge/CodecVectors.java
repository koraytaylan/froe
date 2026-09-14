/*
 * Lucene's own output primitives, as known-answer vectors.
 *
 * froe's codec writer has to produce bytes Lucene reads, and the encodings
 * are small enough that a plausible-looking mistake survives every
 * self-consistent test: a `VInt` written big-endian round-trips through
 * froe's own reader, a packed block padded to eight bytes parses, and a
 * monotonic block whose average is computed in double reconstructs almost
 * every value. So the oracle is Lucene's own writers, run inside the image
 * over a fixed input set.
 *
 * The cases are chosen for the boundaries, not for coverage: the width at
 * which a `VInt` gains a byte, a negative `int` (five bytes, sign surviving
 * into the last group), a string whose UTF-8 length differs from its
 * character count, every packed bits-per-value, and the four block-packed
 * shapes whose headers differ — a large positive minimum, a negative
 * minimum, an all-equal block that carries no packed data at all, and a
 * delta wide enough to force the minimum to zero. The monotonic cases cover
 * the linear stream that carries no packed data either, one whose deltas
 * need bits, and the single-value block whose average is exactly `0f`.
 *
 * Prints `<primitive>\t<input>\t<hex>`, one case per line.
 */
import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Set;

import org.apache.lucene.store.DataOutput;
import org.apache.lucene.util.packed.PackedInts;

public final class CodecVectors {

    /** Collects bytes so each case can be printed on its own line. */
    private static final class Buffer extends DataOutput {
        private final ByteArrayOutputStream bytes = new ByteArrayOutputStream();

        @Override
        public void writeByte(byte b) {
            bytes.write(b);
        }

        @Override
        public void writeBytes(byte[] b, int offset, int length) {
            bytes.write(b, offset, length);
        }

        String hex() {
            StringBuilder rendered = new StringBuilder();
            for (byte b : bytes.toByteArray()) {
                rendered.append(String.format("%02x", b));
            }
            return rendered.toString();
        }
    }

    public static void main(String[] arguments) throws Exception {
        variableIntegers();
        variableLongs();
        fixedWidth();
        strings();
        collections();
        packedBlocks();
        blockPacked();
        monotonicBlockPacked();
    }

    private static void emit(String primitive, String input, Buffer buffer) {
        System.out.println(primitive + "\t" + input + "\t" + buffer.hex());
    }

    private static void variableIntegers() throws Exception {
        for (int value : new int[] {
            0, 1, 127, 128, 129, 16383, 16384, 2097151, 2097152,
            268435455, 268435456, Integer.MAX_VALUE, -1, Integer.MIN_VALUE,
        }) {
            Buffer buffer = new Buffer();
            buffer.writeVInt(value);
            emit("vint", Integer.toString(value), buffer);
        }
    }

    private static void variableLongs() throws Exception {
        for (long value : new long[] {
            0L, 1L, 127L, 128L, 16383L, 16384L, 2097151L, 2097152L,
            34359738367L, 34359738368L, 4398046511103L, 4398046511104L,
            Long.MAX_VALUE,
        }) {
            Buffer buffer = new Buffer();
            buffer.writeVLong(value);
            emit("vlong", Long.toString(value), buffer);
        }
    }

    private static void fixedWidth() throws Exception {
        for (int value : new int[] { 0, 1, -1, Integer.MIN_VALUE, Integer.MAX_VALUE }) {
            Buffer buffer = new Buffer();
            buffer.writeInt(value);
            emit("int", Integer.toString(value), buffer);
        }
        for (long value : new long[] { 0L, 1L, -1L, Long.MIN_VALUE, Long.MAX_VALUE }) {
            Buffer buffer = new Buffer();
            buffer.writeLong(value);
            emit("long", Long.toString(value), buffer);
        }
    }

    private static void strings() throws Exception {
        for (String value : new String[] {
            "", "a", "abc",
            // Two bytes, one character.
            "é",
            // Three bytes, one character.
            "中",
            // A surrogate pair: four bytes, two Java characters.
            "😀",
            // Long enough that the length itself takes two bytes.
            repeat("x", 200),
        }) {
            Buffer buffer = new Buffer();
            buffer.writeString(value);
            emit("string", escape(value), buffer);
        }
    }

    private static void collections() throws Exception {
        Buffer empty = new Buffer();
        empty.writeStringStringMap(new LinkedHashMap<String, String>());
        emit("stringmap", "{}", empty);

        Map<String, String> map = new LinkedHashMap<String, String>();
        map.put("one", "1");
        map.put("two", "2");
        Buffer twoEntries = new Buffer();
        twoEntries.writeStringStringMap(map);
        emit("stringmap", "{one=1,two=2}", twoEntries);

        Buffer emptySet = new Buffer();
        emptySet.writeStringSet(new LinkedHashSet<String>());
        emit("stringset", "[]", emptySet);

        Set<String> set = new LinkedHashSet<String>();
        set.add("_0.cfs");
        set.add("_0.si");
        Buffer twoNames = new Buffer();
        twoNames.writeStringSet(set);
        emit("stringset", "[_0.cfs,_0.si]", twoNames);
    }

    /**
     * The header-less packed writer at every width.
     *
     * The values are the low bits of an ascending sequence, so each width
     * exercises a different straddle of its byte boundaries.
     */
    private static void packedBlocks() throws Exception {
        final int valueCount = 33;
        for (int bits = 1; bits <= 64; ++bits) {
            Buffer buffer = new Buffer();
            PackedInts.Writer writer = PackedInts.getWriterNoHeader(
                    buffer, PackedInts.Format.PACKED, valueCount, bits, PackedInts.DEFAULT_BUFFER_SIZE);
            long mask = bits == 64 ? -1L : (1L << bits) - 1;
            StringBuilder input = new StringBuilder();
            for (int i = 0; i < valueCount; ++i) {
                long value = (long) i * 2654435761L & mask;
                writer.add(value);
                if (i > 0) {
                    input.append(',');
                }
                input.append(value);
            }
            writer.finish();
            emit("packed:" + bits, input.toString(), buffer);
        }
    }

    private static void blockPacked() throws Exception {
        long[][] cases = new long[][] {
            // A large positive minimum: epoch milliseconds.
            { 1789395510042L, 1789395510142L, 1789395510242L, 1789395510342L },
            // A negative minimum.
            { -5L, -3L, 0L, 7L },
            // All equal: bitsRequired == 0, so no packed data at all.
            { 42L, 42L, 42L, 42L },
            // A delta that overflows a signed long, forcing the minimum to 0.
            { Long.MIN_VALUE, Long.MAX_VALUE },
            // Zeros, the other all-equal shape.
            { 0L, 0L, 0L, 0L },
        };
        for (long[] values : cases) {
            Buffer buffer = new Buffer();
            // A block size of 64 is the minimum the writer accepts, and one
            // block is what each case is.
            org.apache.lucene.util.packed.BlockPackedWriter writer =
                    new org.apache.lucene.util.packed.BlockPackedWriter(buffer, 64);
            for (long value : values) {
                writer.add(value);
            }
            writer.finish();
            emit("blockpacked", render(values), buffer);
        }
    }

    private static void monotonicBlockPacked() throws Exception {
        long[][] cases = new long[][] {
            // Perfectly linear: every zigzag delta is 0, so the block
            // carries a vint 0 and no packed data.
            { 0L, 100L, 200L, 300L, 400L },
            // Deltas that need bits.
            { 0L, 3L, 17L, 18L, 900L },
            // One value: the average is exactly 0f.
            { 7L },
            // A non-zero start, so the minimum is the first value.
            { 1000L, 1001L, 1002L },
        };
        for (long[] values : cases) {
            Buffer buffer = new Buffer();
            org.apache.lucene.util.packed.MonotonicBlockPackedWriter writer =
                    new org.apache.lucene.util.packed.MonotonicBlockPackedWriter(buffer, 64);
            for (long value : values) {
                writer.add(value);
            }
            writer.finish();
            emit("monotonic", render(values), buffer);
        }
    }

    private static String render(long[] values) {
        StringBuilder rendered = new StringBuilder();
        for (int i = 0; i < values.length; ++i) {
            if (i > 0) {
                rendered.append(',');
            }
            rendered.append(values[i]);
        }
        return rendered.toString();
    }

    private static String repeat(String unit, int times) {
        StringBuilder rendered = new StringBuilder();
        for (int i = 0; i < times; ++i) {
            rendered.append(unit);
        }
        return rendered.toString();
    }

    /** A string rendered so one case fits one tab-separated line. */
    private static String escape(String value) {
        StringBuilder rendered = new StringBuilder();
        for (int i = 0; i < value.length(); ++i) {
            char character = value.charAt(i);
            if (character < 0x20 || character > 0x7e || character == '\\') {
                rendered.append(String.format("\\u%04x", (int) character));
            } else {
                rendered.append(character);
            }
        }
        return rendered.toString();
    }
}
