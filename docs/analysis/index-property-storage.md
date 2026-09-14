# Index storage: the property family

Behaviour-exact specification of the repository structures Oak's `property`,
unique, node-type, `reference` and `counter` indexes store, and of the exact
computation that produces them. froe reads these structures in plan 0006 and
rebuilds them in plan 0007; a rebuild can only be held to the standard "what
Oak's own reindex would have written" if every input to that computation is
named here.

Java sources cited below are under
`oak-core/src/main/java/org/apache/jackrabbit/oak/plugins/`, unless another
module is named. Paths are given relative to that directory, so
`index/property/PropertyIndexEditor.java` is
`oak-core/src/main/java/org/apache/jackrabbit/oak/plugins/index/property/PropertyIndexEditor.java`.
The revision read is the one
[`README.md`](README.md) pins, Apache Jackrabbit Oak commit
`4984c4cf26a7ca58ae9ce12c63190b7f492bda78`; where the consumer build this
repository verifies against — `oak-segment-tar` 1.90.0 inside the digest-pinned
Sling image — differs, the difference is called out and the consumer build wins.

Builds on (does not repeat):

- [`index-definitions.md`](index-definitions.md) — what a definition node is,
  how it is discovered, the `async` lane properties, the reindex protocol, the
  path filter's construction, and the JSON the definition printer emits.
- [`node-layer.md`](node-layer.md) — how a node's properties and children are
  encoded in a segment store.
- [`record-layer.md`](record-layer.md) — value records, including the string
  values every non-binary property value is stored as.

---

## 1. The four editors, and which structure each one writes

| Index `type` | Editor | Hidden child written |
| --- | --- | --- |
| `property` (mirror) | `index/property/PropertyIndexEditor.java` | `:index` |
| `property` (`unique = true`) | the same editor, different strategy | `:index` |
| `reference` | `index/reference/ReferenceEditor.java` | `:references`, `:weakreferences` |
| `counter` | `index/counter/NodeCounterEditor.java` | `:index` |

The node-type index is not a fourth kind: `/oak:index/nodetype` is an ordinary
`property` index over `jcr:primaryType` and `jcr:mixinTypes`, created by
`InitialContent.initialize` (`oak-core/src/main/java/org/apache/jackrabbit/oak/InitialContent.java`,
line `IndexUtils.createIndexDefinition(index, "nodetype", true, false, List.of(JCR_PRIMARYTYPE, JCR_MIXINTYPES), null)`)
with its `declaringNodeTypeNames` argument **null**, so the definition carries
no `declaringNodeTypes` property at all — `IndexUtils.createIndexDefinition`
(`index/IndexUtils.java`) writes that property only for a non-null, non-empty
collection. Both property names are stored as `NAMES`
(`PropertyStates.createProperty(PROPERTY_NAMES, propertyNames, NAMES)`, same
method). That single fact decides which branch the definition printer takes
over a stock Oak store; see [`index-definitions.md`](index-definitions.md).

Two system properties leave the default code paths entirely and are **out of
scope** for froe, which implements the default of both
(`index/counter/jmx/NodeCounter.java`):

* `oak.countHashed`, default `"true"`
  (`Boolean.parseBoolean(System.getProperty("oak.countHashed", "true"))`).
  Setting it false selects the unhashed counter editor and its matching
  reader.
* `oak.index.useCounterOld`, default false (`Boolean.getBoolean`). Setting it
  true replaces both the editor (`NodeCounterEditorProvider.getIndexEditor`
  returns a `NodeCounterEditorOld`) and the reader
  (`NodeCounter.getEstimatedNodeCount` delegates to `NodeCounterOld`) before
  the default estimate is reached.

## 2. The mirror strategy

`index/property/strategy/ContentMirrorStoreStrategy.java`.

### 2.1 Insert

`insert(NodeBuilder index, String key, String value)`, where `value` is the
absolute path of the indexed node:

```java
ApproximateCounter.adjustCountSync(index, 1);
NodeBuilder builder = fetchKeyNode(index, key);   // index.child(key)
ApproximateCounter.adjustCountSync(builder, 1);
for (String name : PathUtils.elements(value)) {
    builder = builder.child(name);
}
builder.setProperty("match", true);
```

Four facts follow, each load-bearing for a reader and for a rebuild:

1. **The structure is `:index/<key>/<path elements…>` with `match = true`**, a
   `BOOLEAN`, on the node the indexed path addresses.
2. **`match` is set unconditionally, on whatever node the descent reaches.**
   It therefore lands on an *interior* trie node whenever a shorter indexed
   path shares the key with a longer one. A reader that assumes `match` only
   on leaves is wrong; §12.1 shows the case in the real fixture.
3. **The root path maps to the key node itself.** `PathUtils.elements("/")`
   yields nothing (`oak-commons/src/main/java/org/apache/jackrabbit/oak/commons/PathUtils.java`,
   `elements`: `pos = isAbsolute(path) ? 1 : 0` and `pos >= path.length()`
   immediately for `"/"`), so the loop body never runs and `match` is set on
   `:index/<key>`.
4. **The approximate counter is adjusted on exactly two nodes**: the `:index`
   node and the key node. A path-element node never carries one.

### 2.2 Remove and prune

`remove(NodeBuilder index, String key, String value)` adjusts the `:index`
counter by `-1` unconditionally and the key node's by `-1` only when that node
exists, collects the builders along `:index/<key>/<path elements…>` into a
deque so that iteration runs **innermost first** (each deeper node is pushed
with `builders.addFirst`, the key node having been pushed first), removes the
`match` property from the addressed node, and calls `prune`:

```java
void prune(final NodeBuilder index, final Deque<NodeBuilder> builders, final String key) {
    for (NodeBuilder node : builders) {
        if (node.getBoolean("match") || node.getChildNodeCount(1) > 0) {
            return;
        } else if (node.exists()) {
            node.remove();
        }
    }
}
```

So the prune walks from the addressed node upwards and **stops at the first
node that still carries `match` or still has a child**. The key node is the
last element of the deque and is pruned by the same rule, which is why a key
whose last entry is removed disappears entirely. `node.getBoolean("match")` is
`NodeState.getBoolean`'s strict read (§6), so a `match` of any type other than
`BOOLEAN` does not protect a node from the prune.

