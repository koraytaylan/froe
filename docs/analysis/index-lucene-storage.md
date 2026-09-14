# Index storage: Lucene in the repository

How a Lucene index directory is stored as repository content, how Oak reads it
back, and the filesystem layouts oak-run uses to move it in and out. This is
the reader specification for plan 0006, the writer specification for plan
0008's importer, and the storage target for plan 0010's native reindex.

It deliberately stops at the file boundary. A Lucene index is a set of named
files; this document specifies the *node* each file is stored in, and says
nothing about the bytes *inside* one. Those bytes are the Lucene 4.7.2 format:
plan 0008 adds its table of contents as a section of this document before
reading it, and plan 0009 specifies the write side in full.

Java sources cited below are under `oak-lucene/src/main/java/` and
`oak-search/src/main/java/`, with the package prefix
`org/apache/jackrabbit/oak/plugins/index/` elided, so
`lucene/directory/OakDirectory.java` is
`oak-lucene/src/main/java/org/apache/jackrabbit/oak/plugins/index/lucene/directory/OakDirectory.java`.
Where another module is meant, it is named. The revision read is the one
[`README.md`](README.md) pins, Apache Jackrabbit Oak commit
`4984c4cf26a7ca58ae9ce12c63190b7f492bda78`; where the consumer build this
repository verifies against — `oak-segment-tar` 1.90.0 inside the digest-pinned
Sling image — differs, the difference is called out and the consumer build wins.

Builds on (does not repeat):

- [`index-definitions.md`](index-definitions.md) — the definition node, the
  `async` lane, the reindex protocol and the import protocol's bookkeeping.
- [`record-layer.md`](record-layer.md) — the value records a binary property's
  blocks are stored in; froe's `read_binary_stream` reads them.
- [`index-property-storage.md`](index-property-storage.md) — the property
  family's storage, and the visible-editor rule that governs every index
  update.

---

## 1. The directory node

A Lucene index stores each of its files as a child of a hidden *directory*
node under the definition. There are two directories and, with composite
mounts, one decorated name per mount for each:

| Directory | Default name | Mount-decorated name |
| --- | --- | --- |
| the index | `:data` | `:<mount fragment>-index-data` |
| the suggester | `:suggest-data` | `:<mount fragment>-suggest-data` |

`FulltextIndexConstants.INDEX_DATA_CHILD_NAME = ":data"`
(`oak-search`, `search/FulltextIndexConstants.java`);
`LuceneIndexConstants.SUGGEST_DATA_CHILD_NAME = ":suggest-data"`; the suffixes
and the tests that recognize them are
`lucene/writer/MultiplexersLucene.java`:

```java
public static final String INDEX_DIR_SUFFIX = "-index-data";
public static final String SUGGEST_DIR_SUFFIX = "-suggest-data";

public static boolean isIndexDirName(String name) {
    return name.endsWith(INDEX_DIR_SUFFIX)
            || name.equals(FulltextIndexConstants.INDEX_DATA_CHILD_NAME);
}
```

froe detects and reports mount-decorated directories; it does not model
composite mounts.

### 1.1 `dirListing`

`lucene/directory/OakDirectory.java` declares
`public static final String PROP_DIR_LISTING = "dirListing"` and both uses it
and writes it under the same setting.

**Read** (`getListing`, called once from the constructor):

```java
Iterable<String> fileNames = null;
if (definition.saveDirListing()) {
    PropertyState listing = directoryBuilder.getProperty(PROP_DIR_LISTING);
    if (listing != null) {
        fileNames = listing.getValue(Type.STRINGS);
    }
}
if (fileNames == null){
    fileNames = directoryBuilder.getChildNodeNames();
}
```

**Write** (`close`):

```java
if (!readOnly && definition.saveDirListing()) {
    if (!fileNamesAtStart.equals(fileNames)) {
        …
        directoryBuilder.setProperty(createProperty(PROP_DIR_LISTING, fileNames, STRINGS));
    }
}
```

Four consequences:

1. **`saveDirectoryListing` gates both sides.** It is
   `getOptionalValue(defn, LuceneIndexConstants.SAVE_DIR_LISTING, true)`
   (`lucene/LuceneIndexDefinition.java`, constructor), with
   `SAVE_DIR_LISTING = "saveDirectoryListing"`, so the default is **true**.
   Under `saveDirectoryListing = false` Oak never reads the property, so a
   **stale `dirListing` left on such a definition is ignored** and the child
   names are the listing. froe reproduces that exactly; reading the property
   unconditionally would make it disagree with Oak on a store where the flag
   was turned off after the property was written.
2. **The listing is authoritative when it is read**, even where it disagrees
   with the children: `fileNames` is seeded from it and `listAll`,
   `fileExists` and `openInput` all work from that set. A file node present as
   a child but absent from the listing is invisible to Oak.
3. **It is written only when the file set changed** since the directory was
   opened (`!fileNamesAtStart.equals(fileNames)`), so an unchanged directory
   keeps whatever order it already had.
4. **Its stored order is neither sorted nor insertion order.** `fileNames` is
   `SetUtils.newConcurrentHashSet()`, so the array Oak writes is in that hash
   set's iteration order. Oak reads it back into a `LinkedHashSet`
   (`SetUtils.toLinkedSet`) and only ever asks it set questions. **Compare a
   `dirListing` as a set, never as a sequence** — a rebuild that sorted it
   would be as correct as Oak's own output, and a comparison that demanded
   the same order would fail against Oak for no reason.

The fixture's `:data` shows the disorder plainly (§9).

### 1.2 `unsafeForActiveDeletion`

`PROP_UNSAFE_FOR_ACTIVE_DELETION = "unsafeForActiveDeletion"` is a **file-node**
property, not a directory one, written in `createOutput`:

