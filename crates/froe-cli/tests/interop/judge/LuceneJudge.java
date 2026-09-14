/*
 * What Lucene and Oak say about a Lucene index directory.
 *
 * `checkindex` is Lucene 4.7.2's own `CheckIndex` — the level-2 verdict froe
 * cannot produce until it reads the file format, and the only thing that can
 * say a directory froe wrote is a real Lucene index. `numdocs` is the
 * document count over the same directory. `dump` is Oak's own dumper reading
 * `:data` out of a store, which gives a directory froe has *not* produced to
 * check against. `sample-index` writes a tiny `oakCodec` index so a later
 * task can commit a real one without committing the fixture's megabyte-sized
 * compound file.
 *
 * A verdict class commits nothing and prints nothing on standard output: it
 * exits non-zero with the offending item on standard error. A class whose
 * artefact is a file takes the output path explicitly.
 */
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;

import org.apache.jackrabbit.oak.plugins.index.lucene.LuceneIndexDefinition;
import org.apache.jackrabbit.oak.plugins.index.lucene.OakCodec;
import org.apache.jackrabbit.oak.plugins.index.lucene.directory.LuceneIndexDumper;
import org.apache.jackrabbit.oak.plugins.index.lucene.writer.IndexWriterUtils;
import org.apache.jackrabbit.oak.plugins.memory.EmptyNodeState;
import org.apache.jackrabbit.oak.spi.state.NodeBuilder;
import org.apache.jackrabbit.oak.spi.state.NodeState;
import org.apache.lucene.document.Document;
import org.apache.lucene.document.Field;
import org.apache.lucene.document.StringField;
import org.apache.lucene.index.CheckIndex;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.index.IndexWriter;
import org.apache.lucene.index.IndexWriterConfig;
import org.apache.lucene.store.Directory;
import org.apache.lucene.store.FSDirectory;

public final class LuceneJudge {

    /** How many documents `sample-index` writes. Small on purpose. */
    private static final int SAMPLE_DOCUMENT_COUNT = 5;

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 2) {
            StoreSupport.refuse(
                    "usage: LuceneJudge <checkindex|numdocs|sample-index> <directory>"
                            + " | LuceneJudge dump <store> <indexPath> <outputDirectory>");
            return;
        }
        switch (arguments[0]) {
            case "checkindex" -> checkIndex(new File(arguments[1]));
            case "numdocs" -> numberOfDocuments(new File(arguments[1]));
            case "sample-index" -> sampleIndex(new File(arguments[1]));
            case "dump" -> {
                if (arguments.length < 5) {
                    StoreSupport.refuse(
                            "usage: LuceneJudge dump <store> <indexPath> <outputDir> <pathFile>");
                    return;
                }
                dump(arguments[1], arguments[2], new File(arguments[3]), new File(arguments[4]));
            }
            default -> StoreSupport.refuse("unknown command " + arguments[0]);
        }
    }

    /**
     * Lucene's own checker. Its diagnostics go to standard error whatever the
     * verdict, because a clean run's output is of no interest and a dirty
     * one's is the whole answer — and standard output is reserved for the
     * classes that produce data.
     */
    private static void checkIndex(File directory) throws Exception {
        ByteArrayOutputStream captured = new ByteArrayOutputStream();
        CheckIndex.Status status;
        try (Directory index = FSDirectory.open(directory);
                PrintStream stream = new PrintStream(captured, true, StandardCharsets.UTF_8)) {
            CheckIndex checker = new CheckIndex(index);
            checker.setInfoStream(stream);
            status = checker.checkIndex();
        }
        System.err.print(captured.toString(StandardCharsets.UTF_8));
        if (!status.clean) {
            StoreSupport.refuse("CheckIndex reports " + directory + " is not clean");
        }
    }

    /** The document count, on standard output, for the phase to parse. */
    private static void numberOfDocuments(File directory) throws Exception {
        try (Directory index = FSDirectory.open(directory);
                DirectoryReader reader = DirectoryReader.open(index)) {
            System.out.println(reader.numDocs());
        }
    }

    /**
     * Oak's own dumper, reading `:data` out of the store.
     *
     * The directory it creates is named from the index path by Oak's own
     * rule, so the caller cannot predict it; it is written to `pathFile`
     * rather than to standard output, because opening a segment store starts
     * Oak's logging and that logging goes to standard output.
     */
    private static void dump(String store, String indexPath, File outputDirectory, File pathFile)
            throws Exception {
        try (StoreSupport support = StoreSupport.open(store)) {
            LuceneIndexDumper dumper =
                    new LuceneIndexDumper(support.nodeStore().getRoot(), indexPath,
                            outputDirectory);
            dumper.dump();
            Files.writeString(pathFile.toPath(), dumper.getIndexDir().getAbsolutePath());
        }
    }

    /**
     * A single-segment index of a handful of documents, written with Lucene's
     * own writer under the configuration Oak builds for a definition carrying
     * `codec = oakCodec` — which is the only way to get that codec short of a
     * fulltext-enabled rule.
     *
     * One flush and **no merge**: closing the writer commits the documents as
     * a single compound segment, so the file set is the minimal one —
     * `_0.cfs`, `_0.cfe`, `_0.si`, `segments_1`, `segments.gen`, the same
     * shape a real Oak index's dump has. A `forceMerge` here would rewrite
     * `_0` into `_1` and leave the merged segment uncompounded, which is a
     * different file set and a worse sample.
     */
    private static void sampleIndex(File directory) throws Exception {
        NodeBuilder definitionBuilder = EmptyNodeState.EMPTY_NODE.builder();
        definitionBuilder.setProperty("type", "lucene");
        definitionBuilder.setProperty("codec", new OakCodec().getName());
        NodeState definitionState = definitionBuilder.getNodeState();

        NodeBuilder rootBuilder = EmptyNodeState.EMPTY_NODE.builder();
        rootBuilder.child("oak:index").setChildNode("sample", definitionState);
        NodeState root = rootBuilder.getNodeState();

        LuceneIndexDefinition definition = LuceneIndexDefinition
                .newLuceneBuilder(root, definitionState, "/oak:index/sample")
                .build();
        IndexWriterConfig config = IndexWriterUtils.getIndexWriterConfig(definition, true);
        if (config.getCodec() == null || !"oakCodec".equals(config.getCodec().getName())) {
            StoreSupport.refuse("Oak did not select oakCodec for a definition that names it; "
                    + "the sample would not exercise the codec froe has to read");
            return;
        }
        try (Directory index = FSDirectory.open(directory);
                IndexWriter writer = new IndexWriter(index, config)) {
            for (int number = 0; number < SAMPLE_DOCUMENT_COUNT; number++) {
                Document document = new Document();
                document.add(new StringField(":path", "/content/sample/" + number,
                        Field.Store.YES));
                document.add(new StringField("jcr:primaryType", "nt:unstructured",
                        Field.Store.NO));
                writer.addDocument(document);
            }
        }
        System.out.println(directory.getAbsolutePath());
    }

    private LuceneJudge() {
    }
}