### 2.3 The `:index` node always exists for a property index

`PropertyIndexEditor.checkUniquenessConstraints` runs on `leave` of the *root*
editor — outside the `pathFilterResult == INCLUDE` guard — and its first
statement is unconditional:

```java
if (parent == null) {
    // make sure that the index node exist, even with no content
    definition.child(INDEX_CONTENT_NODE_NAME);
```

A `property` definition that indexes nothing therefore still ends a cycle with
an empty `:index` child. This is what distinguishes it from the counter (§9.5)
and the reference index (§8.4), which both leave their hidden children absent
when nothing was indexed.

## 3. The unique strategy

`index/property/strategy/UniqueEntryStoreStrategy.java`.

`insert` writes a single multi-valued `STRING` property named `entry` on
`:index/<key>`, holding the absolute path:

```java
ApproximateCounter.adjustCountSync(index, 1);
NodeBuilder k = index.child(key);
ArrayList<String> list = new ArrayList<String>();
list.add(value);
if (k.hasProperty("entry")) { … }             // transient duplicate state
PropertyState s2 = MultiStringPropertyState.stringProperty("entry", list);
k.setProperty(s2);
```

* **The approximate counter is adjusted on the `:index` node alone.** The key
  node carries none — the counter call precedes `index.child(key)` and there
  is no second call. §12.2 confirms this against the fixture.
* **More than one `entry` value is a transient duplicate state**, set only
  "while trying to add a duplicate entry" (the comment at the branch). The
  editor then refuses the commit: `checkUniquenessConstraints` →
  `getFirstDuplicate` → `throw new CommitFailedException(CONSTRAINT, 30, …)`
  in `PropertyIndexEditor`. A duplicate surviving in a committed store is
  therefore a defect, and `froe index check` reports it as one.
* `remove` deletes the whole key node when `entry` has one value, and rewrites
  `entry` without the removed path otherwise.

Which strategy applies to a definition is decided by one strict `BOOLEAN`
read, in all three readers of it, so there is never a disagreement between the
editor and the query side about the shape on disk:

| Caller | Expression |
| --- | --- |
| editor | `definition.getBoolean(IndexConstants.UNIQUE_PROPERTY_NAME)`, `PropertyIndexEditor` constructor |
| lookup | `definition.getBoolean(IndexConstants.UNIQUE_PROPERTY_NAME)`, `PropertyIndexLookup.getStrategies` |
| planner | `definition.getBoolean(IndexConstants.UNIQUE_PROPERTY_NAME)`, `PropertyIndexPlan` constructor |

`NodeState.getBoolean` is strict (§6), so a `unique` stored as the `STRING`
`"true"` — what a Sling POST without `@TypeHint=Boolean` stores — reads as
`false` everywhere and the index is a **mirror** index to Oak. froe selects the
strategy through that one field and reports the stored type as a warning.

## 4. Key derivation

`index/property/PropertyIndexUtil.java`:

```java
private static final int MAX_STRING_LENGTH = 100;
private static final String EMPTY_TOKEN = ":";

public static Set<String> encode(PropertyValue value, ValuePattern pattern) {
    return encode(ValuePatternUtil.read(value, pattern));
}

public static Set<String> encode(Set<String> set) {
    if (set == null || set.isEmpty()) { return set; }
    Set<String> values = new HashSet<>();
    for (String v : set) {
        if (v.isEmpty()) {
            v = EMPTY_TOKEN;
        } else {
            if (v.length() > MAX_STRING_LENGTH) {
                v = v.substring(0, MAX_STRING_LENGTH);
            }
            v = URLEncoder.encode(v, StandardCharsets.UTF_8);
        }
        values.add(v);
    }
    return values;
}
```

The derivation is therefore, in order:

1. **Read the values through the value pattern.** `ValuePatternUtil.read`
   (`index/property/ValuePatternUtil.java`) converts the property value to
   `Type.STRINGS` and keeps each value for which `pattern.matches(v)` holds.
2. **The empty token.** An empty value becomes the key `:`. Note the order:
   the empty test precedes truncation and encoding, so the empty key is the
   literal one-character name `:`, never a URL-encoded `%3A`. It is a hidden
   name, and a reader that skips hidden children at the key level would lose
   it (§7).
3. **Truncation to 100 UTF-16 code units.** `String.substring(0, 100)` counts
   `char`s, not code points, so a value whose 100th and 101st units are a
   surrogate pair is cut between them.
4. **Java's URL encoding of the truncated value as UTF-8**, which is
   `java.net.URLEncoder.encode(String, Charset)`: alphanumerics and `.`, `-`,
   `*`, `_` pass through; a space becomes `+`; every other character is
   emitted as `%XX` per UTF-8 byte with upper-case hexadecimal digits. **A
   lone surrogate encodes as `%3F`** — the encoder's charset encoder replaces
   an unmappable character with `?`, whose encoding is `%3F` — which is why
   step 3 can produce a `%3F` key with no `?` anywhere in the source value.
   froe reproduces this in `crates/froe/src/java/url_encoder.rs`, pinned by
   vectors generated with the pinned image's own JDK.

**The result is a `Set`.** Two values that collapse to one key — two empty
values, or two values sharing a 100-unit prefix — contribute a single key. A
unique index built from such a property is therefore *not* in a duplicate
state, and a rebuild that emitted one key per value would wrongly refuse it.

### 4.1 The value pattern's match rule

`index/property/ValuePattern.java`, `matches(String v)`:

```java
if (matchesAll() || v == null) { return true; }
if (includePrefixes != null) {
    for (String inc : includePrefixes) { if (v.startsWith(inc)) { return true; } }
}
if (excludePrefixes != null) {
    for (String exc : excludePrefixes) {
        if (v.startsWith(exc) || exc.startsWith(v)) { return false; }
    }
}
if (includePrefixes != null && pattern == null) { return false; }
return pattern == null || pattern.matcher(v).matches();
```

In words, and in this order:

* `matchesAll()` — all three of `includePrefixes`, `excludePrefixes` and
  `pattern` absent — admits everything.
* **An include prefix wins outright.** A value with an include prefix is
  admitted even if it also has an exclude prefix.