```java
if (blobDeletionCallback.isMarkingForActiveDeletionUnsafe()) {
    file.setProperty(PROP_UNSAFE_FOR_ACTIVE_DELETION, true);
}
```

and read in `deleteFile`, where a `true` value suppresses the
blob-deletion callback for that file's blobs. In a segment store with no
external blob store the directory is built with
`BlobFactory.getNodeBuilderBlobFactory(builder)` and
`BlobDeletionCallback.NOOP`, whose `isMarkingForActiveDeletionUnsafe` delegates
to `ActiveDeletedBlobCollectorFactory.NOOP.isActiveDeletionUnsafe()`
(`lucene/directory/ActiveDeletedBlobCollectorFactory.java`), so the property
is not written there. froe reads it where it is present and never writes it.

## 2. The file node

`OakDirectory.createOutput` writes the two metadata properties, and the
`OakIndexFile` implementations write the other two on flush:

```java
byte[] uniqueKey = new byte[UNIQUE_KEY_SIZE];        // 16
secureRandom.nextBytes(uniqueKey);
String key = StringUtils.convertBytesToHex(uniqueKey);
file.setProperty(PROP_UNIQUE_KEY, key);
file.setProperty(PROP_BLOB_SIZE, definition.getBlobSize());
```

| Property | Type | Written by | Value |
| --- | --- | --- | --- |
| `uniqueKey` | `STRING` | `OakDirectory.createOutput` | 16 random bytes as 32 lower-case hexadecimal characters |
| `blobSize` | `LONG` | `OakDirectory.createOutput` | the definition's blob size |
| `jcr:lastModified` | `LONG` | `flush` in both index-file forms | `System.currentTimeMillis()` at the flush |
| `jcr:data` | `BINARY` or `BINARIES` | `flush` | §3 |

* **`uniqueKey`** is 16 bytes — `OakDirectory.UNIQUE_KEY_SIZE = 16` — rendered
  by `StringUtils.convertBytesToHex`, so the stored string is 32 characters.
  It exists "to allow removing binaries from the blob store without risking to
  remove binaries that are still needed" (the field comment in
  `OakBufferedIndexFile`): the key is appended to every stored blob so that two
  files with identical content still hash to different blob identifiers.
* **`blobSize`** is written from `IndexDefinition.getBlobSize()`, which is
  `Math.max(1024, getOptionalValue(defn, BLOB_SIZE, DEFAULT_BLOB_SIZE))`
  (`oak-search`, `search/IndexDefinition.java`) with
  `DEFAULT_BLOB_SIZE = 1024 * 1024 - 1024` — **1,047,552**. So the definition's
  value is clamped up to 1024 and the default is 1,047,552, which is the value
  the real fixture carries (§9).

  **But it is read back from the file node, not from the definition**, and the
  fallback when the file node has no `blobSize` is the *reader's own*
  constant, not the definition's default
  (`lucene/directory/OakBufferedIndexFile.java`):

  ```java
  static final int DEFAULT_BLOB_SIZE = 32 * 1024;

  private static int determineBlobSize(NodeBuilder file){
      if (file.hasProperty(OakDirectory.PROP_BLOB_SIZE)){
          return Math.toIntExact(file.getProperty(OakDirectory.PROP_BLOB_SIZE).getValue(Type.LONG));
      }
      return DEFAULT_BLOB_SIZE;
  }
  ```

  **32,768, not 1,047,552.** A reader that fell back to the definition's
  default would compute a wrong length and a wrong chunk boundary for every
  file node lacking the property. Note also that the read is `getValue(Type.LONG)`
  — **converting**, unlike the definition's strict reads — and then
  `Math.toIntExact`, which throws on a value outside `int`.
* **`jcr:lastModified`** is wall-clock at the flush, so it is not reproducible
  and not comparable between two builds of the same content.

## 3. The two `jcr:data` encodings

`lucene/directory/OakIndexFile.java` dispatches on the stored type alone:

```java
boolean useStreaming;
PropertyState property = file.getProperty(JCR_DATA);
if (property != null) { //reading
        useStreaming = property.getType() == BINARY;
} else { //writing
    useStreaming = streamingWriteEnabled;
}
return useStreaming ?
        new OakStreamingIndexFile(name, file, dirDetails, blobFactory) :
        new OakBufferedIndexFile(name, file, dirDetails, blobFactory);
```

**A single `BINARY` reads as streaming; anything else reads as buffered.**
Note what "anything else" means for a malformed node: a `jcr:data` of some
third type — a `STRING`, say — takes the buffered branch, whose constructor
then finds `property.getType() != BINARIES` and leaves `data` empty, so **Oak
reads such a file as zero-length and never errors**.

> **froe deviation (stricter, never permissive).** froe raises a typed error
> for a `jcr:data` that is neither `BINARY` nor `BINARIES`, rather than
> reporting a zero-length file. The deviation loses no data Oak would return —
> Oak returns nothing for such a node either — and writes no bytes; it only
> refuses to present a silent zero where the store is malformed. Recorded at
> `crates/froe/src/index/lucene/directory.rs`.

### 3.1 The streaming form

`lucene/directory/OakStreamingIndexFile.java`, constructor:

```java
PropertyState property = file.getProperty(JCR_DATA);
if (property != null) {
    if (property.getType() == BINARY) {
        this.blob = property.getValue(BINARY);
    } else {
        throw new IllegalArgumentException("Can't load blob for streaming for " + name + " under " + file);
    }
} else {
    this.blob = null;
}
if (blob != null) {
    this.length = blob.length();
    if (uniqueKey != null) {
        this.length = Math.max(0, this.length - uniqueKey.length);
    }
}
```

**One blob, the unique key appended once, the reported length being the blob
length minus the key length and never below zero.** `flush` writes it back as
`file.setProperty(JCR_DATA, blob, BINARY)`.

