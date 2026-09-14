/*
 * The terms Lucene's numeric fields produce, and the epoch millisecond
 * Oak converts a DATE to, for froe to replay.
 *
 * `numeric-vectors <out>` prints two tables:
 *
 *   * for a list of long, integer and double values, every term the
 *     field's own token stream produces — one per shift level at the
 *     field type's precision step — as hex, with its position increment;
 *   * for a list of date strings, what `FieldFactory.dateToLong` makes of
 *     them: the epoch millisecond, or the refusal it throws.
 *
 * The date half is `org.apache.jackrabbit.util.ISO8601.parse`, and four
 * jars in the pinned image carry a copy of that class. The one that
 * counts is `jackrabbit-jcr-commons`, the bundle that *exports* the
 * package in OSGi; the others embed it privately. This class refuses to
 * run unless the copy it loaded came from that jar, because a flat class
 * path would otherwise let any of the four answer.
 */
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;

import org.apache.lucene.analysis.TokenStream;
import org.apache.lucene.analysis.tokenattributes.PositionIncrementAttribute;
import org.apache.lucene.analysis.tokenattributes.TermToBytesRefAttribute;
import org.apache.lucene.document.DoubleField;
import org.apache.lucene.document.Field;
import org.apache.lucene.document.IntField;
import org.apache.lucene.document.LongField;
import org.apache.lucene.util.BytesRef;

import org.apache.jackrabbit.util.ISO8601;

public final class NumericVectors {

    /** The jar whose copy of `ISO8601` is the one OSGi resolves. */
    private static final String EXPECTED_SOURCE = "jackrabbit-jcr-commons";

    private static final long[] LONGS = {
        0L, 1L, -1L, 2L, -2L, 15L, 16L, 42L, -42L, 255L, 256L,
        4294967295L, 4294967296L, -4294967296L,
        1330605045678L, -62167392000000L,
        Long.MAX_VALUE, Long.MIN_VALUE,
    };

    private static final int[] INTEGERS = {
        0, 1, -1, 2, -2, 15, 16, 42, -42, 255, 256, 65535, 65536,
        Integer.MAX_VALUE, Integer.MIN_VALUE,
    };

    private static final double[] DOUBLES = {
        0.0d, -0.0d, 1.0d, -1.0d, 0.5d, -0.5d, 3.14d, -3.14d,
        Double.MIN_VALUE, Double.MAX_VALUE,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
    };

    private static final String[] DATES = {
        // the form Oak itself writes
        "2012-03-01T12:30:45.678+01:00",
        "2012-03-01T12:30:45.678Z",
        "2012-03-01T12:30:45.678-05:30",
        "2012-03-01T12:30:45.678+00:00",
        "2012-03-01T12:30:45.678-00:00",
        "1970-01-01T00:00:00.000Z",
        "1969-12-31T23:59:59.999Z",
        // the milliseconds are mandatory, the zone is not
        "2012-03-01T12:30:45Z",
        "2012-03-01T12:30:45.678",
        "2012-03-01T12:30:45.67Z",
        "2012-03-01T12:30:45.6780",
        "2012-03-01T12:30:45.6780Z",
        // zone designators at the edges of what Java normalizes back
        "2012-03-01T12:30:45.678+05:45",
        "2012-03-01T12:30:45.678+14:00",
        "2012-03-01T12:30:45.678+23:59",
        "2012-03-01T12:30:45.678+24:00",
        "2012-03-01T12:30:45.678+01:60",
        "2012-03-01T12:30:45.678+1:00",
        "2012-03-01T12:30:45.678+0100",
        "2012-03-01T12:30:45.678+0",
        "2012-03-01T12:30:45.678-0",
        "2012-03-01T12:30:45.678+00",
        "2012-03-01T12:30:45.678Z0",
        "2012-03-01T12:30:45.678UTC",
        "2012-03-01T12:30:45.678-12:00",
        "2012-03-01T12:30:45.678+13:00",
        "2012-03-01T12:30:45.678 Z",
        // the calendar is not lenient
        "2012-13-01T12:30:45.678Z",
        "2012-00-01T12:30:45.678Z",
        "2012-02-30T12:30:45.678Z",
        "2012-02-29T12:30:45.678Z",
        "2013-02-29T12:30:45.678Z",
        "2012-03-01T24:00:00.000Z",
        "2012-03-01T12:60:00.000Z",
        "2012-03-01T12:30:60.000Z",
        "2012-03-01T12:30:45.999Z",
        // the era, the astronomical year and its four-digit bound
        "0000-01-01T00:00:00.000Z",
        "-0001-01-01T00:00:00.000Z",
        "+2012-03-01T12:30:45.678Z",
        "9999-12-31T23:59:59.999Z",
        "-9999-01-01T00:00:00.000Z",
        "10000-01-01T00:00:00.000Z",
        // the Julian-to-Gregorian cutover the calendar still carries
        "1582-10-04T00:00:00.000Z",
        "1582-10-05T00:00:00.000Z",
        "1582-10-14T00:00:00.000Z",
        "1582-10-15T00:00:00.000Z",
        "1000-01-01T00:00:00.000Z",
        "1500-06-15T12:00:00.000Z",
        "1500-02-29T00:00:00.000Z",
        "1900-02-29T00:00:00.000Z",
        "2000-02-29T00:00:00.000Z",
        // not dates at all
        "",
        "2012-03-01",
        "not a date at all",
        "2012-3-01T12:30:45.678Z",
        "2012-03-01t12:30:45.678Z",
        // `Integer.parseInt` takes every BMP decimal digit, not only ASCII
        "\u0662\u0660\u0661\u0662-03-01T12:30:45.678Z",
        "2012-\u0660\u0663-01T12:30:45.678Z",
        // ... but the custom time zone's own parser takes ASCII alone
        "2012-03-01T12:30:45.678+\u0660\u0661:00",
        // a signed year field, which only a doubled leading sign reaches
        "+-123-01-01T00:00:00.000Z",
        "--0123-01-01T00:00:00.000Z",
    };

