/*
 * The writer conformance oracle: builds the committed corpus with Lucene's
 * own index writer, and enumerates any index into a canonical dump.
 *
 * `build-corpus <corpus> <directory>` reads the JSON-lines corpus froe
 * reads and writes it through Lucene's `IndexWriter` under the `oakCodec`
 * composition, with the writer configuration Oak builds for a Lucene index:
 * that codec, one compound segment, serial merges, and a buffer large
 * enough that the whole corpus is one flush — so the commit's counter is 1
 * and the segment is `_0`, as a fresh single-commit index has them.
 *
 * `enumerate <directory> <out>` prints every field, term, posting, stored
 * value, doc value and norm of the **live** documents, ordered so the dump
 * depends on the index's content and not on its layout. Term statistics are
 * recomputed from live postings rather than read from the dictionary,
 * because Lucene's stored counts include deleted documents and a fresh
 * segment cannot reproduce those.
 *
 * The JSON reader here is the same restricted grammar froe's own corpus
 * loader reads, written out rather than pulled from a library, so the two
 * sides cannot drift through a dependency neither of them chose.
 */
import java.io.File;
import java.io.IOException;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

import org.apache.lucene.analysis.Analyzer;
import org.apache.lucene.analysis.TokenStream;
import org.apache.lucene.analysis.core.KeywordAnalyzer;
import org.apache.lucene.analysis.tokenattributes.CharTermAttribute;
import org.apache.lucene.analysis.tokenattributes.OffsetAttribute;
import org.apache.lucene.analysis.tokenattributes.PositionIncrementAttribute;
import org.apache.lucene.codecs.Codec;
import org.apache.lucene.document.Document;
import org.apache.lucene.document.Field;
import org.apache.lucene.document.FieldType;
import org.apache.lucene.document.NumericDocValuesField;
import org.apache.lucene.document.SortedDocValuesField;
import org.apache.lucene.document.SortedSetDocValuesField;
import org.apache.lucene.document.StoredField;
import org.apache.lucene.index.AtomicReader;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.index.DocsAndPositionsEnum;
import org.apache.lucene.index.DocsEnum;
import org.apache.lucene.index.FieldInfo;
import org.apache.lucene.index.FieldInfos;
import org.apache.lucene.index.Fields;
import org.apache.lucene.index.IndexWriter;
import org.apache.lucene.index.IndexWriterConfig;
import org.apache.lucene.index.IndexableField;
import org.apache.lucene.index.MultiDocValues;
import org.apache.lucene.index.MultiFields;
import org.apache.lucene.index.NumericDocValues;
import org.apache.lucene.index.SegmentInfos;
import org.apache.lucene.index.SerialMergeScheduler;
import org.apache.lucene.index.SortedDocValues;
import org.apache.lucene.index.SortedSetDocValues;
import org.apache.lucene.index.Terms;
import org.apache.lucene.index.TermsEnum;
import org.apache.lucene.search.DocIdSetIterator;
import org.apache.lucene.store.Directory;
import org.apache.lucene.store.FSDirectory;
import org.apache.lucene.util.Bits;
import org.apache.lucene.util.BytesRef;
import org.apache.lucene.util.Version;