### 3.2 The buffered form

`lucene/directory/OakBufferedIndexFile.java`, constructor:

```java
PropertyState property = file.getProperty(JCR_DATA);
if (property != null && property.getType() == BINARIES) {
    this.data = ListUtils.toList(property.getValue(BINARIES));
} else {
    this.data = new ArrayList<>();
}

this.length = (long)data.size() * blobSize;
if (!data.isEmpty()) {
    Blob last = data.get(data.size() - 1);
    this.length -= blobSize - last.length();
    if (uniqueKey != null) {
        this.length -= uniqueKey.length;
    }
}
```

So with `n` chunks and a final chunk of `last` stored bytes:

```text
length = n * blobSize - (blobSize - last.length()) - uniqueKey.length
```

**Every** chunk carries the unique key appended to it — `flushBlob` writes
`n = min(blobSize, length - index * blobSize)` payload bytes followed by the
key — so a non-final chunk is stored as `blobSize + 16` bytes, not `blobSize`.
The formula nevertheless subtracts the key length exactly once, and that is
arithmetic rather than an assumption about the other chunks: `n * blobSize`
assumes every chunk holds a full `blobSize` of payload, so the last chunk must
be corrected by `blobSize - payload(last)`; `last.length()` is the *stored*
length, which includes the key, so `blobSize - last.length()` under-corrects by
exactly the key length, and the separate `- uniqueKey.length` makes it up. With
`p` the last chunk's payload bytes, the whole expression reduces to
`(n - 1) * blobSize + p`, the true file length.

Loading chunk `i` reads only the bytes that belong to the file, never the
trailing key — the same `min` bound, which is why the key is invisible to a
reader without ever being addressed:

```java
int n = (int) Math.min(blobSize, length - (long)i * blobSize);
try (InputStream stream = data.get(i).getNewStream()) {
    IOUtils.readFully(stream, blob, 0, n);
}
```

`flush` writes it back as `file.setProperty(JCR_DATA, data, BINARIES)`.

### 3.3 An absent `uniqueKey` is length-neutral

Both readers use the same helper, in each file (`OakBufferedIndexFile`,
`OakStreamingIndexFile`):

```java
private static byte[] readUniqueKey(NodeBuilder file) {
    if (file.hasProperty(OakDirectory.PROP_UNIQUE_KEY)) {
        String key = file.getString(OakDirectory.PROP_UNIQUE_KEY);
        return StringUtils.convertHexToBytes(key);
    }
    return null;
}
```

A `null` key subtracts no length in either form. **froe therefore treats an
absent `uniqueKey` as "no trailing key", not as a defect** — the file simply
has no key bytes appended, and its length is the whole blob.

Note that `file.getString` is the strict `STRING` read, so a `uniqueKey` of
another type reaches `convertHexToBytes` as `null`; that is a malformed store
froe refuses with a typed error rather than reproducing Oak's
`NullPointerException`.

### 3.4 Which writer produces which

| Writer | Form | Why |
| --- | --- | --- |
| Oak's own Lucene editor | **streaming** | `BufferedOakDirectory.ENABLE_WRITING_SINGLE_BLOB_INDEX_FILE_PARAM = "oak.lucene.enableSingleBlobIndexFiles"`, read as `Boolean.parseBoolean(System.getProperty(…, "true"))`, and the OSGi default `PROP_ENABLE_SINGLE_BLOB_INDEX_FILES_DEFAULT = true` in `lucene/LuceneIndexProviderService.java` |
| Oak's own Lucene **importer** | **buffered** | `LuceneIndexImporter.copyDirectory` builds `new OakDirectory(definitionBuilder, jcrName, definition, false, blobStore)`, the constructor overload that leaves `streamingWriteEnabled` at `false` |

So **the shape a store holds says which tool last wrote it**, and a store that
Oak wrote and oak-run then imported into holds both shapes side by side. The
real 1.90.0 fixture holds streaming files (§9). froe reads both and, for plan
0008's importer, writes the buffered form Oak's own importer writes.

## 4. What the blobs are, in a segment store

The directory's blob factory is
`BlobFactory.getNodeBuilderBlobFactory(builder)`, which is
`builder::createBlob` (`lucene/directory/BlobFactory.java`). For a segment
store that stores the bytes **through the node builder**, so every chunk is an
ordinary inline binary value record with a block list over bulk segments,
addressed exactly as [`record-layer.md`](record-layer.md) already specifies.
There is no Lucene-specific encoding below the property: froe's
`read_binary_stream` reads these blobs unchanged, and a file of any size
costs constant memory.

The alternative factory, `BlobFactory.getBlobStoreBlobFactory(store)`, is
selected only when the directory is constructed with a
`GarbageCollectableBlobStore`; it writes to an external data store and stores a
`BlobStoreBlob` reference. External blob stores stay out of scope, as they are
for the rest of froe.

## 5. The consistency check

`lucene/directory/IndexConsistencyChecker.java` has two levels:

```java
public enum Level {
    /** Consistency check would only check if all blobs referred by index nodes
     *  are present in BlobStore */
    BLOBS_ONLY,
    /** Performs full check via {@code org.apache.lucene.index.CheckIndex}. This
     *  reads whole index and hence can take time */
    FULL
}
```

and `check` runs the blob pass always and the full pass only if the blob pass
was clean (`if (level == Level.FULL && result.clean) { checkIndex(result, closer); }`).

**Level 1 (`BLOBS_ONLY`)** — `checkBlobs`:

1. Refuse unless the definition's `type` reads as `lucene`:
   `type != null && LuceneIndexConstants.TYPE_LUCENE.equals(type.getValue(Type.STRING))`,
   a **converting** read; otherwise `result.typeMismatch = true` and the index
   is not clean.
