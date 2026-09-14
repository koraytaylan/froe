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
import org.apache.jackrabbit.oak.plugins.index.property.PropertyIndexInfoProvider;
import org.apache.jackrabbit.oak.spi.state.NodeStore;

public final class IndexJudge {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 3) {
            StoreSupport.refuse(
                    "usage: IndexJudge <definitions|info> <store> <outputFile> [workDir]");
            return;
        }
        String command = arguments[0];
        File output = new File(arguments[2]);
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

    private IndexJudge() {
    }
}
