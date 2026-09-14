# Index definitions, lanes and the reindex protocol

Behaviour-exact specification of everything froe must read from `/oak:index`
and `/:async`, of the bookkeeping Oak performs around a reindex, and of the
JSON form oak-run prints definitions in. Plans 0007, 0008 and 0010 all *write*
into the structures this document describes, so a statement here that is
merely plausible is a bug that reaches disk.

This is the prime directive applied to indexing: the Java is ground truth, this
document is derived, and every behavioural statement names the file and method
it was read from.

Java sources cited below are given relative to the module and package they live
in, with the prefix `<module>/src/main/java/org/apache/jackrabbit/oak/` elided:
`oak-core`, `plugins/index/IndexUpdate.java` is
`oak-core/src/main/java/org/apache/jackrabbit/oak/plugins/index/IndexUpdate.java`.
The revision read is the one [`README.md`](README.md) pins, Apache Jackrabbit
Oak commit `4984c4cf26a7ca58ae9ce12c63190b7f492bda78`.

**Precedence.** Where trunk at that commit and the consumer build this
repository verifies against — `oak-segment-tar` 1.90.0 inside the digest-pinned
Sling image — differ, **the consumer build's behaviour wins** and the
specification says so at the point of difference. froe is published for that
build; a trunk-only refinement froe reproduced would be a divergence from every
repository froe is actually pointed at. No such difference has been found in
the behaviour this document specifies; the rule is recorded so that the next
one is resolved the same way rather than argued about.

Builds on (does not repeat):

- [`index-property-storage.md`](index-property-storage.md) — what the
  `property`, unique, node-type, `reference` and `counter` indexes store.
- [`index-lucene-storage.md`](index-lucene-storage.md) — how a Lucene index
  directory is stored as repository content.
- [`node-layer.md`](node-layer.md), [`record-layer.md`](record-layer.md) — how
  nodes, properties and values are encoded in a segment store.

A note on reading this document: Oak's typed getters on a `NodeState` or a
`NodeBuilder` are **strict** — they return a value only when the stored type is
exactly the requested one — while `PropertyState.getValue(Type)` **converts**.
[`index-property-storage.md`](index-property-storage.md) §6 quotes the Java for
that and it is not repeated here; every read below is labelled with which kind
it is, because the asymmetries are observable.

---

## 1. Definition discovery and identity

### 1.1 What makes a node a definition

Three things, and the indexer tests all three:

```java
for (String name : definitions.getChildNodeNames()) {
    NodeBuilder definition = definitions.getChildNode(name);
    if (isIncluded(rootState.async, definition)) {
        String type = definition.getString(TYPE_PROPERTY_NAME);
        String primaryType = definition.getName(JcrConstants.JCR_PRIMARYTYPE);
        if (type == null) {
            // probably not an index def
            continue;
        }
        if (!IndexConstants.INDEX_DEFINITIONS_NODE_TYPE.equals(primaryType)) {
            … log sparsely …
            continue;
        }
```

(`oak-core`, `plugins/index/IndexUpdate.java`, `collectIndexEditors`.)

* the node is a child of a node named `oak:index`
  (`IndexConstants.INDEX_DEFINITIONS_NAME`);
* `jcr:primaryType` reads **strictly** as the `NAME` `oak:QueryIndexDefinition`
  (`INDEX_DEFINITIONS_NODE_TYPE`) — `definition.getName(...)`;
* `type` reads **strictly** as a single `STRING` —
  `definition.getString(...)`, which returns `null` for a `NAME`, for an array,
  and for a missing property.

**A definition whose `type` is a `String[]` is silently skipped by the
indexer**, because `getString` returns `null` and the `type == null` branch
takes it as "probably not an index def". The *node-type test* the same
definition faces elsewhere converts (`IndexUtils.isIndexNodeType`:
`ps.getValue(STRING).equals(INDEX_DEFINITIONS_NODE_TYPE)`), so the two
disagree. **froe follows the indexer** — it is the reader whose verdict decides
whether the index is maintained — and reports such a definition as an
`IndexWarning` naming the condition, never converting it into a definition it
then treats as live.

An invalid `jcr:primaryType` is logged only once every
`oak.indexer.indexJcrTypeInvalidLogLimiter` cycles (default 1000), so an
operator watching the log is not a reliable detector of one; that is a reason
`froe index list` reports it per definition.

### 1.2 Non-root definitions

A definition need not live under `/oak:index`: `oak:index` may appear anywhere
in the tree, and `ContentMirrorStoreStrategy`'s `pathPrefix` exists to address
those. Enumerating them requires a query, which requires the node-type index to
declare `oak:QueryIndexDefinition`; §9.1 gives the exact rule, because it is
the printer's path source and therefore froe's.

### 1.3 The properties froe models

| Property | Type Oak writes | Read by | Strictness |
| --- | --- | --- | --- |
| `type` | `STRING` | the indexer | strict |
| `async` | `STRING` or `STRINGS` | the lane resolver | converting to `STRINGS` |
| `reindex` | `BOOLEAN` | `shouldReindex` | converting to `BOOLEAN` |
| `reindexCount` | `LONG` | `incrementReIndexCount` | converting to `LONG` |
| `reindex-async` | `BOOLEAN` | `collectIndexEditors` | **strict** |
| `corrupt` | `DATE` | `collectIndexEditors`, `clearCorruptFlag` | converting to `DATE` |
| `supersedes` | `STRINGS` | `IndexDisabler` | converting to `STRINGS` |
| `tags` | `STRINGS` | the query planner | — |
| `selectionPolicy` | `STRING` | the query planner | — |
| `queryPaths` | `STRINGS` | the query planner | — |
| `useIfExists` | `STRING` | the query planner | — |
| `deprecated` | `BOOLEAN` | `PropertyIndexPlan` | strict |
| `entryCount`, `keyCount` | `LONG` | the counting strategies | converting to `LONG` |
| `retainNodeInReindex` (on a hidden child) | `BOOLEAN` | `hasAnyHiddenNodes`, `removeIndexState` | **strict** |
| `:disableIndexesOnNextCycle` | `BOOLEAN` | `IndexDisabler` | **strict** |
| `:originalType` | `STRING` | `PurgeOldIndexVersion` | converting |
| `:version` | `LONG` | the fulltext editor | — |

The property-family fields (`propertyNames`, `declaringNodeTypes`, `unique`,
`valuePattern`, the prefix lists, `resolution`, `seed`) are specified in
[`index-property-storage.md`](index-property-storage.md) §6, and Lucene's
(`compatVersion`, `blobSize`, `saveDirectoryListing`, `includePropertyTypes`,
`indexRules`, `aggregates`, `facets`, `suggestion`, `analyzers`, `tika`) in
[`index-lucene-storage.md`](index-lucene-storage.md).

`TYPE_DISABLED = "disabled"` is the type a superseded index is moved to (§5.5);
`:originalType` records what it was, and is written and read only by oak-run's
`purge-index-versions` (`oak-run-commons`,
`indexversion/PurgeOldIndexVersion.java`), which skips a `disabled` index that
carries no `:originalType` precisely because that one was disabled by hand.
`TYPE_UNKNOWN = "unknown"` is declared in `IndexConstants` and used nowhere in
the tree at this commit.

## 2. The `async` property and lane naming

### 2.1 Resolving the lane name

`oak-core`, `plugins/index/IndexUtils.java`, `getAsyncLaneName`:

```java
if (async != null) {
    Set<String> asyncNames = SetUtils.toSet(async.getValue(Type.STRINGS));
    asyncNames.remove(IndexConstants.INDEXING_MODE_NRT);      // "nrt"
    asyncNames.remove(IndexConstants.INDEXING_MODE_SYNC);     // "sync"
    checkArgument(!asyncNames.isEmpty(), "No valid async name found for "
            + "index [%s], definition %s", indexPath, idxState);
    checkArgument(asyncNames.size() == 1, "Multiple async names found for "
            + "index [%s], definition %s", indexPath, idxState);
    return asyncNames.stream().findAny().orElseThrow();
}
return null;
```

* **The lane name is the one value that is neither `sync` nor `nrt`.**
* **Zero candidates and several candidates are both refusals.** An `async`
  holding only `sync`, or only `nrt`, or both, is a definition Oak throws on
  the moment anything asks for its lane. So is one holding two real lane names.
  froe raises a typed error for both rather than picking one.
* **A definition without an `async` property is synchronous** — the method
  returns `null`.
* The read is `async.getValue(Type.STRINGS)`, which **converts**, so a single
  `STRING`, a `STRINGS` array and a `NAME` all resolve.

### 2.2 Which cycle selects which definition

`IndexUpdate.isIncluded(String asyncRef, NodeBuilder definition)`:

```java
if (definition.hasProperty(ASYNC_PROPERTY_NAME)) {
    Iterable<String> opt = definition.getProperty(ASYNC_PROPERTY_NAME).getValue(Type.STRINGS);
    if (asyncRef == null) {
        // sync index job, accept synonyms
        return IterableUtils.contains(opt, INDEXING_MODE_NRT) || IterableUtils.contains(opt, INDEXING_MODE_SYNC);
    } else {
        return IterableUtils.contains(opt, asyncRef);
    }
} else {
    return asyncRef == null;
}
```

* A **synchronous** cycle (`asyncRef == null`) selects every definition with no
  `async` property, plus those whose `async` contains `sync` or `nrt` — the
  synchronous half of a hybrid index.
* An **asynchronous** cycle for lane `L` selects every definition whose `async`
  contains `L`.

`isMatchingIndexMode` is a *coarser* test used only by `shouldReindex`
(§5.1): it compares presence alone, `definition.hasProperty(ASYNC_PROPERTY_NAME) == rootState.isAsync()`,
whatever the values are.

### 2.3 What names a lane

`oak-core`, `plugins/index/AsyncIndexUpdate.java`:

```java
public static boolean isAsyncLaneName(String asyncName){
    return IndexConstants.ASYNC_REINDEX_VALUE.equals(asyncName) || asyncName.endsWith("async");
}
```

That is Oak's whole test: **a name is a lane name exactly when it is
`async-reindex` or ends in `async`.** `async`, `fulltext-async`,
`offline-reindex-async` and `temp-async` all qualify; `sync` and `nrt` do not.
froe applies the same test rather than a list of known lanes.

## 3. `/:async` — the lane state

`AsyncIndexUpdate.ASYNC = ":async"`, a child of the root. Per lane `L` it
holds four properties, whose names come from three one-line helpers in the
same class:

| Property | Meaning | Helper |
| --- | --- | --- |
| `L` | the checkpoint the lane last indexed to | the lane name itself |
| `L-temp` | checkpoints pending release | `getTempCpName(name)` → `name + "-temp"` |
| `L-lease` | lease expiry, a `LONG` | `leasify(name)` → `name + "-lease"` |
| `L-LastIndexedTo` | a `DATE` | `lastIndexedTo(name)` → `name + "-LastIndexedTo"` |

The checkpoint value is read with `async.getString(name)` — the **strict**
`STRING` read — and `L-LastIndexedTo` is written as
`PropertyStates.createProperty(lastIndexedTo, afterTime, Type.DATE)`.

**The lease is present only while a run holds it.** `AsyncUpdateCallback.close`
removes it at the end of every run
(`async.removeProperty(leaseName)`), and `initLease` removes a stale one it
finds, so a lane that is not indexing right now has no `L-lease` at all. The
fixture's `/:async` accordingly carries exactly three properties:

```text
$ froe node <store> '/:async'
property  async-temp          <String[]> = ["f369be42-…","95521dd3-…"]
property  async               <String>   = "95521dd3-…"
property  async-LastIndexedTo <Date>     = "2026-09-14T06:24:27.689Z"
```

with the current checkpoint also still listed in `async-temp`, which is the
pending-release list rather than a set of superseded checkpoints.

### 3.1 What a dangling lane checkpoint causes

`AsyncIndexUpdate.runWhenPermitted`, the block that reads the previous
checkpoint:

```java
String beforeCheckpoint = async.getString(name);
if (beforeCheckpoint != null) {
    NodeState state = store.retrieve(beforeCheckpoint);
    if (state == null) {
        … re-read the root through a lease grab, retry once …
    }
    if (state == null) {
        log.warn("[{}] Failed to retrieve previously indexed checkpoint {}; re-running the initial index update",
                name, beforeCheckpoint);
        beforeCheckpoint = null;
        callback.setCheckpoint(beforeCheckpoint);
        before = MISSING_NODE;
    } …
```

So Oak **does not fail**: it logs that warning and re-runs "the initial index
update" with `before = MISSING_NODE`.

**That re-run is not a reindex.** `shouldReindex` (§5.1) tests
`!before.getChildNode("oak:index").hasChildNode(name) && !hasAnyHiddenNodes(definition)`,
and the definition still has its `:index` or `:data`, so `hasAnyHiddenNodes` is
true and the second conjunct is false. The hidden state on disk is *retained*
and the cycle is an incremental diff from the missing state over it.

**The one exception the Lucene plans rest on.** The fulltext editor enters
reindex mode on its own test, which looks only at the before state:

```java
public void enter(NodeState before, NodeState after) {
    if (EmptyNodeState.MISSING_NODE == before && parent == null) {
        context.enableReindexMode();
    }
```

(`oak-search`, `plugins/index/search/spi/editor/FulltextIndexEditor.java`.) And
in reindex mode the writer **adds** rather than updates:

```java
if (reindex) {
    if (containsOnlyPath && isPropertyRegexMatchingEnabled) { return; }
    getWriter().addDocument(doc);
} else {
    …
    getWriter().updateDocument(newPathTerm(path), doc);
}
```

(`oak-lucene`, `plugins/index/lucene/writer/DefaultIndexWriter.java`,
`updateDocument`.) `updateDocument(term, doc)` deletes by the path term first;
`addDocument(doc)` does not. So a dangling lane checkpoint makes Oak **append
every document again to the retained `:data`, doubling the index** — no error,
no reindex flag, and the only visible symptom is that the directory grows and
queries return each hit twice. This is the single most consequential
consequence of a dangling lane checkpoint, and it is why froe reports one
(`froe digest`'s dangling-checkpoint fact, and `froe index list`'s per-index
warning) rather than treating `/:async` as opaque.

## 4. The path filter's construction

`oak-store-spi`, `spi/filter/PathFilter.java`. No other document in this
directory covers it, and both the budget derivation of `froe index check` and
plan 0007's reindex need the *unified* include set rather than the stored one.

`PathFilter.from(NodeBuilder defn)` returns the match-everything filter when
neither `includedPaths` nor `excludedPaths` is present. Otherwise both are read
with

```java
public static Iterable<String> getStrings(PropertyState ps, Set<String> defaultValues) {
    if (ps != null && (ps.getType() == Type.STRING || ps.getType() == Type.STRINGS)) {
        return ps.getValue(Type.STRINGS);
    }
    return defaultValues;
}
```