2. Recurse over the definition subtree, and for every `BINARY`-tagged
   property — single- or multi-valued — stream the blob to its end through a
   `CountingInputStream` and compare the streamed count with `blob.length()`.
   A mismatch is an invalid blob; a missing one is a missing blob.

   **The walk covers hidden children and hidden properties.** It goes through
   `RootFactory.createReadOnlyRoot(rootState)`, which builds an `ImmutableRoot`
   over `ImmutableTree`, and `ImmutableTree` overrides
   `protected boolean isHidden(String name) { return false; }`
   (`oak-core`, `plugins/tree/impl/ImmutableTree.java`) precisely so that it
   "does not filter out 'hidden' items", as its class comment says. `:data`,
   `:suggest-data` and every mount-decorated directory are therefore all
   covered. A froe check that skipped hidden children would check nothing at
   all.

**Level 2 (`FULL`)** adds, per directory child that `isIndexDirName` or
`isSuggestIndexDirName` accepts: a copy of every file to a local filesystem
directory, a length comparison between source and copy per file, then Lucene's
own `CheckIndex` over the copy, then `DirectoryReader.open(targetDir).numDocs()`.

`froe index check` implements **level 1**, and says so per index. Level 2
needs the Lucene file format, which plan 0008 adds; the interop suite gets the
level-2 verdict from the judge running Lucene's own `CheckIndex` in the
meantime.

## 6. The filesystem layouts

### 6.1 The dump layout

`lucene/directory/LuceneIndexDumper.dump`:

```java
indexDir = DirectoryUtils.createIndexDir(baseDir, indexPath);
IndexMeta meta = new IndexMeta(indexPath);
for (String dirName : idx.getChildNodeNames()) {
    if (NodeStateUtils.isHidden(dirName) &&
            (isIndexDirName(dirName) || isSuggestIndexDirName(dirName))) {
        copyContent(idx, defn, meta, indexDir, dirName, closer);
    }
}
DirectoryUtils.writeMeta(indexDir, meta);
```

**The index folder's base name** — `lucene/directory/IndexRootDirectory.java`,
`getIndexFolderBaseName`: the path's elements reversed, at most three taken,
`oak:index` dropped, each remaining element passed through
`getFSSafeName(e)`, which is `e.replaceAll("\\W", "")` — every character
outside `[A-Za-z0-9_]` stripped, the underscore included in `\w` and therefore
**kept**, against the method's own Javadoc, which says "Strip of any char
outside of a-zA-Z0-9-"; the `TODO Exclude -_ like chars via [^\W_]` on the line
above is the acknowledgement that the comment describes an intention and the
code describes the behaviour. The code wins — then reversed back and joined
with `_`, then truncated to `MAX_NAME_LENGTH = 127` characters. So `/oak:index/lucene` gives `lucene`, and
`/content/oak:index/abc` gives `content_abc`.

**Reuse before creation.** `DirectoryUtils.createIndexDir` first asks
`IndexRootDirectory.getLocalIndexes(indexPath)` for an existing directory whose
`index-details.txt` names the same `indexPath` and uses the first one if there
is any; only when there is none does it make a new one, appending `_0`, `_1`
and so on while the name is taken:

```java
if (existingDirs.isEmpty()) {
    indexDir = new File(baseDir, subDirPath);
    int count = 0;
    while (true) {
        if (indexDir.exists()) {
            indexDir = new File(baseDir, subDirPath + "_" + count++);
        } else { break; }
    }
    FileUtils.forceMkdir(indexDir);
} else {
    indexDir = existingDirs.get(0).dir;
}
```

**The per-index subdirectory name** strips colons and nothing else —
`DirectoryUtils.createSubDir`: `String fsSafeName = name.replace(":", "")` —
so `:data` becomes `data` and `:suggest-data` becomes `suggest-data`. Note
that this is a *different* rule from `getFSSafeName`: the hyphen survives here
and would not survive there.

**`index-details.txt`** is written beside the copied directories,
`IndexRootDirectory.INDEX_METADATA_FILE_NAME = "index-details.txt"`, by
`IndexMeta.writeTo` through `java.util.Properties.store`:

```java
Properties p = new Properties();
p.putAll(properties);                                    // the dir.* mappings
p.setProperty("metaFormatVersion", String.valueOf(metaFormatVersion));  // 1
p.setProperty("indexPath", indexPath);
p.setProperty("creationTime", String.valueOf(creationTime));
p.store(os, "Index metadata");
```

with one `dir.<filesystem name>=<jcr name>` line per copied directory
(`addDirectoryMapping`: `properties.put(DIR_PREFIX + fsDirName, jcrDirName)`,
`DIR_PREFIX = "dir."`). The mapping is keyed by the **filesystem** name and
holds the **JCR** name, which is the direction the importer reads it
(`getJcrNameFromFSName`); `getFSNameFromJCRName` scans the same map backwards.

Because it is written and read with `java.util.Properties`, the file is in
Java properties syntax: `#`/`!` comment lines, `key=value` with `:` and `=`
accepted as separators, backslash escapes, and `\uXXXX`. froe's reader accepts
what Java's reader accepts, within a declared size bound, and its writer
escapes what Java's writer escapes — these files arrive from an import
directory, so an oversized or malformed one is a typed error naming the file
and line, never a panic or an unbounded allocation.

### 6.2 The import layout

`oak-core`, `index/importer/IndexerInfo.java`:

* **`indexer-info.properties`** (`INDEXER_META`) is read from the **root**
  directory the importer is handed, and carries one property, `checkpoint`:

  ```java
  public static IndexerInfo fromDirectory(File rootDir) throws IOException {
      File infoFile = new File(rootDir, INDEXER_META);
      checkArgument(infoFile.exists(), "No [%s] file found in [%s]. Not a valid exported index " +
              "directory", INDEXER_META, rootDir.getAbsolutePath());
      Properties p = PropUtils.loadFromFile(infoFile);
      return new IndexerInfo(rootDir, PropUtils.getProp(p, "checkpoint"));
  }
  ```

