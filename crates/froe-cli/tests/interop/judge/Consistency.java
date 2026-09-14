/*
 * Oak's own index consistency checker, at the level oak-run's printer names.
 *
 * `oak-lucene`'s `IndexConsistencyChecker` asks two different questions under
 * the one word "consistency". **Level 1** (`BLOBS_ONLY`) resolves every blob
 * the `:data` subtree references — the question froe answers natively.
 * **Level 2** (`FULL`) copies the index out to a local directory and runs
 * Lucene's own `CheckIndex` over it, which needs a JVM and is the verdict
 * froe cannot produce. The `1`/`2` naming is oak-run's printer's.
 *
 * The checker runs the `CheckIndex` pass **only when the directory content
 * came out consistent**, so a caller must assert that the index-check status
 * was *reached* rather than that some verdict was printed: a blob failure
 * would otherwise read as a quiet pass at the level that matters. That is
 * what `indexCheckStatus=not-reached` is for.
 *
 * Its constructor takes the work-directory root, because the full level needs
 * somewhere to put its local copy of the index.
 */
import java.io.File;

import org.apache.jackrabbit.oak.plugins.index.lucene.directory.IndexConsistencyChecker;
import org.apache.jackrabbit.oak.spi.state.NodeState;

public final class Consistency {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 4) {
            StoreSupport.refuse("usage: Consistency <store> <indexPath> <level> <workDirectory>");
            return;
        }
        String store = arguments[0];
        String indexPath = arguments[1];
        int level = Integer.parseInt(arguments[2]);
        File workDirectory = new File(arguments[3]);
        if (level != 1 && level != 2) {
            StoreSupport.refuse("level must be 1 or 2, not " + level);
            return;
        }
        if (!workDirectory.isDirectory() && !workDirectory.mkdirs()) {
            StoreSupport.refuse("cannot create the work directory " + workDirectory);
            return;
        }

        try (StoreSupport support = StoreSupport.open(store)) {
            NodeState root = support.nodeStore().getRoot();
            IndexConsistencyChecker checker =
                    new IndexConsistencyChecker(root, indexPath, workDirectory);
            IndexConsistencyChecker.Result result = checker.check(
                    level == 1
                            ? IndexConsistencyChecker.Level.BLOBS_ONLY
                            : IndexConsistencyChecker.Level.FULL);

            System.out.println("indexPath=" + result.indexPath);
            System.out.println("clean=" + result.clean);
            System.out.println("typeMismatch=" + result.typeMismatch);
            System.out.println("missingBlobs=" + result.missingBlobs);
            System.out.println("blobSizeMismatch=" + result.blobSizeMismatch);
            System.out.println("binaryPropSize=" + result.binaryPropSize);
            System.out.println("missingBlobIds=" + result.missingBlobIds.size());
            System.out.println("invalidBlobIds=" + result.invalidBlobIds.size());
            System.out.println("indexCheckStatus=" + describe(result));
            for (IndexConsistencyChecker.DirectoryStatus status : result.dirStatus) {
                System.out.println("dir=" + status.dirName
                        + " clean=" + status.clean
                        + " size=" + status.size
                        + " numDocs=" + status.numDocs
                        + " missingFiles=" + status.missingFiles.size()
                        + " sizeMismatch=" + status.filesWithSizeMismatch.size()
                        + " checkIndex=" + (status.status == null
                                ? "not-reached"
                                : Boolean.toString(status.status.clean)));
            }
        }
    }

    /**
     * Whether the `CheckIndex` pass actually ran, and what it said.
     *
     * `DirectoryStatus.status` is populated only by the FULL level, and only
     * once the directory's own content was found consistent. "not-reached"
     * is therefore a real answer rather than a missing one, and the phase
     * refuses it.
     */
    private static String describe(IndexConsistencyChecker.Result result) {
        if (result.dirStatus == null || result.dirStatus.isEmpty()) {
            return "not-reached";
        }
        boolean reached = false;
        boolean clean = true;
        for (IndexConsistencyChecker.DirectoryStatus status : result.dirStatus) {
            if (status.status == null) {
                continue;
            }
            reached = true;
            if (!status.status.clean) {
                clean = false;
            }
        }
        if (!reached) {
            return "not-reached";
        }
        return clean ? "clean" : "unclean";
    }
}
