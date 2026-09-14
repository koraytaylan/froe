/*
 * Oak's own analyzers, over a committed corpus, for froe to replay.
 *
 * `analyze <mode> <corpus> <out>` prints one block per corpus line: the
 * token tuples the named chain produces and the end state Lucene reads
 * from the stream once its tokens are consumed — the position increment
 * and the offset — which is what plan 0009's writer needs to compose
 * several values of one field.
 *
 * The five modes are the five chains a Lucene index actually uses:
 *
 *   default        the analyzer `LuceneIndexDefinition` builds for a
 *                  definition with no `analyzers` node, capped at
 *                  `maxFieldLength`
 *   original-term  the same with `analyzers/@indexOriginalTerm`
 *   ancestors      the same definition with `evaluatePathRestrictions`,
 *                  asked for the `:ancestors` field, which routes through
 *                  the path-hierarchy chain
 *   spellcheck     what the writer configuration installs for
 *                  `:spellcheck`: Oak's shared analyzer under a shingle
 *                  wrapper at a maximum size of 3, uncapped
 *   suggest        what it installs for `:suggest`: the suggest helper's
 *                  newline-only tokenizer, uncapped
 *
 * `lower-case-table <out>` prints every code point the image's JVM
 * lower-cases to something other than itself. That table is the
 * consumer's own truth: Lucene's lower-case filter calls
 * `Character.toLowerCase(int)`, so the mapping is the running JVM's and
 * not any Unicode file's.
 */
import java.io.IOException;
import java.io.PrintWriter;
import java.io.StringReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.List;

import org.apache.lucene.analysis.Analyzer;
import org.apache.lucene.analysis.TokenStream;
import org.apache.lucene.analysis.miscellaneous.LimitTokenCountAnalyzer;
import org.apache.lucene.analysis.shingle.ShingleAnalyzerWrapper;
import org.apache.lucene.analysis.tokenattributes.CharTermAttribute;
import org.apache.lucene.analysis.tokenattributes.OffsetAttribute;
import org.apache.lucene.analysis.tokenattributes.PositionIncrementAttribute;
import org.apache.lucene.analysis.tokenattributes.TypeAttribute;
import org.apache.lucene.util.Version;

import org.apache.jackrabbit.oak.plugins.index.lucene.OakAnalyzer;

public final class Analyze {

    /** `LuceneIndexConstants.VERSION`. */
    private static final Version VERSION = Version.LUCENE_47;

    /** `IndexDefinition.DEFAULT_MAX_FIELD_LENGTH`. */
    private static final int MAX_FIELD_LENGTH = 10000;

    /** How many tuples are kept at each end of a long stream. */
    private static final int ELISION_EDGE = 150;

    public static void main(String[] arguments) throws Exception {
        if (arguments.length == 2 && arguments[0].equals("lower-case-table")) {
            lowerCaseTable(arguments[1]);
            return;
        }
        if (arguments.length == 2 && arguments[0].equals("character-class-table")) {
            characterClassTable(arguments[1]);
            return;
        }
        if (arguments.length == 3 && arguments[0].equals("analyze")) {
            analyze(arguments[1], arguments[2], null);
            return;
        }
        if (arguments.length == 4 && arguments[0].equals("analyze")) {
            analyze(arguments[1], arguments[2], arguments[3]);
            return;
        }
        StoreSupport.refuse("usage: Analyze analyze <mode> <corpus> <out>"
                + " | Analyze lower-case-table <out>");
    }

    /**
     * The chain a mode names, built the way Oak builds it.
     *
     * The capped modes are the definition's own analyzer, which
     * `createAnalyzer` wraps in `LimitTokenCountAnalyzer`; the per-field
     * ones the writer configuration installs are not wrapped, so they are
     * returned bare.
     */
    private static Analyzer analyzerFor(String mode) {
        if (mode.equals("default")) {
            return new LimitTokenCountAnalyzer(new OakAnalyzer(VERSION), MAX_FIELD_LENGTH);
        }
        if (mode.equals("original-term")) {
            return new LimitTokenCountAnalyzer(new OakAnalyzer(VERSION, true), MAX_FIELD_LENGTH);
        }
        if (mode.equals("ancestors")) {
            // `createAnalyzer` routes `:ancestors` through a path-hierarchy
            // chain and wraps the whole per-field wrapper in the cap.
            return new LimitTokenCountAnalyzer(new PathHierarchyAnalyzer(), MAX_FIELD_LENGTH);
        }
        if (mode.equals("spellcheck")) {
            return new ShingleAnalyzerWrapper(new OakAnalyzer(VERSION), 3);
        }
        if (mode.equals("suggest")) {
            return new SuggestAnalyzer();
        }
        throw new IllegalArgumentException("unknown mode " + mode);
    }