* **Every direct subdirectory** of that root is then scanned for
  `index-details.txt`, and each one carrying an `indexPath` property becomes
  one local index directory (`getIndexes`, which preserves directory order in
  a `LinkedHashMap` because "order might matter"). A subdirectory without the
  file, or with one lacking `indexPath`, is skipped silently.

`lucene/directory/LuceneIndexImporter.importIndex` then, per index:

```java
definitionBuilder.getChildNode(IndexDefinition.STATUS_NODE).remove();
ReindexOperations reindexOps = new ReindexOperations(root, definitionBuilder, localIndex.getJcrPath(),
        new LuceneIndexDefinition.Builder());
LuceneIndexDefinition definition = (LuceneIndexDefinition)reindexOps.apply(true);
for (File dir : localIndex.dir.listFiles(File::isDirectory)) {
    String jcrName = localIndex.indexMeta.getJcrNameFromFSName(dir.getName());
    if (jcrName != null) {
        copyDirectory(definition, definitionBuilder, jcrName, dir);
    }
}
```

1. **`:status` is removed whole** — not a property of it, the node.
2. **The stored definition is applied from the *updated* state**:
   `reindexOps.apply(true)` passes `useStateFromBuilder = true`, so
   `:index-definition` is a visible clone of `definitionBuilder.getNodeState()`
   rather than of its base state (§7.2). The same call sets `:version`, removes
   `indexImportState`, removes `:status/reindexCompletionTimestamp` if the node
   still existed, and calls `configureUniqueId`.
3. **Each mapped directory is removed and rewritten**:
   `definitionBuilder.getChildNode(jcrName).remove()` before the new
   `OakDirectory` is opened, so a directory being imported is replaced whole
   and any *other* hidden child is left standing — which is the comment at the
   call: "the builder can have existing hidden node structures. So remove the
   ones which are being imported and leave others as is."
4. **Files are copied through `OakDirectory`**, so they land in the buffered
   form (§3.4), each with a fresh `uniqueKey` and the definition's `blobSize`.

## 7. Status and definition bookkeeping beyond the generic index update

The generic reindex bookkeeping — `reindex`, `reindexCount`, hidden-child
removal, `corrupt` — is in [`index-definitions.md`](index-definitions.md).
This section records what the **fulltext editor family alone** writes, because
the property, reference and counter editors write none of it.

### 7.1 On writer close

`oak-search`, `search/spi/editor/FulltextIndexEditorContext.java`,
`closeWriter`, and only `if (indexUpdated)`:

```java
NodeBuilder status = definitionBuilder.child(IndexDefinition.STATUS_NODE);
status.setProperty(IndexDefinition.STATUS_LAST_UPDATED, getUpdatedTime(currentTime), Type.DATE);
status.setProperty("indexedNodes", indexedNodes);
if (reindex) {
    status.setProperty(IndexDefinition.REINDEX_COMPLETION_TIMESTAMP, ISO8601.format(currentTime), Type.DATE);
}
```

with `STATUS_NODE = ":status"`, `STATUS_LAST_UPDATED = "lastUpdated"`,
`REINDEX_COMPLETION_TIMESTAMP = "reindexCompletionTimestamp"`
(`search/IndexDefinition.java`), and

```java
private String getUpdatedTime(Calendar currentTime) {
    CommitInfo info = getIndexingContext().getCommitInfo();
    String checkpointTime = (String) info.getInfo().get(IndexConstants.CHECKPOINT_CREATION_TIME);
    if (checkpointTime != null) { return checkpointTime; }
    return ISO8601.format(currentTime);
}
```

so `lastUpdated` is the `indexingCheckpointTime` commit attribute when the
cycle has one and wall-clock otherwise. **`indexedNodes` is a per-cycle
counter**, reset to `0` in the editor context's constructor and incremented per
indexed node — it is *not* a document count, and comparing it with a Lucene
`numDocs` is a category error.

### 7.2 `uid`, `seed` and the stored definition

`configureUniqueId` writes `:status/uid` when it is absent, as time-increasing
decimal epoch milliseconds in a `STRING`:

```java
NodeBuilder status = definition.child(IndexDefinition.STATUS_NODE);
String uid = status.getString(IndexDefinition.PROP_UID);
if (uid == null) {
    uid = String.valueOf(Clock.SIMPLE.getTimeIncreasing());
    status.setProperty(IndexDefinition.PROP_UID, uid);
}
```

`createIndexDefinition`, for an asynchronous definition only, injects the
random seed when absent — `PROP_RANDOM_SEED = "seed"` — as
`UUID.randomUUID().getMostSignificantBits()`, and maintains `:index-definition`:

* on **refresh** (`refresh` consumed): the property is removed, the clone is
  rewritten from `defnState`, and `creationTimestamp` is stamped on the clone;
* when there is **no clone yet**: the clone is written and `creationTimestamp`
  stamped;
* otherwise: only the clone's `seed` is corrected to match the definition's.

**`creationTimestamp` is written only where `refresh` is consumed or the clone
is first created.** After a reindex or an import — both of which
*overwrite* an existing clone through `ReindexOperations.apply`, which does not
stamp it — the property is **absent** until a later refresh writes it. The
inventory and the importer both expect that absence; reporting it as a defect
would fail on every freshly reindexed Lucene index.

### 7.3 An empty index is still persisted

`lucene/writer/DefaultIndexWriter.close`:

```java
//If reindex or fresh index and write is null on close
//it indicates that the index is empty. In such a case trigger
//creation of write such that an empty Lucene index state is persisted
//in directory
if (reindex && writer == null) {
    getWriter();
}
```