— **`STRING` or `STRINGS` only**, a single `STRING` reading as a one-element
list, anything else falling back to the defaults, which are `{"/"}` for
includes and `{}` for excludes.

The constructor then applies three steps **in this order**:

```java
public PathFilter(Iterable<String> includes, Iterable<String> excludes) {
    checkPathsAreAbsolute(includes, "included");
    checkPathsAreAbsolute(excludes, "excluded");
    Set<String> includeCopy = SetUtils.toSet(includes);
    Set<String> excludeCopy = SetUtils.toSet(excludes);
    PathUtils.unifyInExcludes(includeCopy, excludeCopy);
    Validate.checkState(!includeCopy.isEmpty(), "No valid include provided. Includes %s, Excludes %s", includes, excludes);
    …
}
```

1. **Both sets are checked for absoluteness first**, and a relative path in
   either throws `IllegalStateException("Invalid path in <included|excluded> paths list: … Paths must be absolute.")`.
2. **Unification** (`oak-commons`, `commons/PathUtils.java`,
   `unifyInExcludes`):

   ```java
   Set<String> retain = new HashSet<>();
   Set<String> includesRemoved = new HashSet<>();
   for (String include : includePaths) {
       for (String exclude : excludedPaths) {
           if (exclude.equals(include) || isAncestor(exclude, include)) {
               includesRemoved.add(include);
           } else if (isAncestor(include, exclude)) {
               retain.add(exclude);
           }
       }
       for (String include2 : includePaths) {
           if (isAncestor(include, include2)) { includesRemoved.add(include2); }
       }
   }
   includePaths.removeAll(includesRemoved);
   excludedPaths.retainAll(retain);
   ```

   — an include equal to or under an exclude is dropped; an include under
   another include is dropped as redundant; and, **the rule an implementer will
   not guess, every exclude that lies under no include is dropped entirely**.
   Note "under" means *strictly* under: an exclude equal to an include takes
   the first branch and is not retained. Note also that removals are collected
   and applied only after the loops, so an include that is itself about to be
   dropped still contributes excludes to `retain` — a detail with no
   observable effect once the empty-include refusal of step 3 fires, but one a
   reimplementation must copy to stay bit-identical on the intermediate sets.
3. **An include set left empty is a refusal**, `IllegalStateException`.

So **empty include and exclude sets together include everything** (step 1 never
runs, `from` returns the match-everything filter), and a filter whose every
include is excluded is a definition Oak cannot construct a filter for at all.
Both refusals are typed errors in froe.

`filter(path)`'s three verdicts are specified in
[`index-property-storage.md`](index-property-storage.md) §6.2.

## 5. The reindex protocol

Plans 0007 and 0010 must leave a definition in exactly the state Oak's own
editors leave it in. This section is that state.

### 5.1 What makes a cycle reindex a definition

`oak-core`, `plugins/index/IndexUpdate.java`, `shouldReindex`, in order:

```java
PropertyState type = definition.getProperty(TYPE_PROPERTY_NAME);

// Do not attempt reindex of indexes with no type or disabled
if (type == null || TYPE_DISABLED.equals(type.getValue(Type.STRING))) {
    return false;
}

if (!TYPE_ELASTICSEARCH.equals(type.getValue(Type.STRING)) && !isMatchingIndexMode(definition)) {
    return false;
}

PropertyState ps = definition.getProperty(REINDEX_PROPERTY_NAME);
if (ps != null && ps.getValue(BOOLEAN)) {
    return !rootState.ignoreReindexFlags;
}

boolean result = !before.getChildNode(INDEX_DEFINITIONS_NAME).hasChildNode(name) && !hasAnyHiddenNodes(definition);
```

**Two gates first:**

1. a missing `type`, or one that **converts** to the `STRING` `disabled`. Note
   this read converts where §1.1's discovery read is strict: a `NAME`-typed
   `disabled` is skipped here but never got this far, because discovery already
   dropped it.
2. an index mode that does not match the cycle — `isMatchingIndexMode`,
   presence of `async` against the cycle's own asynchrony — **unless the type
   is `elasticsearch`**.

**Then either of two triggers:**

* **the `reindex` flag**, read **converting** to `BOOLEAN`, so a `STRING`
  `"true"` flags a definition. The trigger is suppressed by
  `rootState.ignoreReindexFlags`, which starts at the system property
  `oak.indexUpdate.ignoreReindexFlags` (`Boolean.getBoolean`, so **false by
  default**) and can also be set per editor provider through
  `IndexUpdate.setIgnoreReindexFlags`;
* **a definition absent from the before state's `oak:index`** with no hidden
  child other than ones flagged `retainNodeInReindex`. **The ignore switch does
  not suppress this path** — it returns before the switch is consulted. The
  code's own warning is worth repeating: "If there is _any_ hidden node, then
  it is assumed that no reindex is needed. Even if the hidden node is
  completely unrelated and doesn't contain index data (for example the node
  `:status`)."

`hasAnyHiddenNodes` reads the retain flag **strictly**:

```java
for (String name : builder.getChildNodeNames()) {
    if (NodeStateUtils.isHidden(name)) {
        NodeBuilder childNode = builder.getChildNode(name);
        if (childNode.getBoolean(IndexConstants.REINDEX_RETAIN)) { continue; }
        return true;
    }
}
```

**So the flag froe writes must be a stored `BOOLEAN`.** The same is true of
`reindex-async` and `:disableIndexesOnNextCycle` (§5.5), both read with
`getBoolean`. Only `reindex` itself is read converting.

An `elasticsearch` definition that reaches the second trigger is **not**
reindexed: Oak logs "Found a new elastic index node … Please set the reindex
flag = true" and returns false, because elastic data is remote and leaves no
hidden child to detect.

### 5.2 What the reindex then does

```java
} else if (shouldReindex) {
    if (definition.getBoolean(REINDEX_ASYNC_PROPERTY_NAME)
            && definition.getString(ASYNC_PROPERTY_NAME) == null) {
        definition.setProperty(ASYNC_PROPERTY_NAME, ASYNC_REINDEX_VALUE);   // "async-reindex"
    } else {
        definition.setProperty(REINDEX_PROPERTY_NAME, false);
        incrementReIndexCount(definition);
        removeIndexState(definition);
        clearCorruptFlag(definition, indexPath);
        reindex.put(concat(getPath(), INDEX_DEFINITIONS_NAME, name), editor);
    }
    rootState.indexDisabler.markDisableFlagIfRequired(indexPath, definition);
}
```

In order, for the ordinary branch:

1. **`reindex` is set to the `BOOLEAN` `false`**;
2. **`reindexCount` is incremented** — `incrementReIndexCount` reads the old
   value **converting** to `LONG`, defaulting to `0`, and writes `count + 1`;
3. **every hidden child is removed except those flagged
   `retainNodeInReindex`** (`removeIndexState`, the flag read strictly), a
   `ReadOnlyBuilder` child being preserved with a debug log instead;
4. **`corrupt` is removed** if present (`clearCorruptFlag`);
5. the editor is registered for the reindex composite, which then runs
   `EditorDiff.process(VisibleEditor.wrap(…), MISSING_NODE, after)` from
   `IndexUpdate.enter` — that is, the reindex is a diff from the missing state
   over the whole repository, wrapped in the visible-editor filter.

**The `reindex-async` switch.** When `reindex-async` is a stored `BOOLEAN`
`true` *and* the definition has no `async` (strict `getString`), the cycle does
none of the above: it only writes `async = "async-reindex"`, moving the
definition onto the reindex lane so that the rebuild happens asynchronously.
The flags are cleared by the later cycle on that lane.