* **An exclude prefix matches in either direction**: `v.startsWith(exc)` *or*
  `exc.startsWith(v)`. A value that is a proper prefix of an exclusion is
  excluded too.
* **A regular expression applies only when no include prefix matched.**
* **With include prefixes and no regular expression, a value matching none of
  them is rejected.**

All three may coexist on one definition, so the model carries all three and
answers the composed rule. froe carries no general regular-expression engine
and will not approximate Java's; `valuePattern` is therefore a typed
`Unsupported` refusal, raised **only when the composed rule actually reaches
the pattern** — never merely because one is stored, since an include prefix
that matches short-circuits before the pattern is consulted.

## 5. The value's string form

`ValuePatternUtil.read` converts through `value.getValue(Type.STRINGS)`. In a
segment store that conversion is not a conversion at all:
`SegmentPropertyState.getValue(RecordId id, Type<T> type)`
(`oak-segment-tar/src/main/java/org/apache/jackrabbit/oak/segment/SegmentPropertyState.java`)
reads the stored string and returns it verbatim for the string-shaped types:

```java
String value = reader.readString(id);
if (type == STRING || type == URI || type == DATE
        || type == NAME || type == PATH
        || type == REFERENCE || type == WEAKREFERENCE) {
    return (T) value; // no conversion needed for string types
}
```

For `LONG`, `DOUBLE`, `BOOLEAN` and `DECIMAL` the request is `STRING`, which is
in that list, so the same verbatim branch is taken: **the key string is the
stored string, byte for byte, for every property type.** froe already stores
every non-binary value as exactly that string
([`record-layer.md`](record-layer.md)), so its key derivation needs no numeric
formatting at all.

**Scope of that statement.** It is specific to `SegmentPropertyState`. The
generic `AbstractPropertyState`/`Conversions` path used by other node stores
*does* convert, running the stored string through
`Conversions.convert(value, base)` and back. For `DOUBLE`, `DECIMAL` and `DATE`
the two agree only because Oak wrote the stored string with the same converter
in the first place; the identity is a property of Oak's own writer, not of the
type. A froe-written value that did not round-trip through Java's
`Double.toString` would key differently on another node store even though it
keys identically here — which is why
[`write-record-writers.md`](write-record-writers.md) holds froe to Java's
rendering.

### 5.1 Which values reach the derivation at all

`PropertyIndexEditor.addValueKeys`:

```java
if (property.getType().tag() != PropertyType.BINARY && property.count() > 0) {
    keys = new HashSet<>(); …
    keys.addAll(encode(PropertyValues.create(property), pattern));
}
```

**Binaries contribute nothing**, single- or multi-valued, and so does an empty
multi-valued property. Every other value of a matching property contributes,
and the keys of every matching property name are unioned into one set
(`getMatchingKeys` loops over `propertyNames` accumulating into one `keys`).

## 6. Which properties of which nodes are indexed, and how strictly each is read

Oak's typed getters on a `NodeState` are **strict**: they return a value only
when the stored type is exactly the requested one, and a default otherwise.
`oak-store-spi/src/main/java/org/apache/jackrabbit/oak/spi/state/AbstractNodeState.java`:

```java
public static Iterable<String> getNames(NodeState state, String name) {
    PropertyState property = state.getProperty(name);
    if (property != null && property.getType() == NAMES) {
        return property.getValue(NAMES);
    } else {
        return emptyList();
    }
}
```

and likewise `getBoolean` (`== BOOLEAN`, else `false`), `getLong` (`== LONG`,
else `0`), `getString` (`== STRING`, else `null`), `getStrings` (`== STRINGS`,
else empty) and `getName` (`== NAME`, else `null`). `MemoryNodeBuilder`'s
same-named methods delegate to the node state
(`oak-store-spi/.../plugins/memory/MemoryNodeBuilder.java`), so a `NodeBuilder`
read is exactly as strict.

A read through `PropertyState.getValue(Type)` on the other hand **converts**.
The table below says, for each definition property froe models, which kind of
read each consumer performs — because the asymmetries between them are
observable on disk and in query results.

| Property | Editor (what is written) | Query planner / lookup (what is selected) |
| --- | --- | --- |
| `propertyNames` | **converting**: `names.getValue(NAME, 0)` for a one-valued property, `names.getValue(NAMES)` otherwise (`PropertyIndexEditor` constructor) | planner **strict** `definition.getNames(PROPERTY_NAMES)` (`PropertyIndexPlan`); lookup **converting with a warning** (`PropertyIndexLookup.getNames`) |
| `declaringNodeTypes` | **strict** `definition.getNames(DECLARING_NODE_TYPES)` (`PropertyIndexEditor` constructor) | planner **strict** `definition.getNames(...)`; lookup **converting with a warning** |
| `unique` | **strict** `definition.getBoolean(...)` | **strict** in both |
| `includedPaths`, `excludedPaths` | `PathFilter.getStrings`: `STRING` or `STRINGS` only, otherwise the defaults (`oak-store-spi/.../spi/filter/PathFilter.java`) | same code |
| `valuePattern` | **strict** `node.getString(VALUE_PATTERN)` (`ValuePattern(NodeBuilder)`) | **strict** `node.getString(...)` (`ValuePattern(NodeState)`) |
| `valueIncludedPrefixes`, `valueExcludedPrefixes` | array: **converting** `getProperty(name).getValue(Type.STRINGS)`; single value: **strict** `node.getString(name)` (`ValuePattern.getStrings(NodeBuilder, …)`) | array: **strict** `node.getStrings(name)`; single value: **strict** `node.getString(name)` (`ValuePattern.getStrings(NodeState, …)`) |

Four consequences an implementer will not guess, each of which froe reports as
a warning rather than reinterpreting:

* **A `STRING` `propertyNames` is indexed but never selected.** The editor
  converts it and fills `:index`; the planner's strict `NAMES` read yields
  nothing, so the property index never serves a query from it. A
  single-valued `NAME` reads as empty at the planner too — `getNames` wants
  `NAMES`, the array type. The node-type index's own path is the exception:
  `PropertyIndexLookup.getNames` falls back to `property.getValue(Type.STRINGS)`
  with a `log.warn`, so that lookup does select such a definition.