A reindex that indexed nothing therefore still leaves a valid, empty Lucene
index in `:data` — unlike the counter and reference indexes, which leave their
hidden children absent
([`index-property-storage.md`](index-property-storage.md) §8.4, §9.5).

### 7.4 The suggester

`DefaultIndexWriter.updateSuggester` writes
`definitionBuilder.child(suggestDirName).setProperty("lastUpdated", ISO8601.format(currentTime), Type.DATE)`
after a successful update, and `shouldUpdateSuggestions` decides whether to
run:

```java
PropertyState suggesterLastUpdatedValue = suggesterStatus.getProperty("lastUpdated");
if (suggesterLastUpdatedValue != null) {
    … // only after suggestUpdateFrequencyMinutes have passed
} else {
    updateSuggestions = true;
}
```

**An absent `lastUpdated` means "rebuild now"**, which is why `:suggest-data`
may legitimately be absent from a store: Oak will rebuild the suggestions on
the next cycle that needs them. froe never reports its absence as a defect.

## 8. Table of contents formats

The bytes *inside* a file, at last — but only the five structures that make an
index's file set enumerable. Everything below is Lucene 4.7.2, which
`oak-lucene` embeds and re-exports as `4.7.2-oak2`; the sources cited are
`lucene-core` 4.7.2 under `org/apache/lucene/`, and where a path below starts
with `codecs/` or `store/` or `index/` it is relative to that.

This section stops where the postings begin. It specifies the codec header,
`segments.gen`, `segments_N`, `.si` and the compound file's table of contents
— enough to say *which files an index is made of* and to reach any one of
them. It says nothing about a term dictionary or a postings list, which plan
0009 specifies for the write side.

### 8.1 Primitive encodings

Three primitives recur, all from `store/DataInput.java` and its writing twin:

| Primitive | Encoding | Source |
| --- | --- | --- |
| `Int` | 4 bytes, big-endian | `DataOutput.writeInt` |
| `Long` | 8 bytes, big-endian | `DataOutput.writeLong` |
| `VInt` | 7 bits per byte, low group first, high bit set while more follow | `DataOutput.writeVInt` |
| `String` | a `VInt` **byte** length, then that many UTF-8 bytes | `DataOutput.writeString` (`writeVInt(utf8Result.length)` then the bytes) |
| `StringSet` | an `Int` count, then that many `String` | `DataOutput.writeStringSet` |
| `StringStringMap` | an `Int` count, then that many key/value `String` pairs | `DataOutput.writeStringStringMap` |

The `String` length is in **bytes, not characters**, and a reader that
allocates it before checking it against the remaining file length is a denial
of service on a hostile file. froe validates every length against the bytes
that remain before allocating.

### 8.2 The codec header

Every file below opens with one (`codecs/CodecUtil.java`):

| Offset | Field | Encoding |
| --- | --- | --- |
| 0 | magic | `Int` = `0x3fd76c17` (`CodecUtil.CODEC_MAGIC`) |
| 4 | codec name | `String` |
| … | version | `Int` |

`CodecUtil.writeHeader` refuses a codec name that is not simple ASCII or is
128 bytes or longer, so `headerLength` is exactly `9 + name.length()`.
`checkHeader` reads the magic and then delegates to `checkHeaderNoMagic`,
which compares the name and then range-checks the version, raising
`IndexFormatTooOldException` below the minimum and `IndexFormatTooNewException`
above the maximum. froe reports all three as distinct typed errors naming the
file and the offset, because an operator's next move differs: a wrong magic is
not a Lucene file at all, a wrong name is the wrong *kind* of Lucene file, and
a version outside the range is one this froe does not read.

### 8.3 `segments.gen`

Its own format, with no codec header (`index/SegmentInfos.java`, the
`writeSegmentsGen`/`readSegmentsGen` pair around line 270 and line 770):

| Field | Encoding |
| --- | --- |
| format | `Int` = `-2` (`SegmentInfos.FORMAT_SEGMENTS_GEN_CURRENT`) |
| generation | `Long` |
| generation, again | `Long` |

**The reading rule, and why the file is never a finding.** The commit
generation is the **maximum** of two candidates: the one derived from the
directory listing — the highest `_N` suffix among the `segments_N` names, with
`segments.gen` itself skipped — and the one in this file, which counts **only
when its two copies agree**. Oak writes it best-effort and deletes it on any
failure, so its absence, its truncation and a disagreement between its two
copies are all ordinary. froe therefore treats the file as a hint: present and
self-consistent, it can only raise the generation; anything else about it is
ignored, and **its absence is never reported as a fault**.

### 8.4 `segments_N`

The commit file (`index/SegmentInfos.java`, `read(Directory, String)` at line
314). After the codec header — name `segments`, versions `VERSION_40` = 0
through `VERSION_46` = 1 — the body is:

| Field | Encoding | Notes |
| --- | --- | --- |
| version | `Long` | the commit's own version counter |
| counter | `Int` | the next segment name to allocate |
| segment count | `Int` | **refused when negative**, as `read` does |
| *per segment* | | |
| name | `String` | e.g. `_0` |
| codec name | `String` | resolved through `Codec.forName` |
| deletion generation | `Long` | |
| deletion count | `Int` | **refused when negative or above the segment's document count** |
| field-infos generation | `Long` | **only from `VERSION_46`** |
| generation update files | `Int` count, then that many (`Long`, `StringSet`) pairs | **only from `VERSION_46`** |
| user data | `StringStringMap` | |
| checksum | `Long` | the running checksum of everything before it |