`markDisableFlagIfRequired` runs on **both** branches (§5.5).

### 5.3 `corrupt` short-circuits everything

Before an editor is even requested:

```java
if (definition.hasProperty(IndexConstants.CORRUPT_PROPERTY_NAME) && !shouldReindex) {
    String corruptSince = definition.getProperty(IndexConstants.CORRUPT_PROPERTY_NAME).getValue(Type.DATE);
    rootState.corruptIndexHandler.skippingCorruptIndex(rootState.async, indexPath, ISO8601.parse(corruptSince));
    continue;
}
```

A `corrupt` definition is skipped entirely unless this very cycle is
reindexing it — which is the only thing that clears the flag (§5.2 step 4).

### 5.4 A type with no registered editor provider

`IndexUpdate.MissingIndexProviderStrategy.onMissingIndex`:

```java
private final Set<String> ignore = Set.of("disabled", "ordered");

public void onMissingIndex(String type, NodeBuilder definition, String indexPath) throws CommitFailedException {
    if (isDisabled(type)) { return; }
    PropertyState ps = definition.getProperty(REINDEX_PROPERTY_NAME);
    if (ps != null && ps.getValue(BOOLEAN)) { return; }         // already flagged
    if (failOnMissingIndexProvider) {
        throw new CommitFailedException("IndexUpdate", 1, "Missing index provider detected for type [" + type + "] on index [" + indexPath + "]");
    } else {
        log.warn("Missing index provider of type [{}], requesting reindex on [{}]", type, indexPath);
        definition.setProperty(REINDEX_PROPERTY_NAME, true);
    }
}
```

* `disabled` and `ordered` are ignored outright — the deprecated `ordered`
  type is expected to have no provider.
* Otherwise, **by default Oak sets `reindex = true` and logs a warning**, so
  that the index is rebuilt whenever a provider appears. Under
  `oak.indexUpdate.failOnMissingIndexProvider` it fails the commit instead.
* The strategy is **not** reached when the definition has an `async` property
  and the cycle is synchronous: that case only logs a rate-limited warning
  about an `nrt`/`sync` index whose data should be trusted only after an
  asynchronous cycle.

**This is why froe must never write a definition with a type it invented.** A
running Oak would see a missing provider and flag the index for reindex, which
on a large repository is hours of unplanned work.

### 5.5 `supersedes` and the disabler

`oak-core`, `plugins/index/upgrade/IndexDisabler.java`. Two phases, one cycle
apart.

**Raising the flag** — `markDisableFlagIfRequired`, called on the reindex path
(§5.2) and by the importer's data step (§7.3). It sets the **hidden**
`:disableIndexesOnNextCycle = true` when `isAnyIndexToBeDisabled` finds, among
the `supersedes` values (read **converting** to `STRINGS`), either

* a plain index path whose node exists and whose `type` **does not read
  strictly as the `STRING` `disabled`** —
  `!TYPE_DISABLED.equals(idxSate.getString(TYPE_PROPERTY_NAME))`, so a
  `NAME`-typed or array-typed `type` **counts as active** and raises the flag;
  or
* a `/path/@type` entry (last segment starting with `@`) whose named node type
  is still among the target index's `declaringNodeTypes`, read **converting**
  to `NAMES`.

**Acting on it** — `disableOldIndexes`, called on the *non*-reindex branch of
`collectIndexEditors`, and only when the definition is synchronous or the cycle
is asynchronous. It returns immediately unless `supersedes` is present, the
flag reads strictly `true`, **and the base state's flag also reads true** —
the last test skipping the cycle in which the reindex just raised it. Then, per
entry:

* a plain path: `type` is set to the `STRING` `disabled`;
* a `/path/@type` entry: the named type is removed from that index's
  `declaringNodeTypes`, rewritten as `NAMES`.

**The flag is removed only when at least one entry was acted on**
(`if (!disabledIndexes.isEmpty()) { idxBuilder.removeProperty(DISABLE_INDEXES_ON_NEXT_CYCLE); }`).

The disabler's only strict read is that `type` test; everything else it reads
converts. That asymmetry is load-bearing: it means a superseded index whose
`type` was stored as a `NAME` is disabled over and over, because the write sets
a `STRING` the next `isAnyIndexToBeDisabled` then recognizes — and the *first*
pass already succeeded, so the flag was removed. froe reproduces the reads; it
never writes `supersedes`.

### 5.6 Every index update is wrapped in the visible-editor filter

All five sites, so that no path escapes the rule:
`IndexUpdate.java` (the reindex composite), `IndexUpdateProvider.java` (the
synchronous incremental editors), `AsyncIndexUpdate.java` (the asynchronous
lane), `importer/IndexImporter.java` (the catch-up diff) and `oak-run-commons`
`index/OutOfBandIndexerBase.java` (the out-of-band build).
[`index-property-storage.md`](index-property-storage.md) §6.3 quotes each call.

## 6. The status and stored-definition nodes

Only the **fulltext editor family** writes these; the property, reference and
counter editors write none of them
([`index-lucene-storage.md`](index-lucene-storage.md) §7).

### 6.1 `:status`

`IndexDefinition.STATUS_NODE = ":status"` (`oak-search`,
`plugins/index/search/IndexDefinition.java`).

| Property | Written by | Value |
| --- | --- | --- |
| `uid` | `FulltextIndexEditorContext.configureUniqueId` | a `STRING` holding `Clock.SIMPLE.getTimeIncreasing()` — time-increasing decimal epoch milliseconds — written only when absent, and read back with the **strict** `status.getString(PROP_UID)` |
| `lastUpdated` | `closeWriter` | a `DATE`: the `indexingCheckpointTime` commit attribute when the cycle has one, `ISO8601.format(currentTime)` otherwise |
| `indexedNodes` | `closeWriter` | a `LONG`: a **per-cycle counter**, reset to 0 in the editor context's constructor, **not** a document count |
| `reindexCompletionTimestamp` | `closeWriter`, only `if (reindex)` | a `DATE` |

All four are written only `if (indexUpdated)` — a cycle that changed nothing
writes no `:status` at all.

**`reindexCompletionTimestamp` is *removed* by `ReindexOperations.apply`**
when the status node exists (§6.2), and the whole `:status` node is removed by
Oak's Lucene importer ([`index-lucene-storage.md`](index-lucene-storage.md)
§6.2).

### 6.2 `:index-definition`, `:version` and `seed`

`IndexDefinition.INDEX_DEFINITION_NODE = ":index-definition"`,
`INDEX_VERSION = ":version"`, `CREATION_TIMESTAMP = "creationTimestamp"`,
`FulltextIndexConstants.PROP_RANDOM_SEED = "seed"`,
`PROP_REFRESH_DEFN = "refresh"`.

`oak-search`, `plugins/index/search/ReindexOperations.java`, `apply`:

```java
IndexFormatVersion version = IndexDefinition.determineVersionForFreshIndex(definitionBuilder);
definitionBuilder.setProperty(IndexDefinition.INDEX_VERSION, version.getVersion());
definitionBuilder.removeProperty(IndexImporter.INDEX_IMPORT_STATE_KEY);

NodeState defnState = useStateFromBuilder ? definitionBuilder.getNodeState() : definitionBuilder.getBaseState();
if (storedIndexDefinitionEnabled) {
    definitionBuilder.setChildNode(INDEX_DEFINITION_NODE, NodeStateCloner.cloneVisibleState(defnState));
    definitionBuilder.setChildNode(INDEX_DEFINITION_NODE, NodeStateCloner.cloneVisibleState(defnState));
    if (definitionBuilder.getChildNode(STATUS_NODE).exists()) {
        definitionBuilder.getChildNode(STATUS_NODE).removeProperty(REINDEX_COMPLETION_TIMESTAMP);
    }
}
String uid = configureUniqueId(definitionBuilder);
```