    public static void main(String[] arguments) throws Exception {
        if (arguments.length != 2 || !arguments[0].equals("numeric-vectors")) {
            StoreSupport.refuse("usage: NumericVectors numeric-vectors <out>");
            return;
        }
        String source = ISO8601.class.getProtectionDomain().getCodeSource().getLocation().toString();
        if (!source.contains(EXPECTED_SOURCE)) {
            StoreSupport.refuse("ISO8601 came from " + source + ", not from " + EXPECTED_SOURCE);
            return;
        }
        PrintWriter out = new PrintWriter(
                Files.newBufferedWriter(Paths.get(arguments[1]), StandardCharsets.UTF_8));
        try {
            out.println("# source\t" + source);
            for (long value : LONGS) {
                emit(out, "long\t" + value, new LongField("f", value, Field.Store.NO));
            }
            for (int value : INTEGERS) {
                emit(out, "integer\t" + value, new IntField("f", value, Field.Store.NO));
            }
            for (double value : DOUBLES) {
                emit(out, "double\t" + Double.toHexString(value)
                        + "\t" + Long.toHexString(Double.doubleToLongBits(value)),
                        new DoubleField("f", value, Field.Store.NO));
            }
            for (String date : DATES) {
                out.println("date\t" + escape(date) + "\t" + dateToLong(date));
            }
        } finally {
            out.flush();
            out.close();
        }
    }

    /** What `FieldFactory.dateToLong` makes of a date string. */
    private static String dateToLong(String date) {
        java.util.Calendar parsed = ISO8601.parse(date);
        return parsed == null ? "refused" : Long.toString(parsed.getTimeInMillis());
    }

    /** Every term of one numeric field's own token stream. */
    private static void emit(PrintWriter out, String heading, Field field) throws Exception {
        out.println(heading);
        TokenStream stream = field.tokenStream(null);
        TermToBytesRefAttribute term = stream.addAttribute(TermToBytesRefAttribute.class);
        PositionIncrementAttribute increment =
                stream.addAttribute(PositionIncrementAttribute.class);
        BytesRef bytes = term.getBytesRef();
        stream.reset();
        while (stream.incrementToken()) {
            term.fillBytesRef();
            out.println("term\t" + hexadecimal(bytes) + "\t" + increment.getPositionIncrement());
        }
        stream.end();
        stream.close();
    }

    private static String hexadecimal(BytesRef bytes) {
        StringBuilder rendered = new StringBuilder(bytes.length * 2);
        for (int at = 0; at < bytes.length; at++) {
            rendered.append(String.format("%02x", bytes.bytes[bytes.offset + at] & 0xff));
        }
        return rendered.toString();
    }

    /** A date string with nothing in it that a tab-separated file minds. */
    private static String escape(String text) {
        StringBuilder built = new StringBuilder();
        for (int at = 0; at < text.length(); at++) {
            char character = text.charAt(at);
            if (character == ' ') {
                built.append("\\s");
            } else if (character == '\\') {
                built.append("\\\\");
            } else {
                built.append(character);
            }
        }
        return built.length() == 0 ? "\\e" : built.toString();
    }
}