Two facts a reader must not miss. The per-segment record does **not** contain
the segment's own metadata — `read` calls into
`codec.segmentInfoFormat().getSegmentInfoReader().read(...)` mid-loop, so the
`.si` file is opened *between* the codec name and the deletion generation, and
a reader that treats the record as self-contained will misparse the rest of
the file. And the first `Int` is read raw and compared to `CODEC_MAGIC`
before any header check: a file whose first `Int` is something else is a
Lucene 3.x commit file, which this froe does not read and reports as such.

**The codec name is not fixed.** It is whatever the definition selected:
`oakCodec` for a fulltext-enabled definition or an explicit `codec = oakCodec`,
`Lucene46` for every other definition, `compressingCodec` under the
`oak.lucene.compressing-codec` system property, or any other name the
`META-INF/services` codec registration carries when `codec` names it. froe's
reader **accepts every registered name and reports it**; only the writers of
plans 0009 and 0010 restrict themselves to `oakCodec`. A name outside the
registered set is *reported, not refused* — Oak would fail on it at open, and
saying so is precisely what the check exists for.

### 8.5 `.si`, the per-segment descriptor

`codecs/lucene46/Lucene46SegmentInfoReader.java`. The file is
`<segment>.si`, and it **always sits beside the `.cfs`, never inside it**.
After the codec header — name `Lucene46SegmentInfo`
(`Lucene46SegmentInfoFormat.CODEC_NAME`), versions `VERSION_START` = 0 through
`VERSION_CURRENT` = 0:

| Field | Encoding | Notes |
| --- | --- | --- |
| Lucene version | `String` | the version that wrote the segment |
| document count | `Int` | **refused when negative** |
| compound flag | 1 byte | `SegmentInfo.YES` means the segment's files live in a `.cfs` |
| diagnostics | `StringStringMap` | |
| file set | `StringSet` | the segment's own files, by full name |

The reader then requires `getFilePointer() == length()` — **the file must be
consumed exactly**, with no trailing bytes. froe enforces the same, because a
`.si` with trailing bytes is one Lucene itself refuses.

### 8.6 The compound file

`store/CompoundFileDirectory.java`, `readEntries` at line 128. The pair is
`<segment>.cfs` (data) and `<segment>.cfe` (entries).

The `.cfs` opens with a codec header naming `CompoundFileWriterData`
(`CompoundFileWriter.DATA_CODEC`), version 0 only. The table of contents is in
the `.cfe`, which opens with a codec header naming
`CompoundFileWriterEntries` (`CompoundFileWriter.ENTRY_CODEC`), version 0
only, and then:

| Field | Encoding |
| --- | --- |
| entry count | `VInt` |
| *per entry* | |
| name | `String` |
| offset | `Long` |
| length | `Long` |

Three rules that a careless reader gets wrong.

**The name is segment-stripped.** The entries carry `.fdt`, `.tim`, `.fnm` —
**never** `_0.fdt`. `CompoundFileDirectory` looks up by
`IndexFileNames.stripSegmentName(name)` (`index/IndexFileNames.java` line
168), which cuts everything up to and including the segment prefix. So a
lookup must strip first, and this namespace must **never** be mixed with the
full names that `segments_N` and `.si` carry. froe's reader refuses a `.cfe`
whose entry name still carries a segment prefix: that file was not written by
Lucene's own writer, and accepting it would let a crafted directory address a
file by two names.

**The entries are in no specified order.** `readEntries` builds a map; the
offsets need not ascend. A reader that assumes order will read the wrong
bytes for an index that is perfectly valid.

**A duplicate name is corruption.** `readEntries` refuses on
`Duplicate cfs entry id=…`, and so does froe.

Every entry's `offset + length` is validated against the `.cfs` length before
any read, so an entry pointing past the end is a typed error naming the entry
and the two numbers rather than a panic or an out-of-bounds read.

### 8.7 What the structural check asserts

Between oak-run's level 1 (the blobs resolve, §5) and its level 2 (Lucene's
own `CheckIndex`), froe's structural check answers: *is this directory a
coherent set of Lucene files?*

* every file the segments name exists in the directory listing, and every
  listed file is named by the segments — where "named by the segments" is
  `SegmentCommitInfo.files()`, not `SegmentInfo.files()`: the `.si`'s own set
  **plus** the deletions file derived from the deletion generation (§8.8)
  **plus** the field-update generation files the commit file lists as string
  sets — and `segments.gen`, which no commit's aggregate file set ever names
  and which may legitimately be absent;
* every codec header validates;
* the deletion count of each segment is within its document count (the same
  bound `SegmentInfos.read` enforces);
* the **live document count** is the sum over segments of document count minus
  deletion count, which is how Oak's own document count over a directory
  computes it;
* the codec name is reported, and reported as unregistered when it is outside
  the set the image's `META-INF/services` registration carries.

### 8.8 The deletions file is derived, never listed

A segment with deletions does not name its `.del` anywhere in the commit
file's strings. The name is **computed** from the deletion generation, and a
reader that only collects the strings calls a real Oak index incoherent — as
froe's did, over a fixture whose `segments_2` names `_0` with a deletion
generation of 1 and whose directory holds `_0_1.del`.

`SegmentCommitInfo.files()` (Lucene 4.7.2, `lucene/core`,
`org/apache/lucene/index/SegmentCommitInfo.java`) unions three sets: the
`.si`'s own `files()`, whatever the codec's live-docs format contributes, and
the field-update generation files. The live-docs half is
`Lucene40LiveDocsFormat`:

```java
static final String DELETES_EXTENSION = "del";

@Override
public void files(SegmentCommitInfo info, Collection<String> files) throws IOException {
  if (info.hasDeletions()) {
    files.add(IndexFileNames.fileNameFromGeneration(info.info.name, DELETES_EXTENSION, info.getDelGen()));
  }
}
```

`hasDeletions()` is `delGen != -1`, and the name comes from
`IndexFileNames.fileNameFromGeneration` (`lucene/core`,
`org/apache/lucene/index/IndexFileNames.java`):