(The duplicated `setChildNode` line is in the source as quoted; it is
idempotent.)

* **`:version` is a hidden property on the definition node itself**, written on
  every path through `apply` — that is, on every Lucene reindex and on every
  import.
* **The clone is of the builder's *base* state on a reindex**
  (`useStateFromBuilder = false`, which is what `enableReindexMode` passes
  unless the definition was rewritten in the same commit) **and of the updated
  state on an import** (`apply(true)`, §7.3). On a reindex the clone therefore
  carries `reindex = true` and the *old* `reindexCount`, because the base state
  predates §5.2's writes.
* `cloneVisibleState` (`oak-search`, `plugins/index/search/util/NodeStateCloner.java`)
  **drops hidden child nodes and keeps hidden properties** — its
  `ApplyVisibleDiff` overrides `childNodeAdded` alone.
* **`creationTimestamp` is not written here.** It is written only in
  `FulltextIndexEditorContext.createIndexDefinition`, on the branch that
  consumes `refresh` and on the branch that creates a clone that did not exist.
  So **after a Lucene reindex or an import the property is absent** until a
  later refresh writes it — the inventory and the importer both expect that.
* `seed` is injected, as `UUID.randomUUID().getMostSignificantBits()`, for an
  **asynchronous** definition with no `seed` yet; on the "neither reindex nor
  refresh" branch the clone's `seed` is corrected to match the definition's.

### 6.3 What makes a Lucene definition hybrid

Plans 0008 and 0010 both refuse a hybrid definition, citing this paragraph. A
Lucene definition is hybrid when either

* a property definition under an index rule carries `sync` **or** `unique` —
  `oak-search`, `plugins/index/search/PropertyDefinition.java`:
  `this.unique = getOptionalValueIfIndexed(defn, PROP_UNIQUE, false);`
  `this.sync = unique || getOptionalValueIfIndexed(defn, PROP_SYNC, false);` — or
* a `nodeTypeIndex` rule carries `sync` — `IndexDefinition.IndexingRule`,
  `if (nodeTypeIndex) { boolean sync = getOptionalValue(config, PROP_SYNC, false); … }`.

The synchronous half then lives in the hidden child
`IndexDefinition.PROPERTY_INDEX = ":property-index"`, and **it is Oak's
*incremental synchronous* editor that maintains it, never the reindex** — the
reindex declines that branch outright and returns no editor for it. The child
is created and flagged `retainNodeInReindex` on first use.

**That is why the retain flag exists**: the asynchronous reindex which replaces
`:data` must leave the synchronous half standing. It is also **the only child
Oak ever flags**, which is the corollary plan 0008 rests on when it says the
retain rule never fires on an import.

### 6.4 The drift rule

`oak-lucene`, `plugins/index/lucene/LuceneIndexInfoProvider.java`:

```java
private static void computeIndexDefinitionChange(NodeState idxState, LuceneIndexInfo info) {
    NodeState storedDefn = idxState.getChildNode(INDEX_DEFINITION_NODE);
    if (storedDefn.exists()) {
        NodeState currentDefn = NodeStateCloner.cloneVisibleState(idxState);
        if (!FilteringEqualsDiff.equals(storedDefn, currentDefn)){
            info.indexDefinitionChanged = true;
            info.indexDiff = JsopDiff.diffToJsop(storedDefn, currentDefn);
        }
    }
}

static class FilteringEqualsDiff extends EqualsDiff {
    private static final Set<String> IGNORED_PROP_NAMES = Set.of(
            IndexConstants.REINDEX_COUNT, IndexConstants.REINDEX_PROPERTY_NAME);
    public static boolean equals(NodeState before, NodeState after) {
        return before.exists() == after.exists()
                && after.compareAgainstBaseState(before, new FilteringEqualsDiff());
    }
    @Override public boolean propertyChanged(PropertyState before, PropertyState after) { return ignoredProp(before.getName()); }
    @Override public boolean propertyAdded(PropertyState after) { return ignoredProp(after.getName()) || super.propertyAdded(after); }
    @Override public boolean propertyDeleted(PropertyState before) { return ignoredProp(before.getName()) || super.propertyDeleted(before); }
    private boolean ignoredProp(String name) { return IGNORED_PROP_NAMES.contains(name) || NodeStateUtils.isHidden(name); }
}
```

with `EqualsDiff` (`oak-store-spi`, `spi/state/EqualsDiff.java`) supplying
`childNodeAdded → false`, `childNodeDeleted → false` and
`childNodeChanged → after.compareAgainstBaseState(before, this)`.

Stated whole, because task 0605 builds from this paragraph:

* **Only the current definition is cloned**, and the clone keeps hidden
  properties while dropping hidden child nodes. The **stored node is compared
  as it stands** — which is equivalent to cloning it too, since every
  Oak-written `:index-definition` is itself such a clone. froe clones both
  sides and says so at the site.
* **Ignored property names**: `reindex`, `reindexCount`, and **every hidden
  property name**.
* **A visible child added or removed is a difference.** A visible child present
  on both sides is compared **by the same filtered rule, recursively**, so the
  ignored names apply **at every depth**, not only on the definition node — a
  child differing only in a hidden property such as `:childOrder` is no
  difference.
* The diff Oak reports is `JsopDiff.diffToJsop(storedDefn, currentDefn)`.
  froe renders its own sorted list of changed paths instead, a departure
  recorded at the site: the JSOP text is a debugging aid, not a contract, and
  a sorted path list is what a `froe index list` row can usefully show.

## 7. The out-of-band import protocol

The reference for plan 0008. `oak-core`,
`plugins/index/importer/IndexImporter.java` unless another file is named.

### 7.1 The directory the importer is handed

`plugins/index/importer/IndexerInfo.java`:
`indexer-info.properties` at the **root** directory, carrying `checkpoint`;
each direct subdirectory carrying an `index-details.txt` with an `indexPath`
becomes one index. Both file formats are specified in
[`index-lucene-storage.md`](index-lucene-storage.md) §6.

### 7.2 `index-definitions.json` is mandatory

`plugins/index/importer/IndexDefinitionUpdater.java`,
`INDEX_DEFINITIONS_JSON = "index-definitions.json"`. The importer's constructor
checks the file exists (`checkArgument(file.exists() && file.canRead(), "File [%s] cannot be read", file)`).

Applying it **replaces the whole definition node** with the file's state:

```java
NodeState newDefinition = indexNodeStates.get(indexPath);
newDefinition = addOrModifyJcrUUID(newDefinition);
…
Tree t = TreeFactory.createTree(indexBuilderParent);
t.addChild(indexNodeName);                       // keeps :childOrder correct
indexBuilderParent.setChildNode(indexNodeName, newDefinition);
```

`addOrModifyJcrUUID` walks the state and **replaces every `jcr:uuid` with a
fresh one**, and adds one to every `nt:resource` child that lacks it, so a
definition copied from another repository cannot collide. The parent path must
already exist (`Validate.checkState(parent.exists(), …)`).

The oak-run command `index --index-definitions-file` is only a wrapper around
this updater.

### 7.3 The four steps

```java
runWithRetry(RETRIES, IndexImportState.SWITCH_LANE, this::switchLanes);
runWithRetry(RETRIES, IndexImportState.IMPORT_INDEX_DATA, this::importIndexData);
runWithRetry(RETRIES, IndexImportState.BRING_INDEX_UPTODATE, this::bringIndexUpToDate);
runWithRetry(RETRIES, IndexImportState.RELEASE_CHECKPOINT, this::releaseCheckpoint);
```

