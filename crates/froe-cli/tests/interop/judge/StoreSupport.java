/*
 * Opening the fixture as Oak itself opens it.
 *
 * Every judge class starts here, and nothing here is froe's: the store is
 * opened through Oak's own read-only file-store builder, exactly as
 * oak-run-commons opens one for a fixture. If froe's reading of a store
 * disagreed with Oak's, this is the side that is right by definition.
 *
 * No oak-run and no oak-run-commons are in the image, so where a plan relies
 * on one of their helpers the judge re-implements it from the shipped
 * bundles. Compiling against those bundles alone is itself an assertion that
 * every class the judge needs is in the image.
 */
import java.io.File;

import org.apache.jackrabbit.oak.segment.SegmentNodeStoreBuilders;
import org.apache.jackrabbit.oak.segment.file.FileStore;
import org.apache.jackrabbit.oak.segment.file.FileStoreBuilder;
import org.apache.jackrabbit.oak.segment.file.ReadOnlyFileStore;
import org.apache.jackrabbit.oak.spi.state.NodeStore;

final class StoreSupport implements AutoCloseable {

    private final AutoCloseable fileStore;
    private final NodeStore nodeStore;

    private StoreSupport(AutoCloseable fileStore, NodeStore nodeStore) {
        this.fileStore = fileStore;
        this.nodeStore = nodeStore;
    }

    /**
     * Opens the segment store at {@code directory} read-only.
     *
     * Read-only is not a convenience: the store is bind-mounted read-only
     * into the container, and a writable open would take the repository lock
     * and write a manifest, which would make the judge's own run a mutation
     * of the fixture every later phase reads.
     */
    static StoreSupport open(String directory) throws Exception {
        ReadOnlyFileStore fileStore = FileStoreBuilder
                .fileStoreBuilder(new File(directory))
                .buildReadOnly();
        return new StoreSupport(fileStore, SegmentNodeStoreBuilders.builder(fileStore).build());
    }

    NodeStore nodeStore() {
        return nodeStore;
    }

    /**
     * Opens the segment store at {@code directory} for **writing**.
     *
     * Only ever used against a copy. A writable open takes the repository
     * lock and writes a manifest, so pointing it at the shared fixture would
     * make the judge's own run a mutation of what every later phase reads.
     */
    static StoreSupport openWritable(String directory) throws Exception {
        FileStore fileStore = FileStoreBuilder
                .fileStoreBuilder(new File(directory))
                .build();
        return new StoreSupport(fileStore, SegmentNodeStoreBuilders.builder(fileStore).build());
    }

    @Override
    public void close() throws Exception {
        fileStore.close();
    }

    /** Fails with {@code message} on standard error and a non-zero status. */
    static void refuse(String message) {
        System.err.println(message);
        System.exit(1);
    }
}