* **A non-`NAMES` `declaringNodeTypes` yields a predicate that matches
  nothing.** `TypePredicate` is constructed from
  `definition.getNames(DECLARING_NODE_TYPES)`, empty for any other stored
  type, so the index indexes nothing — while `definition.hasProperty(...)`
  was true, so the predicate is still installed.
* **A `NAMES`-typed `valueIncludedPrefixes` is indexed but never selected**,
  the same asymmetry as `propertyNames`: the editor converts the array, the
  planner's strict `getStrings` returns empty, and an empty include list with
  no pattern rejects every value (§4.1, last rule).
* **A `NAMES`-typed `valueExcludedPrefixes` is a query-correctness defect, not
  merely an unused index.** The planner reads the exclusion as empty and
  happily answers from an index the editor never populated with the excluded
  values, so the query returns *wrong, short* results. This is the one case
  where the asymmetry loses data rather than performance.
* **A single non-`STRING` prefix value is a definition Oak cannot index.**
  `ValuePattern.getStrings` takes the non-array branch and calls
  `node.getString(propertyName)`, which returns `null` for any other type;
  `Collections.singleton(null)` then reaches `matches`, where
  `v.startsWith(inc)` throws a `NullPointerException`. froe refuses such a
  definition with a typed error rather than guessing.

Oak's own definitions store `propertyNames` and `declaringNodeTypes` as
`NAMES` (`IndexUtils.createIndexDefinition`, §1), and §12.1 confirms it in the
fixture.

### 6.1 The node-type predicate

`nodetype/TypePredicate.java`. For each declared name, `addNodeType` reads the
node type's own definition under `/jcr:system/jcr:nodeTypes`:

```java
NodeState type = types.getChildNode(name);
for (String primary : type.getNames(REP_PRIMARY_SUBTYPES)) {
    primaryTypes = add(primaryTypes, primary);
}
if (type.getBoolean(JCR_ISMIXIN)) {
    mixinTypes = add(mixinTypes, name);
    for (String mixin : type.getNames(REP_MIXIN_SUBTYPES)) {
        mixinTypes = add(mixinTypes, mixin);
    }
} else {
    primaryTypes = add(primaryTypes, name);
}
```

* `rep:primarySubtypes` of the declared type always joins the primary set,
  whether or not the type is a mixin.
* A **mixin** additionally contributes its own name and `rep:mixinSubtypes` to
  the mixin set.
* A **non-mixin** contributes its own name to the primary set, and nothing to
  the mixin set. A declared primary type therefore never matches a node by its
  mixins — the asymmetry a rebuild must reproduce.
* The sets are built lazily, on the first `test`, from `root`.

`test(NodeState input)` reads the node **strictly**:

```java
if (primaryTypes != null && primaryTypes.contains(input.getName(JCR_PRIMARYTYPE))) { return true; }
if (mixinTypes != null && StreamUtils.toStream(input.getNames(JCR_MIXINTYPES)).anyMatch(mixinTypes::contains)) { return true; }
return false;
```

so a `jcr:primaryType` that is not a single `NAME`, or a `jcr:mixinTypes` that
is not `NAMES`, matches nothing.

`PropertyIndexEditor` installs the predicate only when the property is present
(`if (definition.hasProperty(DECLARING_NODE_TYPES))`). Without one,
`applyTypeRestrictions` does nothing at all — its whole body sits under
`if (typePredicate != null)` — and `enter`'s `typeChanged = typePredicate == null`
sets the flag true only to short-circuit the `typeChanged || isTypeProperty(name)`
test each property callback performs, which is what its `// disables property
name checks` comment means. With a predicate, a changed `jcr:primaryType` or
`jcr:mixinTypes` sets the flag and `applyTypeRestrictions` then re-reads the
matching keys of **both** states in full, because a diff-derived key set would
miss values whose membership changed only through the node's type.

### 6.2 The path filter's three verdicts

`oak-store-spi/.../spi/filter/PathFilter.java`, `filter(String path)`, in
order: **exclude wins** (an excluded path or any descendant of one), then
**include by ancestry** (the path is an include or a descendant of one), then
**traverse** (the path is a strict ancestor of an include), else exclude. The
editor consults it twice — once for its own path and once per child name — and
returns `null` for an `EXCLUDE` child, which prunes the whole subtree from the
diff (`PropertyIndexEditor.childNodeAdded`/`Changed`/`Deleted`). Only an
`INCLUDE` verdict reaches `updateIndex`
(`if (pathFilterResult == PathFilter.Result.INCLUDE)` in `leave`).

The filter's *construction* — the two absolute-path refusals and the
unification of includes against excludes — is specified in
[`index-definitions.md`](index-definitions.md), because the printer and the
budget derivation need it too.

### 6.3 Hidden nodes and properties are invisible to indexing

Every index update Oak drives is wrapped in `VisibleEditor`
(`oak-store-spi/.../spi/commit/VisibleEditor.java`), which drops hidden
children and hidden properties from the diff before an index editor ever sees
them. All five sites, so that no path escapes the rule:

| Site | Java |
| --- | --- |
| the reindex composite | `index/IndexUpdate.java`, `VisibleEditor.wrap(wrapProgress(CompositeEditor.compose(…)))` |
| the synchronous incremental editors | `index/IndexUpdateProvider.java`, `return VisibleEditor.wrap(editor)` |
| the asynchronous lane | `index/AsyncIndexUpdate.java`, `EditorDiff.process(VisibleEditor.wrap(indexUpdate), before, after)` |
| the importer's catch-up diff | `index/importer/IndexImporter.java`, same call |
| the out-of-band build | `oak-run-commons/.../index/OutOfBandIndexerBase.java`, same call |

A rebuild that walked hidden content would index nodes Oak never indexes.

## 7. Reading a mirror index back

The key level and the path levels obey different rules, and conflating them is
the reader defect this section exists to prevent.

* **At the key level** — the direct children of `:index`, `:references` and
  `:weakreferences` — **every child is a key, whatever its name.** For a
  property index the empty value's key is the hidden name `:` (§4) and every
  other key is URL-encoded, so no other key can begin with a colon; for a
  reference index the keys are unencoded identifiers (§8.1). Neither shape is
  something a reader should test for: the rule is the level, not the name. A
  reader that skipped hidden children here would silently drop every node whose
  indexed value is the empty string.