and each step stamps the definition's `indexImportState` property
(`INDEX_IMPORT_STATE_KEY = "indexImportState"`) so an interrupted import is
idempotent on retry. The property is removed on success, and by
`ReindexOperations.apply` (§6.2).

* **`switchLanes`** moves each existing index onto `temp-<lane>` through
  `AsyncLaneSwitcher.switchLane`.
* **`importIndexData`** applies `index-definitions.json` per index, copies the
  lane properties back onto the new definition for an existing index,
  **increments `reindexCount` again**, hands the directory to the type's
  importer, and raises the disable flag (§5.5).
* **`bringIndexUpToDate`** diffs the indexed state against the lane's current
  checkpoint through an `IndexUpdate` on the temp lane, then reverts the lane.
* **`releaseCheckpoint`** releases the checkpoint `indexer-info.properties`
  names, **unless `oak.index.importer.preserveCheckpoint` is set**
  (`OAK_INDEX_IMPORTER_PRESERVE_CHECKPOINT = "oak.index.importer.preserveCheckpoint"`,
  `Boolean.getBoolean`).

### 7.4 The lane switch

`plugins/index/importer/AsyncLaneSwitcher.java`:

* `ASYNC_PREVIOUS = "async-previous"` holds the old `async` value, cloned with
  its original type (`STRING` or `STRINGS`), or the literal
  `ASYNC_PREVIOUS_NONE = "none"` when the index was synchronous.
* `getTempLaneName(L)` is `"temp-" + L` (`TEMP_LANE_PREFIX = "temp-"`).
* `revertSwitch` restores `async` (or removes it for `none`), removes
  `async-previous`, and **sets `refresh = true`** — "otherwise the lane changes
  won't reflect in the storedIndexDefinition".
* `switchLane` is idempotent: with `async-previous` present and `async` already
  equal to the target lane it returns; with `async-previous` present but `async`
  different it treats the property as stale, discards it and switches anyway.

**Two facts plan 0008 rests on.** `bringIndexUpToDate` skips the `sync` lane
outright —

```java
for (String laneName : asyncLaneToIndexMapping.keySet()) {
    if (ASYNC_LANE_SYNC.equals(laneName)) {
        continue; //TODO Handle sync indexes
    }
    bringAsyncIndexUpToDate(laneName, asyncLaneToIndexMapping.get(laneName));
}
```

— and the lane revert on the success path is reached **only** from
`bringAsyncIndexUpToDate` (the failure handler reverts every lane it switched).
So **oak-run's own importer leaves a synchronous Lucene definition sitting on
`async = temp-sync`**, with `async-previous = none`. There is no Oak reference
behaviour for importing a synchronous Lucene definition, which is why plan 0008
refuses it rather than inventing one.

The out-of-band reindex lane name is `offline-reindex-async`
(`oak-run-commons`, `index/IndexerSupport.java`,
`private static final String REINDEX_LANE = "offline-reindex-async"`).

### 7.5 What an oak-run out-of-band build prints into `index-definitions.json`

`oak-run-commons`, `index/IndexerSupport.java`:

```java
public void postIndexWork(NodeStore copyOnWriteStore) throws CommitFailedException, IOException {
    switchIndexLanesBack(copyOnWriteStore);
    dumpIndexDefinitions(copyOnWriteStore);
}

protected void dumpIndexDefinitions(NodeStore nodeStore) throws IOException {
    IndexDefinitionPrinter printer = new IndexDefinitionPrinter(nodeStore, indexHelper.getIndexPathService());
    printer.setFilter("{\"properties\":[\"*\", \"-:childOrder\"],\"nodes\":[\"*\", \"-:index-definition\", \"-:data\", \"-:suggest-data\"]}");
    …
}
```

It dumps the lane-switched **copy**, **after** that copy's reindex and **after**
the lanes are switched back. Five consequences the importer's consumer must
expect:

1. `reindexCount` is **one above** the store's, because the copy was reindexed
   (§5.2). The importer's data step then increments it once more, so an
   oak-run import ends **two above** the original store.
2. `refresh = true`, from `revertSwitch` (§7.4).
3. A `seed` when the run created one (§6.2).
4. **The copy's `:status` is in the file.** The dump filter excludes only
   `:index-definition`, `:data` and `:suggest-data`; `:status` and every hidden
   property are kept.
5. **`corrupt` and `indexImportState` are absent whenever the store has them**,
   because the copy's reindex cleared the first (§5.2 step 4) and
   `ReindexOperations.apply` removed the second (§6.2).

And two more:

* **The file's key set is `--index-paths` when oak-run was given them**
  (`IndexHelper.getIndexPathService` returns `() -> indexPaths` in that case)
  **and the index path service's enumeration otherwise**. Since that
  enumeration refuses outright when `/oak:index/nodetype` is absent or its
  `type` does not read strictly as the `STRING` `property` (§9.1), **an
  oak-run dump over a store whose nodetype index is disabled produces no file
  at all** — which is the case plan 0008's import meets, rather than a
  malformed file.
* The order of the keys is the printer's order (§9), never a sort.

## 8. The definition printer's JSON

This is the format `froe index definitions` reproduces byte for byte, and the
file `oak-run index --index-definitions-file` and §7.2's updater consume.

`oak-core`, `plugins/index/inventory/IndexDefinitionPrinter.java`:

```java
private String filter = "{\"properties\":[\"*\", \"-:childOrder\"],\"nodes\":[\"*\", \"-:*\"]}";

public void print(PrintWriter printWriter, Format format, boolean isZip) {
    if (format == Format.JSON) {
        NodeState root = nodeStore.getRoot();
        JsopBuilder json = new JsopBuilder();
        json.object();
        for (String indexPath : indexPathService.getIndexPaths()) {
            json.key(indexPath);
            NodeState idxState = NodeStateUtils.getNode(root, indexPath);
            createSerializer(json).serialize(idxState);
        }
        json.endObject();
        printWriter.print(JsopBuilder.prettyPrint(json.toString()));
    }
}
```

One JSON object keyed by index path, each value the definition node serialized
under the filter, the whole thing pretty-printed.

### 8.1 The filter

`oak-store-spi`, `json/JsonSerializer.java`, inner class `JsonFilter`. A
pattern starting with `-` is an exclude, `\-` escapes a leading hyphen into an
include, and a pattern is globbed by quoting everything but `*`, which becomes
`.*`. `include(name, includes, excludes)` admits a name when **some include
matches and no exclude matches**.

The printer's default filter is therefore:

* **properties**: every property except `:childOrder` — so hidden properties
  such as `:version` and `:originalType` **are included**. A renderer that
  dropped hidden properties would fail a byte comparison against Oak on the
  fixture's `lucene` definition, which carries `:version`.
* **nodes**: every child except those matching `:*` — so **every hidden child
  is excluded**, recursively.

The out-of-band build's variant (§7.5) replaces the node excludes with
`-:index-definition`, `-:data`, `-:suggest-data`, which keeps `:status`.

### 8.2 Child order

```java
private Iterable<? extends ChildNodeEntry> getChildNodeEntries(NodeState node, String basePath) {
    PropertyState order = node.getProperty(":childOrder");
    if (order != null) {
        List<String> names = ListUtils.toList(order.getValue(NAMES));
        List<ChildNodeEntry> entries = new ArrayList<>(names.size());
        for (String name : names) {
            NodeState childNode = node.getChildNode(name);
            if (childNode.exists()) { entries.add(new MemoryChildNodeEntry(name, childNode)); }
        }
        return entries;
    }
    return node.getChildNodeEntries();
}
```