public final class Corpus {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length == 3 && arguments[0].equals("build-corpus")) {
            build(arguments[1], arguments[2]);
            return;
        }
        if (arguments.length == 3 && arguments[0].equals("enumerate")) {
            enumerate(arguments[1], arguments[2]);
            return;
        }
        StoreSupport.refuse("usage: Corpus build-corpus <corpus> <directory>"
                + " | Corpus enumerate <directory> <out>");
    }

    // ---------------------------------------------------------------- build

    private static void build(String corpusPath, String directoryPath) throws Exception {
        List<Json> lines = readCorpus(corpusPath);
        Directory directory = FSDirectory.open(new File(directoryPath));
        Analyzer analyzer = new KeywordAnalyzer();
        IndexWriterConfig config = new IndexWriterConfig(Version.LUCENE_47, analyzer);
        Codec codec = Codec.forName("oakCodec");
        config.setCodec(codec);
        config.setUseCompoundFile(true);
        config.setMergeScheduler(new SerialMergeScheduler());
        // One flush for the whole corpus, so the segment is `_0` and the
        // commit's counter is 1.
        config.setMaxBufferedDocs(IndexWriterConfig.DISABLE_AUTO_FLUSH);
        config.setRAMBufferSizeMB(1024.0);
        IndexWriter writer = new IndexWriter(directory, config);

        int number = 0;
        for (Json line : lines) {
            int count = line.get("count") == null ? 1 : Integer.parseInt(line.get("count").text());
            for (int repeat = 0; repeat < count; repeat++) {
                writer.addDocument(document(line, number));
                number++;
            }
        }
        writer.close();
        directory.close();
        System.out.println("documents\t" + number);
    }

    private static Document document(Json line, int number) {
        Document document = new Document();
        Json fields = line.get("fields");
        if (fields == null) {
            return document;
        }
        for (Json field : fields.array()) {
            addField(document, field, number);
        }
        return document;
    }

    private static void addField(Document document, Json field, int number) {
        String name = field.get("name").text();
        String options = field.get("options") == null ? "positions" : field.get("options").text();
        boolean indexed = field.get("indexed") == null || field.get("indexed").bool();
        boolean omitNorms = field.get("omit_norms") != null && field.get("omit_norms").bool();
        float boost = field.get("boost") == null ? 1.0f : Float.parseFloat(field.get("boost").text());

        List<CorpusToken> tokens = tokens(field, number);
        int finalIncrement = field.get("final_increment") == null
                ? 0 : Integer.parseInt(field.get("final_increment").text());
        int finalOffset;
        if (field.get("final_offset") != null) {
            finalOffset = Integer.parseInt(field.get("final_offset").text());
        } else {
            finalOffset = tokens.isEmpty() ? 0 : tokens.get(tokens.size() - 1).end;
        }

        if (indexed) {
            FieldType type = new FieldType();
            type.setIndexed(true);
            type.setTokenized(true);
            type.setStored(false);
            type.setOmitNorms(omitNorms);
            type.setIndexOptions(indexOptions(options));
            type.freeze();
            Field built = new Field(name, new CannedTokenStream(tokens, finalIncrement, finalOffset), type);
            built.setBoost(boost);
            document.add(built);
        }

        Json stored = field.get("stored");
        if (stored != null) {
            document.add(storedField(name, stored, number));
        }
        Json value = field.get("doc_value");
        if (value != null) {
            addDocValue(document, name, value, number);
        }
    }

    private static FieldInfo.IndexOptions indexOptions(String name) {
        if (name.equals("docs")) {
            return FieldInfo.IndexOptions.DOCS_ONLY;
        }
        if (name.equals("freqs")) {
            return FieldInfo.IndexOptions.DOCS_AND_FREQS;
        }
        if (name.equals("positions")) {
            return FieldInfo.IndexOptions.DOCS_AND_FREQS_AND_POSITIONS;
        }
        if (name.equals("offsets")) {
            return FieldInfo.IndexOptions.DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS;
        }
        throw new IllegalArgumentException("unknown index options " + name);
    }

    private static IndexableField storedField(String name, Json stored, int number) {
        if (stored.get("text") != null) {
            return new StoredField(name, substitute(stored.get("text").text(), number));
        }
        if (stored.get("binary") != null) {
            return new StoredField(name, hexadecimal(stored.get("binary").text()));
        }
        if (stored.get("integer") != null) {
            return new StoredField(name,
                    Integer.parseInt(substitute(stored.get("integer").text(), number)));
        }
        if (stored.get("long") != null) {
            return new StoredField(name,
                    Long.parseLong(substitute(stored.get("long").text(), number)));
        }
        throw new IllegalArgumentException("a stored value names no type");
    }

    private static void addDocValue(Document document, String name, Json value, int number) {
        if (value.get("numeric") != null) {
            document.add(new NumericDocValuesField(name,
                    Long.parseLong(substitute(value.get("numeric").text(), number))));
            return;
        }
        if (value.get("sorted_text") != null) {
            document.add(new SortedDocValuesField(name,
                    new BytesRef(substitute(value.get("sorted_text").text(), number))));
            return;
        }
        if (value.get("sorted") != null) {
            document.add(new SortedDocValuesField(name,
                    new BytesRef(hexadecimal(value.get("sorted").text()))));
            return;
        }
        if (value.get("sorted_set") != null) {
            for (Json entry : value.get("sorted_set").array()) {
                document.add(new SortedSetDocValuesField(name,
                        new BytesRef(substitute(entry.text(), number))));
            }
            return;
        }
        throw new IllegalArgumentException("a doc value names no type");
    }

    private static List<CorpusToken> tokens(Json field, int number) {
        List<CorpusToken> tokens = new ArrayList<CorpusToken>();
        Json list = field.get("tokens");
        if (list == null) {
            return tokens;
        }
        int at = 0;
        for (Json token : list.array()) {
            int repeat = token.get("repeat") == null ? 1 : Integer.parseInt(token.get("repeat").text());
            StringBuilder text = new StringBuilder();
            String unit = substitute(token.get("t").text(), number);
            for (int index = 0; index < repeat; index++) {
                text.append(unit);
            }
            int increment = token.get("i") == null ? 1 : Integer.parseInt(token.get("i").text());
            int start = token.get("s") == null ? at : Integer.parseInt(token.get("s").text());
            int end = token.get("e") == null
                    ? start + text.length() : Integer.parseInt(token.get("e").text());
            at = end + 1;
            tokens.add(new CorpusToken(text.toString(), increment, start, end));
        }
        return tokens;
    }

    private static String substitute(String text, int number) {
        return text.replace("{n}", Integer.toString(number));
    }

    private static byte[] hexadecimal(String text) {
        byte[] bytes = new byte[text.length() / 2];
        for (int index = 0; index < bytes.length; index++) {
            bytes[index] = (byte) Integer.parseInt(text.substring(index * 2, index * 2 + 2), 16);
        }
        return bytes;
    }

    /** One token of the corpus. */
    private static final class CorpusToken {
        final String text;
        final int increment;
        final int start;
        final int end;

        CorpusToken(String text, int increment, int start, int end) {
            this.text = text;
            this.increment = increment;
            this.start = start;
            this.end = end;
        }
    }

    /**
     * The corpus's tokens as a stream, whose `end()` reports the corpus's own
     * final increment and offset — so the composition rules for several
     * fields of one name are proved against Lucene rather than assumed.
     */
    private static final class CannedTokenStream extends TokenStream {
        private final List<CorpusToken> tokens;
        private final int finalIncrement;
        private final int finalOffset;
        private final CharTermAttribute term = addAttribute(CharTermAttribute.class);
        private final PositionIncrementAttribute increment =
                addAttribute(PositionIncrementAttribute.class);
        private final OffsetAttribute offset = addAttribute(OffsetAttribute.class);
        private int at;

        CannedTokenStream(List<CorpusToken> tokens, int finalIncrement, int finalOffset) {
            this.tokens = tokens;
            this.finalIncrement = finalIncrement;
            this.finalOffset = finalOffset;
        }

        @Override
        public void reset() throws IOException {
            super.reset();
            at = 0;
        }

        @Override
        public boolean incrementToken() {
            if (at == tokens.size()) {
                return false;
            }
            clearAttributes();
            CorpusToken token = tokens.get(at++);
            term.setEmpty().append(token.text);
            increment.setPositionIncrement(token.increment);
            offset.setOffset(token.start, token.end);
            return true;
        }

        @Override
        public void end() throws IOException {
            super.end();
            increment.setPositionIncrement(finalIncrement);
            offset.setOffset(finalOffset, finalOffset);
        }
    }

    // ------------------------------------------------------------ enumerate

    private static void enumerate(String directoryPath, String outPath) throws Exception {
        Directory directory = FSDirectory.open(new File(directoryPath));
        SegmentInfos infos = new SegmentInfos();
        infos.read(directory);
        DirectoryReader reader = DirectoryReader.open(directory);
        PrintWriter out = new PrintWriter(
                Files.newBufferedWriter(Paths.get(outPath), StandardCharsets.UTF_8));
        try {
            out.println("counter\t" + infos.counter);
            out.println("numdocs\t" + reader.numDocs());

            Bits live = MultiFields.getLiveDocs(reader);
            TreeMap<String, FieldInfo> byName = new TreeMap<String, FieldInfo>();
            for (FieldInfo info : MultiFields.getMergedFieldInfos(reader)) {
                byName.put(info.name, info);
            }
            for (Map.Entry<String, FieldInfo> entry : byName.entrySet()) {
                FieldInfo info = entry.getValue();
                out.println("field\t" + info.name
                        + "\tindexed=" + info.isIndexed()
                        + "\toptions=" + (info.getIndexOptions() == null
                                ? "NONE" : info.getIndexOptions().name())
                        + "\tnorms=" + (info.hasNorms() ? "yes" : "no")
                        + "\tdocvalues=" + (info.getDocValuesType() == null
                                ? "NONE" : info.getDocValuesType().name()));
            }

            writeTerms(out, reader, live, byName.keySet().toArray(new String[0]));
            writeStored(out, reader, live);
            writeDocValues(out, reader, live, byName);
            writeNorms(out, reader, live, byName);
        } finally {
            out.close();
            reader.close();
            directory.close();
        }
    }

    /**
     * Every term with statistics recomputed from **live** postings: the
     * dictionary's own counts include deleted documents, which an
     * incrementally maintained index carries and a fresh segment cannot.
     */
    private static void writeTerms(PrintWriter out, DirectoryReader reader, Bits live,
            String[] names) throws IOException {
        Fields fields = MultiFields.getFields(reader);
        if (fields == null) {
            return;
        }
        for (String name : names) {
            Terms terms = fields.terms(name);
            if (terms == null) {
                continue;
            }
            TermsEnum iterator = terms.iterator(null);
            BytesRef term;
            while ((term = iterator.next()) != null) {
                DocsAndPositionsEnum positions = iterator.docsAndPositions(live, null);
                StringBuilder postings = new StringBuilder();
                long documentFrequency = 0;
                long totalTermFrequency = 0;
                if (positions != null) {
                    while (positions.nextDoc() != DocIdSetIterator.NO_MORE_DOCS) {
                        documentFrequency++;
                        int frequency = positions.freq();
                        totalTermFrequency += frequency;
                        postings.append("\nposting\t").append(name).append('\t')
                                .append(hexadecimal(term)).append('\t')
                                .append(positions.docID()).append('\t').append(frequency);
                        for (int index = 0; index < frequency; index++) {
                            int position = positions.nextPosition();
                            postings.append('\t').append(position)
                                    .append(':').append(positions.startOffset())
                                    .append(':').append(positions.endOffset());
                        }
                    }
                } else {
                    DocsEnum documents = iterator.docs(live, null, DocsEnum.FLAG_FREQS);
                    while (documents.nextDoc() != DocIdSetIterator.NO_MORE_DOCS) {
                        documentFrequency++;
                        int frequency = documents.freq();
                        totalTermFrequency += frequency;
                        postings.append("\nposting\t").append(name).append('\t')
                                .append(hexadecimal(term)).append('\t')
                                .append(documents.docID()).append('\t').append(frequency);
                    }
                }
                if (documentFrequency == 0) {
                    continue;
                }
                out.println("term\t" + name + "\t" + hexadecimal(term)
                        + "\t" + documentFrequency + "\t" + totalTermFrequency
                        + postings);
            }
        }
    }

    private static void writeStored(PrintWriter out, DirectoryReader reader, Bits live)
            throws IOException {
        for (int document = 0; document < reader.maxDoc(); document++) {
            if (live != null && !live.get(document)) {
                continue;
            }
            for (IndexableField field : reader.document(document).getFields()) {
                String rendered;
                if (field.binaryValue() != null) {
                    rendered = "binary\t" + hexadecimal(field.binaryValue());
                } else if (field.numericValue() != null) {
                    rendered = field.numericValue().getClass().getSimpleName()
                            + "\t" + field.numericValue();
                } else {
                    rendered = "text\t" + escape(field.stringValue());
                }
                out.println("stored\t" + document + "\t" + field.name() + "\t" + rendered);
            }
        }
    }

    private static void writeDocValues(PrintWriter out, DirectoryReader reader, Bits live,
            TreeMap<String, FieldInfo> byName) throws IOException {
        for (Map.Entry<String, FieldInfo> entry : byName.entrySet()) {
            FieldInfo info = entry.getValue();
            if (info.getDocValuesType() == null) {
                continue;
            }
            String name = info.name;
            if (info.getDocValuesType() == FieldInfo.DocValuesType.NUMERIC) {
                NumericDocValues values = MultiDocValues.getNumericValues(reader, name);
                Bits present = MultiDocValues.getDocsWithField(reader, name);
                for (int document = 0; document < reader.maxDoc(); document++) {
                    if (live != null && !live.get(document)) {
                        continue;
                    }
                    out.println("docvalue\t" + name + "\t" + document
                            + "\t" + (present != null && present.get(document) ? 1 : 0)
                            + "\t" + (values == null ? 0 : values.get(document)));
                }
            } else if (info.getDocValuesType() == FieldInfo.DocValuesType.SORTED) {
                SortedDocValues values = MultiDocValues.getSortedValues(reader, name);
                for (int document = 0; document < reader.maxDoc(); document++) {
                    if (live != null && !live.get(document)) {
                        continue;
                    }
                    int ordinal = values == null ? -1 : values.getOrd(document);
                    String rendered = "";
                    if (ordinal >= 0) {
                        BytesRef bytes = new BytesRef();
                        values.lookupOrd(ordinal, bytes);
                        rendered = hexadecimal(bytes);
                    }
                    out.println("docvalue\t" + name + "\t" + document
                            + "\t" + ordinal + "\t" + rendered);
                }
            } else if (info.getDocValuesType() == FieldInfo.DocValuesType.SORTED_SET) {
                SortedSetDocValues values = MultiDocValues.getSortedSetValues(reader, name);
                for (int document = 0; document < reader.maxDoc(); document++) {
                    if (live != null && !live.get(document)) {
                        continue;
                    }
                    StringBuilder rendered = new StringBuilder();
                    if (values != null) {
                        values.setDocument(document);
                        long ordinal;
                        while ((ordinal = values.nextOrd())
                                != SortedSetDocValues.NO_MORE_ORDS) {
                            BytesRef bytes = new BytesRef();
                            values.lookupOrd(ordinal, bytes);
                            rendered.append('\t').append(ordinal)
                                    .append('=').append(hexadecimal(bytes));
                        }
                    }
                    out.println("docvalue\t" + name + "\t" + document + rendered);
                }
            } else {
                // A type this enumerator cannot render would otherwise be
                // announced on the field line and then contribute no value
                // line at all — on both sides, so the comparison would pass
                // over it in silence. A dump that cannot render a field
                // refuses instead.
                StoreSupport.refuse("this enumerator renders no "
                        + info.getDocValuesType().name() + " doc values, and "
                        + name + " carries them");
                return;
            }
        }
    }

    private static void writeNorms(PrintWriter out, DirectoryReader reader, Bits live,
            TreeMap<String, FieldInfo> byName) throws IOException {
        for (Map.Entry<String, FieldInfo> entry : byName.entrySet()) {
            FieldInfo info = entry.getValue();
            if (!info.hasNorms()) {
                continue;
            }
            NumericDocValues norms = MultiDocValues.getNormValues(reader, info.name);
            for (int document = 0; document < reader.maxDoc(); document++) {
                if (live != null && !live.get(document)) {
                    continue;
                }
                out.println("norm\t" + info.name + "\t" + document
                        + "\t" + (norms == null ? 0 : (norms.get(document) & 0xff)));
            }
        }
    }

    /**
     * A stored string as one line: the dump is line-oriented, and a stored
     * value is the only field in it that carries text a document supplied.
     * Oak stores a binary's extracted text as a `:fulltext` value, and an
     * SVG or an HTML page's text carries newlines and tabs — which would
     * otherwise turn one stored value into several lines of a kind no
     * reader knows.
     */
    private static String escape(String value) {
        StringBuilder rendered = new StringBuilder(value.length());
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            if (character == '\\') {
                rendered.append("\\\\");
            } else if (character == '\n') {
                rendered.append("\\n");
            } else if (character == '\r') {
                rendered.append("\\r");
            } else if (character == '\t') {
                rendered.append("\\t");
            } else {
                rendered.append(character);
            }
        }
        return rendered.toString();
    }

    private static String hexadecimal(BytesRef bytes) {
        StringBuilder rendered = new StringBuilder(bytes.length * 2);
        for (int index = 0; index < bytes.length; index++) {
            rendered.append(String.format("%02x", bytes.bytes[bytes.offset + index] & 0xff));
        }
        return rendered.toString();
    }

    // ----------------------------------------------------------------- json

    private static List<Json> readCorpus(String path) throws IOException {
        List<Json> lines = new ArrayList<Json>();
        for (String line : Files.readAllLines(Paths.get(path), StandardCharsets.UTF_8)) {
            if (line.startsWith("#") || line.trim().isEmpty()) {
                continue;
            }
            lines.add(Json.parse(line));
        }
        return lines;
    }

    /**
     * The restricted JSON the corpus uses, read the same way froe's own
     * loader reads it — written out rather than pulled from a library, so
     * the two sides cannot drift through a dependency neither chose.
     */
    static final class Json {
        private final Object value;

        private Json(Object value) {
            this.value = value;
        }

        static Json parse(String text) {
            int[] at = new int[] { 0 };
            Json parsed = parseValue(text, at);
            return parsed;
        }

        @SuppressWarnings("unchecked")
        Json get(String key) {
            return ((Map<String, Json>) value).get(key);
        }

        @SuppressWarnings("unchecked")
        List<Json> array() {
            return (List<Json>) value;
        }

        String text() {
            return (String) value;
        }

        boolean bool() {
            return ((Boolean) value).booleanValue();
        }

        private static void skip(String text, int[] at) {
            while (at[0] < text.length() && Character.isWhitespace(text.charAt(at[0]))) {
                at[0]++;
            }
        }

        private static Json parseValue(String text, int[] at) {
            skip(text, at);
            char first = text.charAt(at[0]);
            if (first == '{') {
                at[0]++;
                Map<String, Json> entries = new TreeMap<String, Json>();
                while (true) {
                    skip(text, at);
                    if (text.charAt(at[0]) == '}') {
                        at[0]++;
                        return new Json(entries);
                    }
                    String key = parseString(text, at);
                    skip(text, at);
                    at[0]++; // the colon
                    entries.put(key, parseValue(text, at));
                    skip(text, at);
                    if (text.charAt(at[0]) == ',') {
                        at[0]++;
                    }
                }
            }
            if (first == '[') {
                at[0]++;
                List<Json> values = new ArrayList<Json>();
                while (true) {
                    skip(text, at);
                    if (text.charAt(at[0]) == ']') {
                        at[0]++;
                        return new Json(values);
                    }
                    values.add(parseValue(text, at));
                    skip(text, at);
                    if (text.charAt(at[0]) == ',') {
                        at[0]++;
                    }
                }
            }
            if (first == '"') {
                return new Json(parseString(text, at));
            }
            if (first == 't') {
                at[0] += 4;
                return new Json(Boolean.TRUE);
            }
            if (first == 'f') {
                at[0] += 5;
                return new Json(Boolean.FALSE);
            }
            int start = at[0];
            while (at[0] < text.length()
                    && "0123456789-+.eE".indexOf(text.charAt(at[0])) >= 0) {
                at[0]++;
            }
            return new Json(text.substring(start, at[0]));
        }

        private static String parseString(String text, int[] at) {
            skip(text, at);
            at[0]++; // the opening quote
            StringBuilder built = new StringBuilder();
            while (text.charAt(at[0]) != '"') {
                if (text.charAt(at[0]) == '\\') {
                    at[0]++;
                    char escaped = text.charAt(at[0]);
                    if (escaped == 'n') {
                        built.append('\n');
                    } else if (escaped == 'r') {
                        built.append('\r');
                    } else if (escaped == 't') {
                        built.append('\t');
                    } else {
                        built.append(escaped);
                    }
                } else {
                    built.append(text.charAt(at[0]));
                }
                at[0]++;
            }
            at[0]++;
            return built.toString();
        }
    }
}