* **Below the key level**, names are content path elements, and hidden
  children and hidden properties are skipped exactly as the visible editor
  skips them (§6.3) — with one exception: the `:count_*` properties, which are
  counted and reported rather than treated as content.
* `match` is the only content-bearing property at a path level.

Enumeration order matters to plan 0007's builder, which must produce a
byte-identical tree: entries sort by `(key, path elements)` with the elements
compared as byte strings. That differs from a plain sort of full paths
whenever one indexed path is a strict prefix of another.

## 8. The reference index

`index/reference/ReferenceEditor.java`; the names are in
`NodeReferenceConstants.java`: `REF_NAME = ":references"` and
`WEAK_REF_NAME = ":weakreferences"` — the latter spelled in lower case with no
separator, unlike every other Oak name of the kind.

### 8.1 What is indexed

`propertyChanged(before, after)` (which `propertyAdded` and `propertyDeleted`
both delegate to) collects, per property:

```java
if (after.getType().tag() == REFERENCE) {
    if (!isVersionStorePath(getPath())) {
        put(newRefs, after.getValue(STRINGS), concat(getPath(), after.getName()));
    }
}
if (after.getType().tag() == WEAKREFERENCE) {
    put(newWeakRefs, after.getValue(STRINGS), concat(getPath(), after.getName()));
}
```

* The **key is the referenced identifier, unencoded** — the property's own
  value, with no URL encoding and no truncation. `update` passes
  `Set.of(key)` straight to the mirror strategy.
* The **value is the property's path, made relative** by stripping the leading
  `/`: `String asRelative = isAbsolute(value) ? value.substring(1) : value;`
  in `put`. So an entry for `/content/interop/a/@ref` is stored at
  `:references/<uuid>/content/interop/a/ref`.
* **Strong references under the version store are skipped**, weak ones never
  are. `isVersionStorePath` is `oakPath.startsWith(VERSION_STORE_PATH)` with
  `VERSION_STORE_PATH = "/jcr:system/jcr:versionStorage"`
  (`oak-core-spi/.../spi/version/VersionConstants.java`). Because that is a
  **plain string prefix test, not a path-ancestry test**, a sibling named
  `/jcr:system/jcr:versionStorage2` is excluded too. That is a quirk, not a
  design; froe reproduces it.

### 8.2 No path filter, no node-type restriction

`ReferenceEditorProvider.getIndexEditor` constructs
`new ReferenceEditor(definition, root, mountInfoProvider)` — there is no
`PathFilter` and no `TypePredicate` in the class at all. **`includedPaths`,
`excludedPaths` and `declaringNodeTypes` on a `reference` definition are
therefore ignored**, and the only restrictions are the version-store test of
§8.1 and the visible-editor wrap of §6.3. A budget derived from a reference
definition's `includedPaths` would bound a walk that in fact covers the whole
store.

### 8.3 A reindex processes every reference as an addition

`enter` sets `isReindex = true` when `MISSING_NODE == before && parent == null`,
and a diff from the missing state reports every property as added, which
`propertyAdded` forwards to `propertyChanged(null, after)`. The `newIds`
bookkeeping is skipped under `isReindex` (`childNodeAdded`), since there is no
move to reconcile.

### 8.4 Both hidden children are created lazily

`ReferenceEditor.update` builds the child through a memoized supplier:

```java
for (String p : add) {
    Supplier<NodeBuilder> index = memoize(() -> definition.child(store.getIndexNodeName()));
    store.update(index, p, name, definition, empty, Set.of(key));
}
```

and `ContentMirrorStoreStrategy.update` calls `index.get()` only inside its
loops over `beforeKeys` and `afterKeys`. With no keys there is no call, so the
supplier never runs and the child is never created. **A store with no weak
references ends Oak's reindex with `:weakreferences` absent**, and a store with
no references at all ends it with neither child. The fixture is in exactly that
state today (§12.3); interop task 0616 adds content that creates both.

## 9. The counter index

### 9.1 `resolution` and `seed`

`index/counter/NodeCounterEditorProvider.getIndexEditor`:

```java
int resolution;
PropertyState s = definition.getProperty(RESOLUTION);
if (s == null) {
    resolution = NodeCounterEditor.DEFAULT_RESOLUTION;      // 1000
} else {
    resolution = s.getValue(Type.LONG).intValue();
}
long seed;
s = definition.getProperty(SEED);
if (s != null) {
    seed = s.getValue(Type.LONG).intValue();
} else {
    seed = 0;
    if (NodeCounter.COUNT_HASH) {
        seed = UUID.randomUUID().getMostSignificantBits();
        definition.setProperty(SEED, seed);
    }
}
```

* `resolution` is read **converting** to `LONG` and then **narrowed to 32
  bits**, so a `STRING` `"500"` counts as 500.
* `seed` is created as the **most significant 64 bits of a random UUID** and
  used **untruncated** on the run that creates it, but every later run reads it
  back through `.intValue()` — **narrowed to 32 bits and sign-extended** into
  the `long` field. Only those 32 bits take part from then on. A reader that
  used the stored 64-bit value would compute a different hash than the store
  was built with on every run after the first. The fixture's stored seed is
  `-7610761686379641542` (§12.4); the value that matters is its low 32 bits,
  sign-extended.
* The bit mask is `(Integer.highestOneBit(resolution) * 2) - 1`
  (`NodeCounterEditor.NodeCounterRoot` constructor). For the default
  `resolution = 1000` that is `(512 * 2) - 1 = 1023`, and the increment is
  `bitMask + 1 = 1024`.

### 9.2 The SipHash chain

`index/counter/SipHash.java` is "an implementation of the SipHash-2-2
function, to prevent hash flooding". Two constructors and one accessor:

```java
public SipHash(long seed) {
    long k0 = seed;
    long k1 = Long.rotateLeft(seed, 32);
    v0 = k0 ^ 0x736f6d6570736575L;
    v1 = k1 ^ 0x646f72616e646f6dL;
    v2 = k0 ^ 0x6c7967656e657261L;
    v3 = k1 ^ 0x7465646279746573L;
}

public SipHash(SipHash parent, long m) {
    long v0 = parent.v0, v1 = parent.v1, v2 = parent.v2, v3 = parent.v3;
    for (int i = 0; i < 2; i++) {
        v0 += v1; v2 += v3;
        v1 = Long.rotateLeft(v1, 13); v3 = Long.rotateLeft(v3, 16);
        v1 ^= v0; v3 ^= v2;
        v0 = Long.rotateLeft(v0, 32);
        v2 += v1; v0 += v3;
        v1 = Long.rotateLeft(v1, 17); v3 = Long.rotateLeft(v3, 21);
        v1 ^= v2; v3 ^= v0;
        v2 = Long.rotateLeft(v2, 32);
    }
    v0 ^= m;
    this.v0 = v0; this.v1 = v1; this.v2 = v2; this.v3 = v3;
}

public int hashCode() {
    long x = v0 ^ v1 ^ v2 ^ v3;
    return (int) (x ^ (x >>> 16));
}
```

All arithmetic is 64-bit and wrapping; `Long.rotateLeft` masks its distance to
six bits, and `>>>` is the unsigned shift.

The editor chains one instance per path element
(`NodeCounterEditor.getHash`):

```java
if (parent == null) { h = new SipHash(root.seed); }
else { h = new SipHash(parent.getHash(), name.hashCode()); }
```

so `hash("/") = SipHash(seed)` and `hash(p + "/" + n) = SipHash(hash(p), n.hashCode())`,
where `name.hashCode()` is `java.lang.String::hashCode` — an `int`, widened to
`long` with **sign extension** at the call. froe's
`crate::hashing::utf16_string_hash` is that function.

### 9.3 The test and what it increments

`NodeCounterEditor.childNodeAdded`:

```java
SipHash h = new SipHash(getHash(), name.hashCode());
if ((h.hashCode() & root.bitMask) == 0) {
    count(root.bitMask + 1, currentMount);
}
return getChildIndexEditor(name, h);
```

* The test is on the **added child's own** hash.
* `count` is invoked on the *parent* editor and recurses upward
  (`if (parent != null) { parent.count(offset, mount); }`), so the increment
  lands on **every strict ancestor of the hit node**, the node itself
  excluded. `:cnt` at `:index/<p>` is therefore a count of descendants of `p`,
  which is what "approximate descendant node counter" means.
* `childNodeDeleted` runs the identical test and subtracts the same amount,
  which is why "after adding and removing all nodes the count goes back to
  zero" (the class comment on `NodeCounter.COUNT_HASH`).

### 9.4 What is written, and when `:cnt` is removed

`NodeCounterEditor.leaveNew`, per mount:

```java
PropertyState p = builder.getProperty(COUNT_HASH_PROPERTY_NAME);   // ":cnt"
long count = p == null ? 0 : p.getValue(Type.LONG);
count += countOffset;
if (count <= 0) {
    if (builder.getChildNodeCount(1) >= 0) {
        builder.removeProperty(COUNT_HASH_PROPERTY_NAME);
    } else {
        builder.remove();
    }
} else {
    builder.setProperty(COUNT_HASH_PROPERTY_NAME, count);
}
```

`getChildNodeCount` never returns a negative number — it returns
`Long.MAX_VALUE` when the exact count is unknown — so the `else` branch is
unreachable and **Oak removes the property but never the node**. An
incrementally maintained counter index therefore accumulates mirror nodes
carrying no `:cnt` at all after deletions, and a reader must report such a node
with an *absent* count rather than a zero.

`getBuilder` is what creates the storage node:

```java
if (parent == null) { return root.definition.child(Multiplexers.getNodeForMount(mount, DATA_NODE_NAME)); }
else { return parent.getBuilder(mount).child(name); }
```

with `DATA_NODE_NAME = ":index"`.

### 9.5 A small store ends a reindex with no `:index` at all

`leaveNew` returns immediately when `countOffsets.isEmpty()`, before
`getBuilder` is ever called. With the default resolution one node in about 1024
hits the test, so a store small enough that no node hits ends Oak's reindex
with the `counter` definition holding **no `:index` child**. That is a legal
state, not a defect, and it is the state `estimated_node_count` answers
`Unknown` for (§10).

### 9.6 Mount-decorated data nodes

`Multiplexers.getNodeForMount(mount, ":index")` returns `":index"` for the
default mount and `":" + mount.getPathFragmentName() + "-index"` otherwise
(`index/property/Multiplexers.java`, `getNodeForMount` and `asSuffix`). The
reader recognizes both:

```java
private static boolean isDataNodeName(ChildNodeEntry childNodeEntry) {
    String name = childNodeEntry.getName();
    return NodeCounterEditor.DATA_NODE_NAME.equals(name)
            || (name.startsWith(":") && name.endsWith("-" + Multiplexers.stripStartingColon(NodeCounterEditor.DATA_NODE_NAME)));
}
```

froe detects and reports mount-decorated children; it does not model composite
mounts.

## 10. The estimate

`index/counter/jmx/NodeCounter.java`, `doGetEstimatedNodeCount(root, path, max)`
— the default path, reached when both switches of §1 sit at their defaults.
The rules, in the order Oak applies them, with the constants it actually uses:

1. **`0` when the target node does not exist.**
   `NodeState s = child(root, PathUtils.elements(path)); if (s == null || !s.exists()) return 0;`
2. **Under `max == false` only**, the target node's *own* approximate count,
   when present: `ApproximateCounter.getCountSync(s)`, returned unless it is
   `-1`. This is the branch that answers for a property index's `:index` node.
3. **Under both bounds**, the target node's combined `:cnt` and `:count`, when
   either is present, plus `ApproximateCounter.COUNT_RESOLUTION` — **100**, the
   approximate counter's own resolution, never the definition's `resolution` —
   for `max`, and nothing for the expected bound
   (`getCombinedCount`, first statement).
4. **`-1` when the index literally named `counter` has no data node.** Oak
   consults `child(root, "oak:index", "counter")` — a fixed name, not a search
   by type — and returns `-1` if that node is absent or if `dataNodeExists`
   finds no `:index` or `:<mount>-index` child.
5. **Otherwise the sum over every data child** of the combined `:cnt` and
   `:count` of the node reached by descending `path`'s elements under it
   (`getIndexingData`), plus 100 for `max`. A non-root `path` therefore answers
   for that subtree, not for the store.