**When `:childOrder` exists, it is the list of children to serialize** — a
child not named in it is not rendered at all, even if it exists — and when it
does not, the children come in stored order. The `:childOrder` read is
`getValue(Type.NAMES)`, which converts. Note that the property itself is
excluded by the filter, so it steers the output without appearing in it.

### 8.3 Values

`JsonSerializer.serialize(PropertyState)`:

```java
Type<?> type = property.getType();
if (!type.isArray()) {
    serialize(property, type, 0);
} else {
    Type<?> base = type.getBaseType();
    int count = property.count();
    if (base == STRING || count > 0) {
        json.array();
        for (int i = 0; i < count; i++) { serialize(property, base, i); }
        json.endArray();
    } else {
        json.value(TypeCodes.EMPTY_ARRAY + PropertyType.nameFromValue(type.tag()));
    }
}
```

* **An empty `STRING[]` renders as `[]`**; an empty array of **any other type**
  renders as the single string `[0]:<TypeName>` — `TypeCodes.EMPTY_ARRAY = "[0]:"`
  and `PropertyType.nameFromValue`, so an empty `Name[]` is `"[0]:Name"`.
* A non-empty array renders as a JSON array of the per-value renderings below.

`JsonSerializer.serialize(PropertyState, Type, int)`:

```java
if (type == BOOLEAN) {
    json.value(property.getValue(BOOLEAN, index));
} else if (type == LONG) {
    json.value(property.getValue(LONG, index));
} else if (type == DOUBLE) {
    Double value = property.getValue(DOUBLE, index);
    if (value.isNaN() || value.isInfinite()) {
        json.value(TypeCodes.encode(type.tag(), value.toString()));
    } else {
        json.encodedValue(value.toString());
    }
} else if (type == BINARY) {
    Blob blob = property.getValue(BINARY, index);
    json.value(TypeCodes.encode(type.tag(), blobs.serialize(blob)));
} else  {
    String value = property.getValue(STRING, index);
    if (type != STRING || TypeCodes.split(value) != -1) {
        value = TypeCodes.encode(type.tag(), value);
    }
    json.value(value);
}
```

* **Booleans and longs are unquoted JSON literals.**
* **Doubles render in Java's own textual form** (`Double.toString`) as an
  unquoted number, except `NaN` and the two infinities, which render as the
  quoted string `dou:NaN`, `dou:Infinity`, `dou:-Infinity`.
* **Binaries** render as the quoted string `:blobId:<base64>` —
  `TypeCodes.encode` uses the code `:blobId` for `BINARY`, not a three-letter
  abbreviation.
* **Every other type renders as a quoted string prefixed by its type code**,
  which is the lower-cased first three characters of
  `PropertyType.nameFromValue(tag)` followed by `:` — `nam:`, `dat:`, `pat:`,
  `ref:`, `wea:`, `uri:`, `dec:`.
* **A plain `STRING` is prefixed too, whenever the type-code splitter
  recognizes a prefix in it**:

  ```java
  public static int split(String jsonString) {
      if (jsonString.startsWith(":blobId:")) { return 7; }
      else if (jsonString.length() >= 4 && jsonString.charAt(3) == ':') { return 3; }
      else { return -1; }
  }
  ```

  — a string starting with `:blobId:`, or of length four or more with `:` at
  index 3, **whether or not the three characters are a known code**. So the
  `STRING` `jcr:title` renders as `"str:jcr:title"`, and so does `abc:x`. This
  is the rule a renderer is most likely to miss, and the fixture exercises it.

### 8.4 The binary size refusal

`oak-store-spi`, `json/Base64BlobSerializer.java`:

```java
private static final int DEFAULT_LIMIT = Integer.getInteger("oak.serializer.maxBlobSize", (int)FileUtils.ONE_MB);
…
public String serialize(Blob blob) {
    checkArgument(blob.length() < maxSize, "Cannot serialize Blob of size [%s] which is more than allowed maxSize of [%s]", blob.length(), maxSize);
```

**A blob at or above the limit is refused**, not truncated — the test is
`length() < maxSize`. The default limit is 1 MiB. froe raises a typed error at
the same boundary.

### 8.5 String escaping, in two phases

`oak-commons`, `commons/json/JsopBuilder.java`. **Phase one** scans:

```java
private static boolean shouldEscape(char c) {
    return c == '\"' || c == '\\' || c < ' ' || (c >= 0xd800 && c <= 0xdbff);
}
```

and if no character trips it, the string is appended **untouched** between
quotes. Note what is *not* in that test: **DEL (U+007F) and the whole C1 range
are emitted raw**, and so is a **lone low surrogate** (U+DC00–U+DFFF), because
only the high-surrogate range triggers the scan.

**Phase two** runs only when phase one tripped:

```java
case '"':  buff.append("\\\""); break;
case '\\': buff.append("\\\\"); break;
case '\b': buff.append("\\b"); break;
case '\f': buff.append("\\f"); break;
case '\n': buff.append("\\n"); break;
case '\r': buff.append("\\r"); break;
case '\t': buff.append("\\t"); break;
default:
    if (c < ' ') {
        buff.append(String.format("\\u%04x", (int) c));
    } else if (Character.isSurrogate(c)) {
        if (i < length - 1 && Character.isSurrogatePair(c, s.charAt(i + 1))) {
            buff.append(c); buff.append(s.charAt(i + 1)); i += 1;
        } else {
            buff.append(String.format("\\u%04x", (int) c));
        }
    } else {
        buff.append(c);
    }
```

— `\"` and `\\` first, then the five named escapes, then **lower-case
`\uXXXX`** for other controls and for **either half** of an unpaired surrogate.

The two phases together produce an observable asymmetry froe must reproduce:
**a lone low surrogate stays raw in a string that trips nothing else, and is
escaped as `\udcXX` in a string that also contains, say, a quote.**

### 8.6 The pretty-printed layout

`JsopBuilder.prettyPrint(StringBuilder, JsopTokenizer, String ident)` with
`ident = "  "`:

```java
case '{':
    if (t.matches('}')) { buff.append("{}"); }
    else { buff.append("{\n").append(space += ident); }
    break;
case '}':
    space = space.substring(0, space.length() - ident.length());
    buff.append('\n').append(space).append("}");
    break;
case '[': inArray = true;  buff.append("["); break;
case ']': inArray = false; buff.append("]"); break;
case ',':
    if (!inArray) { buff.append(",\n").append(space); } else { buff.append(", "); }
    break;
default:
    buff.append((char) token).append(' ');
    break;
```

* the indent unit is **two spaces**;
* an object with no members is **`{}` inline**; otherwise `{` is followed by a
  newline and the deeper indent, and `}` by a newline and the shallower one;
* **arrays stay on one line**, elements separated by `", "`;
* object members are separated by `",\n"` plus the current indent;
* **every other token — the `:` between a key and its value — is followed by a
  single space**, which is the `default` branch.

## 9. Which paths the printer walks

`oak-core`, `plugins/index/IndexPathServiceImpl.java`, `getIndexPaths`. **Two
branches, and neither is a sort.**

### 9.1 The precondition that throws before either branch

```java
NodeState nodeType = NodeStateUtils.getNode(nodeStore.getRoot(), "/oak:index/nodetype");
Validate.checkState("property".equals(nodeType.getString("type")), "nodetype index at " +
        "/oak:index/nodetype is found to be disabled. Cannot determine the paths of all indexes");
```