    private static void analyze(String mode, String corpusPath, String outPath) throws IOException {
        Analyzer analyzer = analyzerFor(mode);
        List<String> lines = Files.readAllLines(Paths.get(corpusPath), StandardCharsets.UTF_8);
        PrintWriter out = outPath == null
                ? new PrintWriter(System.out)
                : new PrintWriter(Files.newBufferedWriter(Paths.get(outPath), StandardCharsets.UTF_8));
        try {
            out.println("# " + mode);
            int number = 0;
            for (String line : lines) {
                if (line.startsWith("#") || line.isEmpty()) {
                    continue;
                }
                String text = unescape(line);
                out.println("line\t" + number);
                emit(analyzer, mode, text, out);
                number++;
            }
        } finally {
            out.flush();
            if (outPath != null) {
                out.close();
            }
        }
    }

    private static void emit(Analyzer analyzer, String mode, String text, PrintWriter out)
            throws IOException {
        String field = mode.equals("ancestors") ? ":ancestors"
                : mode.equals("spellcheck") ? ":spellcheck"
                : mode.equals("suggest") ? ":suggest" : "full:body";
        TokenStream stream = analyzer.tokenStream(field, new StringReader(text));
        CharTermAttribute term = stream.addAttribute(CharTermAttribute.class);
        PositionIncrementAttribute increment = stream.addAttribute(PositionIncrementAttribute.class);
        OffsetAttribute offset = stream.addAttribute(OffsetAttribute.class);
        TypeAttribute type = stream.addAttribute(TypeAttribute.class);
        stream.reset();
        // A line above the token cap produces ten thousand tuples, and the
        // uncapped shingle chain three times that. The ends are what the
        // cap and the boundaries are proved by, so a long stream is
        // recorded as its head, its count and its tail — and froe's replay
        // elides by the same rule.
        List<String> tuples = new java.util.ArrayList<String>();
        while (stream.incrementToken()) {
            tuples.add("token\t" + hexadecimal(term.toString())
                    + "\t" + increment.getPositionIncrement()
                    + "\t" + offset.startOffset()
                    + "\t" + offset.endOffset()
                    + "\t" + type.type());
        }
        out.println("count\t" + tuples.size());
        if (tuples.size() <= 2 * ELISION_EDGE) {
            for (String tuple : tuples) {
                out.println(tuple);
            }
        } else {
            for (int at = 0; at < ELISION_EDGE; at++) {
                out.println(tuples.get(at));
            }
            out.println("elided\t" + (tuples.size() - 2 * ELISION_EDGE));
            for (int at = tuples.size() - ELISION_EDGE; at < tuples.size(); at++) {
                out.println(tuples.get(at));
            }
        }
        stream.end();
        out.println("end\t" + increment.getPositionIncrement() + "\t" + offset.endOffset());
        stream.close();
    }

    private static void lowerCaseTable(String outPath) throws IOException {
        PrintWriter out = new PrintWriter(
                Files.newBufferedWriter(Paths.get(outPath), StandardCharsets.UTF_8));
        try {
            out.println("# Character.toLowerCase(int) on " + System.getProperty("java.vendor")
                    + " " + System.getProperty("java.version"));
            out.println("# Columns: <code point in hex>\\t<lower case in hex>");
            for (int point = 0; point <= Character.MAX_CODE_POINT; point++) {
                int lower = Character.toLowerCase(point);
                if (lower != point) {
                    out.println(Integer.toHexString(point) + "\t" + Integer.toHexString(lower));
                }
            }
        } finally {
            out.close();
        }
    }

    /**
     * The character classes `WordDelimiterIterator` gives code points, as
     * the image's own JVM computes them.
     *
     * Below 256 the iterator uses a table built from `Character.isLowerCase`,
     * `isUpperCase` and `isDigit`; at 256 and above it switches on
     * `Character.getType`. Both are the running JVM's, so this table is the
     * consumer's own truth in the same way the lower-case table is — and for
     * the same reason: `lucene-oak-analysis.md` §4.1.
     *
     * Emitted as ranges of equal class, and only for classes other than
     * `SUBWORD_DELIM`, which is the default.
     */
    private static void characterClassTable(String outPath) throws IOException {
        PrintWriter out = new PrintWriter(
                Files.newBufferedWriter(Paths.get(outPath), StandardCharsets.UTF_8));
        try {
            out.println("# WordDelimiterIterator character classes on "
                    + System.getProperty("java.vendor") + " " + System.getProperty("java.version"));
            out.println("# Columns: <first code point in hex>\t<last>\t<class>");
            out.println("# Classes: LOWER=1 UPPER=2 DIGIT=4 ALPHA=3 ALPHANUM=7;"
                    + " a code point not listed is SUBWORD_DELIM=8.");
            int runStart = 0;
            int runClass = characterClass(0);
            for (int point = 1; point <= Character.MAX_CODE_POINT; point++) {
                int code = characterClass(point);
                if (code != runClass) {
                    if (runClass != 8) {
                        out.println(Integer.toHexString(runStart) + "\t"
                                + Integer.toHexString(point - 1) + "\t" + runClass);
                    }
                    runStart = point;
                    runClass = code;
                }
            }
            if (runClass != 8) {
                out.println(Integer.toHexString(runStart) + "\t"
                        + Integer.toHexString(Character.MAX_CODE_POINT) + "\t" + runClass);
            }
        } finally {
            out.close();
        }
    }