6. **When that sum is zero**, Oak answers `COUNT_RESOLUTION * 20` — 2000 — for
   `max` and `0` for the expected bound. **Neither number counts anything**:
   they are placeholders for "the sampling counter never recorded this path".
   froe returns a `Fallback` verdict instead of the number and lets the caller
   decide, which is how `froe index check` tells "no estimate" from "an
   estimate of zero".

The definition's `resolution` plays no part in the estimate at any step.

`ApproximateCounter.getCountSync` (§11) is `-1` when the node carries no
`:count_*` property at all, and otherwise `max(added / 2, added - removed)`
over the positive and negated-negative values.

## 11. The approximate counter, and why a comparison must exclude it

`index/counter/ApproximateCounter.java`:

```java
public static final String COUNT_PROPERTY_PREFIX = ":count_";
public static final int COUNT_RESOLUTION = 100;
public static final int COUNT_MAX = 10000000;
private static final Random RANDOM = new Random();

private static void adjustCountSync(NodeBuilder builder, boolean added) {
    if (RANDOM.nextInt(COUNT_RESOLUTION) != 0) { return; }
    int max = getMaxCount(builder, added);
    if (max >= COUNT_MAX) { return; }
    int x = Math.max(COUNT_RESOLUTION, max * 2) / COUNT_RESOLUTION;
    if (RANDOM.nextInt(x) > 0) { return; }
    long value = x * COUNT_RESOLUTION;
    String propertyName = COUNT_PROPERTY_PREFIX + UUID.randomUUID();
    builder.setProperty(propertyName, added ? value : -value);
}
```

Every one of the three things a comparison could key on is drawn from a random
generator:

* **the name** — `":count_" + UUID.randomUUID()`;
* **the presence** — two independent `RANDOM.nextInt` gates decide whether a
  property is written at all;
* **the value** — `x * COUNT_RESOLUTION`, where `x` depends on the largest
  same-signed value already on the node, which is itself randomized.

**No rebuild can reproduce them**, and two Oak reindexes of identical content
differ on them. A digest comparison that is to prove "froe's rebuilt index
equals Oak's own rebuild" must therefore excuse exactly the properties whose
name starts with `:count_` — which is what `froe digest
--exclude-property-prefix` exists for — and nothing else. The read side is
unaffected: `getCountSync` tolerates any subset, returning `-1` when none is
present.

## 12. Worked example, checked against the real Sling fixture

The fixture is the store `generate` produces: Apache Sling 14 with
`oak-segment-tar` 1.90.0, stopped cleanly, its segment store extracted. Every
node quoted below was read from it with `froe node`.

### 12.1 One `sling:Folder` node under `/content/interop`

`/content/interop` has `jcr:primaryType = sling:Folder` (a `NAME`). The
definition that indexes it is `/oak:index/nodetype`:

```text
property  jcr:primaryType <Name>    = "oak:QueryIndexDefinition"
property  propertyNames   <Name[]>  = ["jcr:primaryType","jcr:mixinTypes"]
property  type            <String>  = "property"
property  reindex         <Boolean> = false
property  reindexCount    <Long>    = 1
child     :index
```

`propertyNames` is `NAMES` and `type` is a single `STRING`, as §6 says Oak's
own definitions are; there is no `declaringNodeTypes`, as §1 says.

Derivation: the property matches (`getMatchingKeys` over `propertyNames`), it
is not binary and has one value (§5.1), the value pattern is `MATCH_ALL`, the
value's string form is `sling:Folder` verbatim (§5), it is non-empty and under
100 units, and Java's URL encoding maps `:` to `%3A` and leaves the letters
alone (§4). The key is therefore `sling%3AFolder`, and the mirror insert
descends `content`, `interop` from it (§2.1). Read back:

```text
$ froe node <store> '/oak:index/nodetype/:index/sling%3AFolder/content/interop'
property  match <Boolean> = true
child     throwaway
child     files
child     pages
```

`match = true` on a node that **also has children** — the interior-trie case of
§2.1 fact 2, in the real fixture, because `/content/interop/files` and
`/content/interop/pages` are `sling:Folder` nodes too. The key node itself
carries the approximate counters and no `match`:

```text
$ froe node <store> '/oak:index/nodetype/:index/sling%3AFolder'
property  :count_0daeb465-9c87-4a82-96ec-32360239c509 <Long> = 200
property  :count_65f31f1f-2d09-40c0-9c11-e4ef0412bc95 <Long> = 400
property  :count_3239b6a2-c389-4e43-abbf-1828d6e6eee5 <Long> = 100
child     conf
child     etc
child     content
child     libs
child     var
child     apps
```

which is §2.1 fact 4 — counters on `:index` and on the key node, and none on
`content` or `interop` — and §11's randomized names and values, each a multiple
of the counter's own resolution of 100.

### 12.2 One `mix:referenceable` node

`/oak:index/uuid` is `unique = true` (a `BOOLEAN`, §3) over `jcr:uuid`. Its
storage is the unique strategy's:

```text
$ froe node <store> '/oak:index/uuid/:index'
property  :count_21b3108d-fb83-423a-a182-bac83ad9dc59 <Long> = 200
property  :count_c8897fed-79f6-41c7-b4e5-8ca7b2221765 <Long> = 100
property  :count_0871bca5-354d-4314-bd3e-680054607da7 <Long> = 400
property  :count_d142018e-fe7a-4540-bcdc-715a9f46ffdf <Long> = 800
child     4c2b99e6-28cc-43fa-bd44-0c084fc7639f
…

$ froe node <store> '/oak:index/uuid/:index/4c2b99e6-28cc-43fa-bd44-0c084fc7639f'
property  entry <String[]> = ["/libs/jslibs/bootstrap-table/1.14.2/extensions/select2-filter/bootstrap-table-select2-filter.js/jcr:content"]
```