`/oak:index/nodetype` must **exist** and its `type` must read **strictly** as
the `STRING` `property`. A missing node yields `getString` → `null` and throws
just the same. This is checked **before** either branch is chosen, so a
disabled nodetype index refuses the whole enumeration whatever it declares.

### 9.2 Branch one — the `/oak:index` fallback

```java
boolean indxDefnTypeIndexed = IterableUtils.contains(nodeType.getNames(DECLARING_NODE_TYPES), INDEX_DEFINITIONS_NODE_TYPE);
if (!indxDefnTypeIndexed) {
    log.warn("{} is not found to be indexed as part of nodetype index. Non root indexes would not be listed", …);
    NodeState oakIndex = nodeStore.getRoot().getChildNode("oak:index");
    return IterableUtils.transform(IterableUtils.filter(oakIndex.getChildNodeEntries(),
            cne -> INDEX_DEFINITIONS_NODE_TYPE.equals(cne.getNodeState().getName(JCR_PRIMARYTYPE))),
            cne -> PathUtils.concat("/oak:index", cne.getName()));
}
```

* the declaration test is a **strict `NAMES`** read, so a `STRINGS`-typed
  `declaringNodeTypes` takes **this** branch even though the property-index
  lookup would select the index converting
  ([`index-property-storage.md`](index-property-storage.md) §6);
* the result is `/oak:index`'s **stored child order**, filtered to children
  whose `jcr:primaryType` reads **strictly** as the `NAME`
  `oak:QueryIndexDefinition`;
* **the Sling fixture takes this branch**, because Oak's initial content
  creates `/oak:index/nodetype` with `declaringNodeTypes` null
  ([`index-property-storage.md`](index-property-storage.md) §1). Oak's own
  verdict there is the logged warning that non-root indexes will not be listed.

### 9.3 Branch two — the node-type index's own mirror walk

Otherwise the paths come from a query:

```java
return () -> {
    Iterator<IndexRow> itr = getIndex().query(createFilter(INDEX_DEFINITIONS_NODE_TYPE), nodeStore.getRoot());
    return IteratorUtils.transform(itr, input -> input.getPath());
};
```

`getIndex()` is a `NodeTypeIndex`, whose `query` is

```java
NodeTypeIndexLookup lookup = new NodeTypeIndexLookup(root, mountInfoProvider);
if (!hasNodeTypeRestriction(filter) || !lookup.isIndexed(filter.getPath(), filter)) {
    throw new IllegalStateException("NodeType index is used even when no index is available for filter " + filter);
}
return Cursors.newPathCursorDistinct(lookup.query(filter), filter.getQueryLimits());
```

(`oak-core`, `plugins/index/nodetype/NodeTypeIndex.java`.) So there is a
**second precondition that throws**: `isIndexed` requires, **for
`jcr:primaryType` and for `jcr:mixinTypes` alike**, that
`PropertyIndexLookup.getIndexNode` finds a `property` definition that lists the
property and has an `:index` child. That selection is not "whatever
`/oak:index/nodetype` is":

```java
for (ChildNodeEntry entry : state.getChildNodeEntries()) {
    NodeState index = entry.getNodeState();
    PropertyState type = index.getProperty(TYPE_PROPERTY_NAME);
    if (type == null || type.isArray() || !getType().equals(type.getValue(Type.STRING))) { continue; }
    if (IterableUtils.contains(getNames(index, PROPERTY_NAMES), propertyName)) {
        NodeState indexContent = index.getChildNode(INDEX_CONTENT_NODE_NAME);
        if (!indexContent.exists()) { continue; }
        Set<String> supertypes = getSuperTypes(filter);
        if (index.hasProperty(DECLARING_NODE_TYPES)) {
            if (supertypes != null) {
                for (String typeName : getNames(index, DECLARING_NODE_TYPES)) {
                    if (supertypes.contains(typeName)) { return index; }
                }
            }
        } else if (supertypes == null) { return index; }
        else if (fallback == null) { fallback = index; }
    }
}
return fallback;
```

(`oak-core`, `plugins/index/property/PropertyIndexLookup.java`.) In
`/oak:index` stored order: the `type` is read **converting** and **arrays are
skipped**; `propertyNames` and `declaringNodeTypes` are read through
`getNames`, which is strict `NAMES` **with a converting `STRINGS` fallback and
a warning**; the **first** definition whose `declaringNodeTypes` names
`oak:QueryIndexDefinition` or a supertype wins immediately; one naming other
types is **never** selected; and the **first without `declaringNodeTypes`** is
the fallback — which in the fixture is `/oak:index/nodetype`, whatever precedes
it in child order.

The walk itself is `lookup.query` for `jcr:primaryType` chained with
`lookup.query` for `jcr:mixinTypes` (`NodeTypeIndexLookup.query`), each a
depth-first mirror walk under the matching key nodes, the whole chain
**de-duplicated by path** by `newPathCursorDistinct`. Because the lookup reads
`propertyNames` converting, this branch **can list a definition branch one
omits**.

froe reproduces both branches and both preconditions, with one deliberate
difference at the *listing* level: `IndexInventory::collect` **warns where the
path service throws**, so `froe index list` runs on a store whose nodetype
index is disabled while `froe index definitions` refuses — the refusal being
correct there, because a definitions file with the wrong key set is worse than
none.

## 10. AEM safety invariants

1. **`/:async` lane checkpoints belong to the lane, not to one index.** An
   offline tool may write a definition's own properties and hidden children; it
   must never write `L`, `L-temp`, `L-lease` or `L-LastIndexedTo`. Writing a
   lane checkpoint that does not resolve makes the next cycle re-run from the
   missing state, which for a Lucene index **doubles it** (§3.1) with no error
   anywhere.
2. **A flag Oak reads strictly must be written as its stored type.**
   `retainNodeInReindex`, `reindex-async` and `:disableIndexesOnNextCycle` are
   `BOOLEAN`-only (§5.1, §5.2, §5.5). A `STRING` `"true"` there is invisible to
   Oak, so a retained `:property-index` would be deleted by the next reindex —
   silent loss of the synchronous half of a hybrid index.
3. **Leave `reindex` and `reindexCount` exactly as a real reindex would.**
   `reindex = false` as a `BOOLEAN`, `reindexCount` incremented by one. A
   definition left flagged is rebuilt again by the next cycle — hours of work
   on a large repository; one left un-incremented makes every later
   drift comparison and every oak-run version-purge reason from a wrong count.
4. **Never invent an index `type`.** A type with no registered provider makes
   Oak set `reindex = true` by default (§5.4).
5. **`:index-definition` is a *visible clone*, and which state it clones
   differs between a reindex and an import** (§6.2). Writing the post-reindex
   state where Oak writes the base state makes `froe index list` and Oak's own
   index printer disagree about whether the definition has drifted, on a store
   nothing is wrong with.
6. **`creationTimestamp` is absent after a reindex or an import** (§6.2), and
   so is `:status` on a cycle that indexed nothing (§6.1). Neither is a defect.
7. **Never write `supersedes` or act as the disabler.** Disabling a superseded
   index changes which index answers a query; that is a decision for the
   operator and for a running Oak, not for an offline tool.
8. **A definitions file is an artifact, not a report.** It is consumed by
   Oak's own definition updater, which replaces the whole node with it (§7.2).
   A renderer that drops a hidden property, or gets a type code wrong, produces
   a file that silently changes a definition on import. That is why the
   rendering is held to a byte comparison against Oak's own printer rather
   than to a round trip through froe.