    /** `WordDelimiterIterator.charType`, transcribed. */
    private static int characterClass(int point) {
        if (point < 256) {
            int code = 0;
            if (Character.isLowerCase(point)) {
                code |= 1;
            } else if (Character.isUpperCase(point)) {
                code |= 2;
            } else if (Character.isDigit(point)) {
                code |= 4;
            }
            return code == 0 ? 8 : code;
        }
        switch (Character.getType(point)) {
            case Character.UPPERCASE_LETTER: return 2;
            case Character.LOWERCASE_LETTER: return 1;
            case Character.TITLECASE_LETTER:
            case Character.MODIFIER_LETTER:
            case Character.OTHER_LETTER:
            case Character.NON_SPACING_MARK:
            case Character.ENCLOSING_MARK:
            case Character.COMBINING_SPACING_MARK:
                return 3;
            case Character.DECIMAL_DIGIT_NUMBER:
            case Character.LETTER_NUMBER:
            case Character.OTHER_NUMBER:
                return 4;
            case Character.SURROGATE:
                return 7;
            default:
                return 8;
        }
    }

    /**
     * The corpus's own escapes, so a line can carry a newline, a tab or an
     * astral character without the file carrying a control character.
     */
    private static String unescape(String line) {
        StringBuilder built = new StringBuilder();
        for (int at = 0; at < line.length(); at++) {
            char ch = line.charAt(at);
            if (ch != '\\') {
                built.append(ch);
                continue;
            }
            at++;
            char escaped = line.charAt(at);
            if (escaped == 'n') {
                built.append('\n');
            } else if (escaped == 't') {
                built.append('\t');
            } else if (escaped == 'r') {
                built.append('\r');
            } else if (escaped == '\\') {
                built.append('\\');
            } else if (escaped == 'u') {
                built.append((char) Integer.parseInt(line.substring(at + 1, at + 5), 16));
                at += 4;
            } else if (escaped == 'x') {
                // \xNNNNNN: one code point, so an astral character is one escape
                built.appendCodePoint(Integer.parseInt(line.substring(at + 1, at + 7), 16));
                at += 6;
            } else if (escaped == '*') {
                // \*NNNN*TEXT*: TEXT repeated NNNN times
                int firstStar = line.indexOf('*', at + 1);
                int secondStar = line.indexOf('*', firstStar + 1);
                int count = Integer.parseInt(line.substring(at + 1, firstStar));
                String unit = line.substring(firstStar + 1, secondStar);
                for (int index = 0; index < count; index++) {
                    built.append(unit);
                }
                at = secondStar;
            } else {
                built.append(escaped);
            }
        }
        return built.toString();
    }

    private static String hexadecimal(String text) {
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        StringBuilder rendered = new StringBuilder(bytes.length * 2);
        for (byte b : bytes) {
            rendered.append(String.format("%02x", b & 0xff));
        }
        return rendered.toString();
    }

    /** The `:ancestors` chain: a path-hierarchy tokenizer and nothing else. */
    private static final class PathHierarchyAnalyzer extends Analyzer {
        @Override
        protected TokenStreamComponents createComponents(String fieldName, java.io.Reader reader) {
            return new TokenStreamComponents(
                    new org.apache.lucene.analysis.path.PathHierarchyTokenizer(reader));
        }
    }

    /** What `SuggestHelper.getAnalyzer()` builds. */
    private static final class SuggestAnalyzer extends Analyzer {
        @Override
        protected TokenStreamComponents createComponents(String fieldName, java.io.Reader reader) {
            return new TokenStreamComponents(
                    new org.apache.jackrabbit.oak.plugins.index.lucene.util.CRTokenizer(
                            VERSION, reader));
        }
    }
}
