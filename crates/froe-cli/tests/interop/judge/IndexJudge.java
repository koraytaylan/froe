/*
 * What Oak says about a store's indexes.
 *
 * `definitions` is `IndexDefinitionPrinter` over the index paths Oak's own
 * path service yields, which is the byte-for-byte oracle for
 * `froe index definitions`. `info` is `IndexPrinter` over an index-
 * information service bound to the same providers Oak binds at runtime,
 * which is the oracle for `froe index list`.
 *
 * Both write to a file the caller names rather than to standard output, and
 * that is not a style choice: opening a segment store initializes Oak's
 * logging, whose console appender writes to **standard output**, so a
 * printer's bytes on that stream would arrive interleaved with
 * `TarMK ReadOnly opened` and friends. A byte comparison against Oak cannot
 * be made against a stream something else is also writing to. The file
 * carries the printer's bytes and nothing else.
 */
import java.io.File;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;

import org.apache.felix.inventory.Format;
import org.apache.jackrabbit.oak.plugins.index.AsyncIndexInfoService;
import org.apache.jackrabbit.oak.plugins.index.AsyncIndexInfoServiceImpl;
import org.apache.jackrabbit.oak.plugins.index.IndexInfoServiceImpl;
import org.apache.jackrabbit.oak.plugins.index.IndexPathService;
import org.apache.jackrabbit.oak.plugins.index.IndexPathServiceImpl;
import org.apache.jackrabbit.oak.plugins.index.inventory.IndexDefinitionPrinter;
import org.apache.jackrabbit.oak.plugins.index.inventory.IndexPrinter;
import org.apache.jackrabbit.oak.plugins.index.lucene.LuceneIndexInfoProvider;
import org.apache.jackrabbit.oak.plugins.index.IndexUpdateProvider;
import org.apache.jackrabbit.oak.plugins.index.property.PropertyIndexEditorProvider;
import org.apache.jackrabbit.oak.plugins.index.property.PropertyIndexInfoProvider;
import org.apache.jackrabbit.oak.api.Type;
import org.apache.jackrabbit.oak.spi.commit.CommitInfo;
import org.apache.jackrabbit.oak.spi.commit.EditorHook;
import org.apache.jackrabbit.oak.spi.state.NodeBuilder;
import org.apache.jackrabbit.oak.spi.state.NodeState;
import org.apache.jackrabbit.oak.spi.state.NodeStore;
import org.apache.jackrabbit.oak.spi.state.NodeStateUtils;