One `entry` value, the **absolute** path (§3, against §8.1's relative one), and
the approximate counters on `:index` **only** — the key node carries none,
which is the difference from §12.1 that distinguishes the two strategies on
disk. The key is the UUID unencoded in effect, since a UUID's characters all
pass Java's URL encoder unchanged.

### 12.3 One `REFERENCE` property

Derived from §8, and **not present in today's fixture**: `/oak:index/reference`
has `type = "reference"`, `reindex = false`, `reindexCount = 1` and **no hidden
child at all** —

```text
$ froe node <store> '/oak:index/reference'
property  jcr:primaryType <Name>    = "oak:QueryIndexDefinition"
property  info            <String>  = "Oak index for reference lookup."
property  type            <String>  = "reference"
property  reindex         <Boolean> = false
property  reindexCount    <Long>    = 1
```

no `:references`, no `:weakreferences`. That is §8.4 observed: Sling's own
content carries no `REFERENCE` or `WEAKREFERENCE` property outside the version
store, so `update` was never called for either name and neither child was
created. Interop task 0616 adds a `mix:referenceable` node and a sibling
holding a `REFERENCE` and a `WEAKREFERENCE` to it; by §8.1 the resulting
entries are `:references/<uuid>/content/interop/<sibling>/ref` and
`:weakreferences/<uuid>/content/interop/<sibling>/weakref` with `match = true`,
the path relative and the key the referenced `jcr:uuid` unencoded.

### 12.4 One counter hit

```text
$ froe node <store> '/oak:index/counter'
property  seed  <Long>   = -7610761686379641542
property  type  <String> = "counter"
property  async <String> = "async"
child     :index

$ froe node <store> '/oak:index/counter/:index'
property  :cnt <Long> = 10240
child     jcr:system
child     libs
```

No `resolution` property, so the default 1000 applies and the increment is
`bitMask + 1 = 1024` (§9.1). `10240 = 10 × 1024`, and by §9.3 every hit
increments every strict ancestor of the hit node, the root `:index` among
them: exactly ten *visible* nodes the `async` lane had indexed by shutdown
hashed to zero under the mask. `:cnt` and not `:count`, because
`oak.countHashed` is at its default.

Two reasons the number is not comparable with a node count of the store, and
why §10 is careful never to present it as one: the counter sees only what the
visible-editor wrap passes (§6.3), so no `:index` subtree and no other hidden
child is counted; and the definition sits on the `async` lane, so it reflects
the state that lane had reached, not the head.

The `seed` is stored as the full 64-bit value the creating run drew from
`UUID.randomUUID().getMostSignificantBits()`, and every later run — including
froe's rebuild — must use `(int) -7610761686379641542` sign-extended back to
64 bits, not the stored value (§9.1).

## 13. AEM safety invariants

1. **`:count_*` properties are never reproduced, only preserved or excused.**
   A rebuild writes none; a comparison excludes them by prefix. Writing a
   plausible-looking one would make froe's output differ from every Oak
   rebuild in a way no test could pin, and would corrupt the estimate of §10
   step 2 for a property index.
2. **The key level is not the content level.** Hidden-name filtering belongs
   below `:index/<key>`, never at it. Applying it at the key level silently
   drops every entry whose indexed value is the empty string (§7), which for a
   unique index means a `froe index check` that reports a missing entry Oak
   can see.
3. **`unique` is one strict `BOOLEAN` read, everywhere.** The editor, the
   lookup and the planner all agree; a reader that converted `"true"` would
   look for `entry` properties in a tree Oak built with `match` properties and
   report the whole index as missing.
4. **`seed` is narrowed to 32 bits on every run but the first.** A rebuild
   that hashed with the stored 64-bit value would place hits at different
   paths than the store already holds, so the rebuilt `:cnt` map would disagree
   with Oak's next incremental cycle — a divergence that grows silently.
5. **A reference definition's `includedPaths` bound nothing.** Deriving a work
   budget from them (§8.2) under-budgets a walk that covers the store; derive
   it from `/`.
6. **An absent hidden child is a legal state, not a defect.** `:weakreferences`
   (§8.4), a counter's `:index` (§9.5) and a counter mirror node without `:cnt`
   (§9.4) all occur in stores Oak itself wrote. Reporting any of them as
   corruption would make `froe index check` fail on a healthy store — and a
   check that cries wolf is a check an operator stops running.
7. **A covered node that no entry names is an observation, never a verdict.**
   The three entry-side faults are unambiguous, because each is the index
   contradicting content that is there to read: a *stale* entry names a node
   that does not exist, a *mismatched* entry names a node that does not carry
   the key, a *duplicate* is a unique key holding several paths, which Oak
   refuses commits on. A *missing* entry is not in that class — it may be an
   entry the index lost, or a node Oak never indexed.

   This is not a hypothetical. The generated Oak 1.90.0 fixture, written by
   Sling and never touched by froe, has **eighteen** of them under
   `/oak:index/nodetype`, in two clusters: the `indexRules` subtree of the
   `lucene` definition (nine `nt:unstructured` nodes, while the definition
   node `/oak:index/lucene` itself *is* indexed under
   `oak:QueryIndexDefinition`), and the `rep:permissionStore` nodes under
   `/jcr:system` (nine, `rep:PermissionStore` and `rep:Permissions`). The
   definition carries no `declaringNodeTypes`, no `valuePattern` and no path
   filter, so nothing in the definition excuses them, and nothing in
   `IndexUpdate`, `VisibleEditor` or `PropertyIndexEditor` excludes an index
   definition's own visible subtree or the permission store — each of those
   was read at the pinned commit and none of them filters here. **The
   mechanism is therefore not established; the observation is.** Oak's own
   tooling makes no claim either way: `IndexConsistencyCheckPrinter` adds
   every definition whose `type` is not `lucene` to `ignoredIndexes` and
   checks nothing about it, so the property family's consistency check is
   froe's own and has no Oak verdict to match.

   `PropertyIndexReport::has_definite_faults` is therefore what decides
   `froe index check`'s exit code, and `is_consistent` — which includes the
   missing set — is the strict all-clear a store Oak wrote can fail. Task
   0615 asks Oak itself, through the judge, which of the two explanations is
   right; until it answers, froe reports the count and says plainly that it
   cannot tell.
7. **A property index's `:index` is never absent** (§2.3). That asymmetry with
   invariant 6 is real and is the one case where absence *is* reportable.
8. **Never write a value froe cannot prove Oak would have written.** The
   version-store prefix quirk of §8.1 and the unreachable `builder.remove()`
   of §9.4 are reproduced as they are. "Fixing" either changes bytes on disk
   that a later Oak cycle compares against its own expectation.
