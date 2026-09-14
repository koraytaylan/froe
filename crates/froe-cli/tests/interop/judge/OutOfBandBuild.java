/*
 * An index built out of band by Oak's own editors, as `oak-run index
 * --reindex` builds one.
 *
 * `oak-run-commons` is not in the image, so this reproduces its sequence
 * from the classes that are — which is also the point: every byte of the
 * artefact comes from Oak, and nothing here decides what an index or a
 * definitions file looks like.
 *
 * The sequence is `IndexerSupport`'s, in its own order
 * (`docs/analysis/index-definitions.md` §7.4, §7.5):
 *
 *  1. an in-memory copy of the **named checkpoint's** state — the only
 *     state an asynchronous definition's lane can resume from, since every
 *     lane cycle commits after taking its checkpoint and the head's root
 *     therefore never equals a lane checkpoint's;
 *  2. the lane switch and the `reindex` flag oak-run sets before a build;
 *  3. Oak's own cycle on the switched lane, driven as a diff under the
 *     visible-editor filter, the cycle performing the traversal from the
 *     missing state internally;
 *  4. the lanes switched back;
 *  5. the artefact: Oak's own dumper for the index directory and its
 *     `index-details.txt`, Oak's own `JsonSerializer` under the printer's
 *     out-of-band filter for `index-definitions.json`, and
 *     `indexer-info.properties` through `java.util.Properties`.
 *
 * **One recorded departure from oak-run.** oak-run hands the editor
 * provider a filesystem directory factory, so the files land in a local
 * directory and its post-index step copies them into the artefact. This
 * lets the provider write into the in-memory copy's `:data` as it
 * ordinarily would, and then runs **Oak's own `LuceneIndexDumper`** over
 * that copy. The resulting file bytes are the same — the `lucene_dump`
 * phase is what says so, having held froe's dump of a `:data` subtree
 * byte-identical against that very dumper — and it makes `index-details.txt`
 * Oak's own output rather than this file's guess at its format.
 */
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.Properties;

import org.apache.jackrabbit.oak.commons.json.JsopBuilder;
import org.apache.jackrabbit.oak.json.JsonSerializer;
import org.apache.jackrabbit.oak.plugins.index.IndexUpdate;
import org.apache.jackrabbit.oak.plugins.index.IndexUpdateCallback;
import org.apache.jackrabbit.oak.plugins.index.lucene.LuceneIndexEditorProvider;
import org.apache.jackrabbit.oak.plugins.index.lucene.directory.LuceneIndexDumper;
import org.apache.jackrabbit.oak.plugins.memory.MemoryNodeStore;
import org.apache.jackrabbit.oak.spi.commit.CommitInfo;
import org.apache.jackrabbit.oak.spi.commit.EditorDiff;
import org.apache.jackrabbit.oak.spi.commit.EmptyHook;
import org.apache.jackrabbit.oak.spi.commit.VisibleEditor;
import org.apache.jackrabbit.oak.spi.state.NodeBuilder;
import org.apache.jackrabbit.oak.spi.state.NodeState;
import org.apache.jackrabbit.oak.spi.state.NodeStateUtils;

public final class OutOfBandBuild {

    /** The lane oak-run switches a definition onto for an offline build. */
    private static final String OFFLINE_LANE = "offline-reindex-async";

    /**
     * The filter `IndexerSupport.dumpIndexDefinitions` sets, verbatim.
     *
     * It differs from the printer's default in its node excludes: this one
     * keeps `:status`, which is why an oak-run artefact carries the copy's
     * status node.
     */
    private static final String OUT_OF_BAND_FILTER =
            "{\"properties\":[\"*\", \"-:childOrder\"],"
                    + "\"nodes\":[\"*\", \"-:index-definition\", \"-:data\", \"-:suggest-data\"]}";

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 4) {
            StoreSupport.refuse(
                    "usage: OutOfBandBuild <store> <indexPath> <checkpoint> <outputDirectory>");
            return;
        }
        String store = arguments[0];
        String indexPath = arguments[1];
        String checkpoint = arguments[2];
        File output = new File(arguments[3]);

        try (StoreSupport support = StoreSupport.open(store)) {
            NodeState checkpointRoot = support.nodeStore().retrieve(checkpoint);
            if (checkpointRoot == null) {
                StoreSupport.refuse(checkpoint + " does not resolve in " + store);
                return;
            }
            MemoryNodeStore copy = new MemoryNodeStore(checkpointRoot);
            build(copy, indexPath);
            writeArtefact(copy, indexPath, checkpoint, output);
        }
    }

    /** Steps 2 to 4: switch the lane, reindex, switch it back. */
    private static void build(MemoryNodeStore copy, String indexPath) throws Exception {
        String definitionName = indexPath.substring(indexPath.lastIndexOf('/') + 1);

        NodeState before = copy.getRoot();
        NodeBuilder builder = before.builder();
        NodeBuilder definition = builder.child("oak:index").child(definitionName);
        definition.setProperty("async", OFFLINE_LANE);
        definition.setProperty("reindex", true);

        IndexUpdate update = new IndexUpdate(
                new LuceneIndexEditorProvider(),
                OFFLINE_LANE,
                builder.getNodeState(),
                builder,
                IndexUpdateCallback.NOOP);
        // The cycle sees `reindex = true` and performs the traversal from
        // the missing state itself; the diff only has to reach it.
        Exception failure = EditorDiff.process(new VisibleEditor(update), before, builder.getNodeState());
        if (failure != null) {
            throw failure;
        }
        copy.merge(builder, EmptyHook.INSTANCE, CommitInfo.EMPTY);

        // `switchIndexLanesBack`, which is what leaves `refresh = true` on
        // the definition oak-run then prints.
        NodeBuilder revert = copy.getRoot().builder();
        NodeBuilder reverted = revert.child("oak:index").child(definitionName);
        reverted.setProperty("async", "async");
        reverted.setProperty("refresh", true);
        copy.merge(revert, EmptyHook.INSTANCE, CommitInfo.EMPTY);
    }

    /** Step 5: the three files and the index directory oak-run leaves. */
    private static void writeArtefact(
            MemoryNodeStore copy, String indexPath, String checkpoint, File output)
            throws IOException {
        File dumps = new File(output, "index-dumps");
        if (!dumps.mkdirs() && !dumps.isDirectory()) {
            throw new IOException("cannot create " + dumps);
        }

        // Oak's own dumper: the index directory and its index-details.txt.
        LuceneIndexDumper dumper = new LuceneIndexDumper(copy.getRoot(), indexPath, dumps);
        dumper.dump();

        // Oak's own serializer, under the out-of-band build's own filter.
        JsopBuilder json = new JsopBuilder();
        json.object();
        json.key(indexPath);
        NodeState definition = NodeStateUtils.getNode(copy.getRoot(), indexPath);
        new JsonSerializer(json, OUT_OF_BAND_FILTER, new org.apache.jackrabbit.oak.json.BlobSerializer())
                .serialize(definition);
        json.endObject();
        Files.writeString(
                new File(dumps, "index-definitions.json").toPath(),
                JsopBuilder.prettyPrint(json.toString()),
                StandardCharsets.UTF_8);

        // The properties file, written the way Oak writes one, so its
        // escaping is Oak's rather than this file's.
        Properties info = new Properties();
        info.setProperty("checkpoint", checkpoint);
        try (FileOutputStream out = new FileOutputStream(new File(dumps, "indexer-info.properties"))) {
            info.store(out, null);
        }
    }
}