public final class IndexJudge {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 3) {
            StoreSupport.refuse(
                    "usage: IndexJudge <definitions|info> <store> <outputFile> [workDir]");
            return;
        }
        String command = arguments[0];
        File output = new File(arguments[2]);
        // `probe` writes, so it opens the store differently — and only ever
        // against a copy the caller made.
        if ("probe".equals(command)) {
            if (arguments.length < 4) {
                StoreSupport.refuse("usage: IndexJudge probe <store> <resultFile> <path>");
                return;
            }
            try (StoreSupport support = StoreSupport.openWritable(arguments[1])) {
                write(output, probe(support.nodeStore(), arguments[3]));
            }
            return;
        }
        try (StoreSupport support = StoreSupport.open(arguments[1])) {
            switch (command) {
                case "definitions" -> write(output, definitions(support.nodeStore()));
                case "info" -> write(output, info(support.nodeStore(), new File(
                        arguments.length > 3
                                ? arguments[3]
                                : System.getProperty("java.io.tmpdir"))));
                default -> StoreSupport.refuse("unknown command " + command);
            }
        }
    }

    /** UTF-8, no added newline: the printer's bytes are the artefact. */
    private static void write(File output, String content) throws Exception {
        Files.write(output.toPath(), content.getBytes(StandardCharsets.UTF_8));
    }

    /**
     * Oak's definition printer, with its own default filter — the one that
     * keeps every property but `:childOrder` and drops every hidden child.
     */
    private static String definitions(NodeStore nodeStore) {
        IndexPathService indexPathService = new IndexPathServiceImpl(nodeStore);
        IndexDefinitionPrinter printer = new IndexDefinitionPrinter(nodeStore, indexPathService);
        StringWriter captured = new StringWriter();
        PrintWriter writer = new PrintWriter(captured);
        printer.print(writer, Format.JSON, false);
        writer.flush();
        return captured.toString();
    }

    /**
     * Oak's index printer, over an information service bound to exactly the
     * providers Oak's own runtime binds: the property family's and Lucene's.
     * A type with no provider is reported by type alone, which is what makes
     * the counter and reference comparisons thin rather than absent.
     *
     * `workDir` is where Lucene's provider stages a directory it has to copy
     * out to read; it is a scratch path, never the store.
     */
    private static String info(NodeStore nodeStore, File workDir) throws Exception {
        IndexPathService indexPathService = new IndexPathServiceImpl(nodeStore);
        AsyncIndexInfoService asyncService = new AsyncIndexInfoServiceImpl(nodeStore);
        IndexInfoServiceImpl infoService = new IndexInfoServiceImpl(nodeStore, indexPathService);
        infoService.bindInfoProviders(new PropertyIndexInfoProvider(nodeStore));
        infoService.bindInfoProviders(
                new LuceneIndexInfoProvider(nodeStore, asyncService, workDir));
        IndexPrinter printer = new IndexPrinter(infoService, asyncService);
        StringWriter captured = new StringWriter();
        PrintWriter writer = new PrintWriter(captured);
        printer.print(writer, Format.JSON, false);
        writer.flush();
        return captured.toString();
    }

    /** The child `probe` creates, and the type it carries. */
    private static final String PROBE_NAME = "interopProbe";
    private static final String PROBE_TYPE = "nt:unstructured";

    /**
     * Creates a child node under {@code path} through Oak's own index
     * update, and reports whether the node-type index gained an entry for it.
     *
     * This settles a question source reading could not. A pristine
     * Oak-written store holds nodes the `nodetype` property index does not
     * name — the `lucene` definition's `indexRules` subtree and the
     * `rep:permissionStore` nodes — and nothing in `IndexUpdate`,
     * `VisibleEditor` or `PropertyIndexEditor` read at the pinned commit
     * explains it. Two explanations are possible: Oak's editor does not
     * *cover* those nodes, or it never *saw* them.
     *
     * A **new node** is what tells them apart, and an existing node touched
     * on an unrelated property does not: `PropertyIndexEditor` writes an
     * entry when an *indexed* property is added, changed or removed, so a
     * commit that leaves `jcr:primaryType` alone is correctly a no-op for
     * this index whatever the coverage rule is. Creating a child sets
     * `jcr:primaryType`, which is one of the two properties this definition
     * names.
     *
     * Run against a content path it is the control: the entry must appear
     * there, or the harness rather than the coverage rule is what the
     * verdict is about.
     *
     * The result is `entry=<path>`, `before=<bool>`, `after=<bool>`.
     */
    private static String probe(NodeStore nodeStore, String path) throws Exception {
        String probePath = path + "/" + PROBE_NAME;
        String entryPath = nodeTypeEntryPath(PROBE_TYPE, probePath);
        boolean before = NodeStateUtils.getNode(nodeStore.getRoot(), entryPath).exists();

        NodeBuilder builder = nodeStore.getRoot().builder();
        NodeBuilder node = builder;
        for (String element : path.split("/")) {
            if (!element.isEmpty()) {
                node = node.getChildNode(element);
            }
        }
        if (!node.exists()) {
            StoreSupport.refuse(path + " does not exist in the store");
            return "";
        }
        node.child(PROBE_NAME).setProperty("jcr:primaryType", PROBE_TYPE, Type.NAME);
        // The whole point: the commit runs Oak's index update over the
        // property-index editor, which is the editor that maintains the
        // node-type index.
        nodeStore.merge(builder,
                new EditorHook(new IndexUpdateProvider(new PropertyIndexEditorProvider())),
                CommitInfo.EMPTY);

        boolean after = NodeStateUtils.getNode(nodeStore.getRoot(), entryPath).exists();
        return "entry=" + entryPath + "\nbefore=" + before + "\nafter=" + after + "\n";
    }

    /**
     * Where the node-type index holds an entry for a node of {@code type} at
     * {@code path}: the key is the type URL-encoded, then the path below it.
     */
    private static String nodeTypeEntryPath(String type, String path) {
        String key = java.net.URLEncoder.encode(type, java.nio.charset.StandardCharsets.UTF_8);
        return "/oak:index/nodetype/:index/" + key + path;
    }

    private IndexJudge() {
    }
}
