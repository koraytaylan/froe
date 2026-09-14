/*
 * What Oak's own counter editor computes, as a known-answer vector.
 *
 * The counter index is the one froe rebuild whose output cannot be checked
 * against content: its `:cnt` values are a function of a SipHash chain, a
 * seed and a resolution, and a rebuild that got the chain subtly wrong would
 * produce a plausible map that disagreed with Oak's next cycle in a way that
 * only grows. So the oracle is Oak's editor itself, run over a tree this
 * class builds in memory.
 *
 * The seed is deliberately outside the `int` range. Oak's provider draws the
 * most significant 64 bits of a random UUID and uses them untruncated on the
 * run that *creates* the seed, while narrowing them to 32 bits sign-extended
 * on every later run — so a rebuild that skipped the narrowing would match
 * the first cycle and diverge from every one after it. A seed that does not
 * fit `int` is what makes that difference visible in the vector.
 *
 * Prints `path<TAB>cnt`, one line per node of the resulting `:index`,
 * depth-first by path.
 */
import java.util.ArrayList;
import java.util.List;

import org.apache.jackrabbit.oak.plugins.index.IndexConstants;
import org.apache.jackrabbit.oak.plugins.index.IndexUpdateProvider;
import org.apache.jackrabbit.oak.plugins.index.counter.NodeCounterEditorProvider;
import org.apache.jackrabbit.oak.plugins.memory.EmptyNodeState;
import org.apache.jackrabbit.oak.plugins.memory.MemoryNodeStore;
import org.apache.jackrabbit.oak.spi.commit.CommitInfo;
import org.apache.jackrabbit.oak.spi.commit.EditorHook;
import org.apache.jackrabbit.oak.spi.state.NodeBuilder;
import org.apache.jackrabbit.oak.spi.state.NodeState;
import org.apache.jackrabbit.oak.spi.state.NodeStore;

public final class CounterVectors {

    /** Outside the `int` range, so the vector pins the 32-bit narrowing. */
    private static final long SEED = -7610761686379641542L;

    /** Small enough that a modest tree produces hits. */
    private static final long RESOLUTION = 8L;

    /** How many children each level of the synthetic tree has. */
    private static final int FAN_OUT = 12;

    /** How deep the synthetic tree goes. */
    private static final int DEPTH = 3;

    public static void main(String[] arguments) throws Exception {
        NodeStore store = new MemoryNodeStore();

        // The definition, with no `async` property, so the synchronous cycle
        // selects it — and with the seed and resolution fixed, so the vector
        // is a function of the tree alone.
        NodeBuilder builder = store.getRoot().builder();
        NodeBuilder definition = builder
                .child(IndexConstants.INDEX_DEFINITIONS_NAME)
                .child("counter");
        definition.setProperty("jcr:primaryType",
                IndexConstants.INDEX_DEFINITIONS_NODE_TYPE,
                org.apache.jackrabbit.oak.api.Type.NAME);
        definition.setProperty(IndexConstants.TYPE_PROPERTY_NAME, "counter");
        definition.setProperty(IndexConstants.REINDEX_PROPERTY_NAME, true);
        definition.setProperty("seed", SEED);
        definition.setProperty("resolution", RESOLUTION);

        populate(builder, DEPTH);

        // Oak's own index update over the counter's editor provider, from
        // the missing state — which is what a reindex is.
        store.merge(builder,
                new EditorHook(new IndexUpdateProvider(new NodeCounterEditorProvider())),
                CommitInfo.EMPTY);

        NodeState index = store.getRoot()
                .getChildNode(IndexConstants.INDEX_DEFINITIONS_NAME)
                .getChildNode("counter")
                .getChildNode(IndexConstants.INDEX_CONTENT_NODE_NAME);
        StringBuilder output = new StringBuilder();
        print(index, "", output);
        System.out.print(output);
    }

    /** A deterministic tree: `n0`…`n11` at each level, `DEPTH` deep. */
    private static void populate(NodeBuilder parent, int remaining) {
        if (remaining == 0) {
            return;
        }
        for (int index = 0; index < FAN_OUT; index++) {
            NodeBuilder child = parent.child("n" + index);
            child.setProperty("jcr:primaryType", "nt:unstructured",
                    org.apache.jackrabbit.oak.api.Type.NAME);
            populate(child, remaining - 1);
        }
    }

    /** Depth-first by path, `path<TAB>cnt`, nodes without `:cnt` skipped. */
    private static void print(NodeState node, String path, StringBuilder output) {
        if (!node.exists()) {
            return;
        }
        if (node.hasProperty(":cnt")) {
            output.append(path.isEmpty() ? "/" : path)
                    .append('\t')
                    .append(node.getProperty(":cnt").getValue(
                            org.apache.jackrabbit.oak.api.Type.LONG))
                    .append('\n');
        }
        List<String> names = new ArrayList<>();
        node.getChildNodeNames().forEach(names::add);
        names.sort(String::compareTo);
        for (String name : names) {
            print(node.getChildNode(name), path + "/" + name, output);
        }
    }

    private CounterVectors() {
    }
}