```java
public static String fileNameFromGeneration(String base, String ext, long gen) {
  if (gen == -1) {
    return null;
  } else if (gen == 0) {
    return segmentFileName(base, "", ext);
  } else {
    assert gen > 0;
    StringBuilder res = new StringBuilder(base.length() + 6 + ext.length())
        .append(base).append('_').append(Long.toString(gen, Character.MAX_RADIX));
    if (ext.length() > 0) {
      res.append('.').append(ext);
    }
    return res.toString();
  }
}
```

So, per segment:

| deletion generation | deletions file |
| --- | --- |
| `-1` | none |
| `0` | `<segment>.del` |
| `n > 0` | `<segment>_<n in base 36>.del` — `Character.MAX_RADIX` is 36, the same radix §8.3's commit-file generation uses |

A negative generation other than `-1` is malformed: `fileNameFromGeneration`
asserts `gen > 0` on that branch, and froe refuses it rather than computing a
name Lucene never writes.

---

## 9. Worked example, checked against the real Sling fixture

The fixture is the store `generate` produces: Apache Sling 14 with
`oak-segment-tar` 1.90.0, stopped cleanly, its segment store extracted.

```text
$ froe node <store> '/oak:index/lucene/:data'
property  dirListing <String[]> = ["_0_1.del","_1.cfs","_0.si","_0.cfe","_1.cfe","_1.si","_0.cfs","segments_2","segments.gen"]
child     _1.si
child     _0_1.del
child     _0.si
child     _0.cfe
child     _1.cfe
child     _0.cfs
child     segments_2
child     _1.cfs
child     segments.gen
```

Nine files, and the `dirListing` in neither sorted nor child order — §1.1 fact
4, observed. The definition carries no `saveDirectoryListing`, so the default
`true` applies and this listing is what Oak reads (§1.1 fact 1).

```text
$ froe node <store> '/oak:index/lucene/:data/_1.si'
property  blobSize         <Long>   = 1047552
property  uniqueKey        <String> = "443be1c792a673d4c26078ee12ba006d"
property  jcr:lastModified <Long>   = 1789367067705
property  jcr:data         <Binary> = {"binary_length":241}
```

Derived from the rules this document states:

* `blobSize = 1047552` is `IndexDefinition.DEFAULT_BLOB_SIZE`,
  `1024 * 1024 - 1024`, because the definition sets no `blobSize` and the
  clamp `Math.max(1024, …)` does not bind (§2).
* `uniqueKey` is 32 hexadecimal characters, which is
  `UNIQUE_KEY_SIZE = 16` bytes rendered by `convertBytesToHex` (§2).
* `jcr:data` is a **single `BINARY`**, so `OakIndexFile.getOakIndexFile`
  dispatches to the streaming form (§3), which Oak's own editor wrote because
  `oak.lucene.enableSingleBlobIndexFiles` defaults to true (§3.4).
* The blob is 241 bytes, so the file length Oak reports is
  `241 - 16 = 225` bytes (§3.1). The stored blob is the 225 bytes of the
  Lucene `.si` file followed by the 16 key bytes.
* `jcr:lastModified` is the wall-clock millisecond of the flush and is not
  reproducible (§2).

The definition itself shows the bookkeeping of §7:

```text
$ froe node <store> '/oak:index/lucene'
property  :version             <Long>     = 2
property  includePropertyTypes <String[]> = ["String","Binary"]
property  seed                 <Long>     = -1057390239000474647
property  type                 <String>   = "lucene"
property  async                <String>   = "async"
property  reindex              <Boolean>  = false
property  reindexCount         <Long>     = 1
child     :data
child     :status
child     :index-definition
child     indexRules
```

`:version` on the definition node itself, `seed` injected because the
definition is asynchronous (§7.2), and both `:status` and `:index-definition`
present. There is no `:suggest-data`: the definition does not enable
suggestions, and §7.4 says its absence is legal in any case.

## 10. AEM safety invariants

1. **A wrong file length makes Oak throw on open.** The length is derived, not
   stored (§3), so it is wrong exactly when the blob bytes, the `blobSize` or
   the `uniqueKey` disagree with each other. A writer that appended the key to
   only some chunks, or that wrote a `blobSize` different from the chunking it
   actually used, produces a directory Lucene cannot read — and Oak logs that
   as index corruption and reindexes, losing the work.
2. **An absent `uniqueKey` is read as "no trailing key", never as an error**
   (§3.3). Subtracting sixteen bytes anyway would truncate every file in a
   directory written without keys.
3. **The `blobSize` fallback is the reader's 32 KiB, not the definition's
   default** (§2). This is the single most likely place for a port to
   introduce a silent, content-dependent corruption, because it only shows on
   file nodes that lack the property.
4. **`dirListing` must agree with the children whenever `saveDirectoryListing`
   is on** — Oak believes the property (§1.1 fact 2). A writer that adds a
   file node without adding it to the listing has written a file Oak cannot
   see; one that removes a node without removing the name has written a
   listing Oak will fail to open. Under `saveDirectoryListing = false` the
   property must be left alone, not "repaired".
5. **Compare a `dirListing` as a set** (§1.1 fact 4). Its order is a hash
   set's iteration order and carries no meaning.
6. **`:suggest-data` may be absent** (§7.4), and so may `creationTimestamp` on
   a freshly reindexed or imported definition (§7.2). Neither is a defect.
7. **`indexedNodes` is not a document count** (§7.1). A check that compared it
   with Lucene's `numDocs` would report every healthy index as inconsistent.
8. **Never write `jcr:lastModified` or a `uniqueKey` froe did not derive from
   its own write.** Both are per-write values; copying them from a source node
   while writing different bytes breaks the blob-identity property the key
   exists to provide.
