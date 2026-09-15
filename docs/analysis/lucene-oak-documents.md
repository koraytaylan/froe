# Oak's Lucene document model

From a node state to the exact set of Lucene fields Oak writes for it. The
formats those fields become are
[`lucene-4-7-codec.md`](lucene-4-7-codec.md); the analyzers that turn a
value into terms are [`lucene-oak-analysis.md`](lucene-oak-analysis.md).
This document is the layer between: which fields exist, what each one is,
and in what order.

Everything is read at commit `4984c4cf26a7ca58ae9ce12c63190b7f492bda78` of
`https://github.com/apache/jackrabbit-oak`, from

* `oak-search/…/plugins/index/search/` — the definition model
  (`IndexDefinition.java`, 2,116 lines at this commit),
  `PropertyDefinition.java`, `FieldNames.java`, `Aggregate.java`,
  `FulltextIndexConstants.java`, and under `spi/editor/` the shared
  `FulltextDocumentMaker.java` and `FulltextIndexEditor.java`;
* `oak-lucene/…/plugins/index/lucene/` — `LuceneIndexDefinition.java`,
  `LuceneDocumentMaker.java`, `FieldFactory.java`,
  `LuceneIndexConstants.java`, and under `writer/`
  `IndexWriterUtils.java`.

Methods are cited rather than files, because the definition model alone is
longer than this document.

### The facet artifact

Oak does not vendor Lucene's facet module, so §5's citations point at the
published artifact:

```
org.apache.lucene:lucene-facet:4.7.2:sources
https://repo1.maven.org/maven2/org/apache/lucene/lucene-facet/4.7.2/lucene-facet-4.7.2-sources.jar
sha256  e9de8b412a7fbae164cb9ebf64b6adab923a6d369865364724d808b3271a09a9
```

Obtained with `curl -sfL -O <the URL above>` and checksummed with
`sha256sum`, exactly as `lucene-oak-analysis.md` §0.2 pins the analyzers.

---

## 1. Which definitions froe reproduces

### 1.1 The codec verdict

`LuceneIndexDefinition.createCodec`:

```java
private Codec createCodec() {
    String mmp = System.getProperty("oak.lucene.compressing-codec");
    if (mmp != null) {
        return new CompressingCodec();
    }
    String codecName = getOptionalValue(definition, LuceneIndexConstants.CODEC_NAME, null);
    Codec codec = null;
    if (codecName != null) {
        …
        codec = Codec.forName(codecName);
    } else if (fullTextEnabled) {
        codec = new OakCodec();
    }
    return codec;
}
```

Four outcomes, in order:

1. **`oak.lucene.compressing-codec` set** — a `CompressingCodec`, whatever
   the definition says. This is a *consumer-JVM system property*, and froe
   cannot observe the JVM that will read the index. **A recorded
   departure**: froe writes as though it were unset, which is what every
   consumer that has not set it sees.
2. **an explicit `codec` property** — resolved by registered name.
3. **fulltext-enabled and no explicit codec** — `oakCodec`.
4. **otherwise** — `null`, which leaves Lucene's default, `Lucene46`.

**froe refuses by name any definition whose verdict is not `oakCodec`**,
because `oakCodec` is the composition plan 0009 writes. In practice that
means: fulltext-enabled, with no `codec` property naming something else.

"Fulltext-enabled" is `IndexingRule.fulltextEnabled`:

```java
this.fulltextEnabled = aggregate.hasNodeAggregates() || hasAnyFullTextEnabledProperty();
```

with

```java
public boolean fulltextEnabled() {
    return index && (analyzed || nodeScopeIndex);
}
```

— a rule with node aggregates, or one with a property definition that is
indexed and either `analyzed` or `nodeScopeIndex`. **`useInSuggest` and
`useInSpellcheck` are not in it**, and a definition whose only fulltext
intent is one of those two therefore gets `Lucene46` and is refused here.
The patterns count as well as the exact names: `hasAnyFullTextEnabledProperty`
walks `propDefinitions` and then `namePatterns`.

### 1.2 The old-format definition

A definition with no `indexRules` child is a version-1 definition, for
which `IndexDefinition` synthesizes rules from the flat
`includePropertyNames` and friends. froe **refuses it by name**: plan
0008's transport already declines to import one, and a version-1 index
names its analyzed fields without the `full:` prefix (§3.2), which is a
second format rather than a variation on this one.

---

## 2. Rules, and the property definition a name resolves to

### 2.1 Collecting the rules

`IndexDefinition.collectIndexRules`: the children of `indexRules` are read
through the `Tree` API, which yields them in **`:childOrder`** when that
property is present and in raw child order otherwise. Each child's name is
a node type, and the rule is registered under that type — and, when the
rule sets `inherited`, under **every registered node type that is a subtype
of it**:

```java
List<String> ntNames = allNames;
if (!rule.inherited) {
    ntNames = List.of(rule.getNodeTypeName());
}
for (String ntName : ntNames) {
    if (ntReg.isNodeType(ntName, rule.getNodeTypeName())) { … }
}
```

So `inherited` is not a query-time test: it expands the rule across the
node-type hierarchy when the definition is read.

### 2.2 Resolving a rule for a node

`IndexDefinition.getApplicableIndexingRule`:

```java
List<IndexingRule> rules = indexRules.get(getPrimaryTypeName(state));
IndexingRule rule = getApplicableIndexingRule(state, rules);
if (rule != null) return rule;
for (String name : getMixinTypeNames(state)) {
    rules = indexRules.get(name);
    rule = getApplicableIndexingRule(state, rules);
    if (rule != null) return rule;
}
return null;
```

**The primary type first, then the mixins**, and within each the first rule
whose own condition holds. A node no rule applies to contributes no
document at all.

### 2.3 Resolving a property name

`IndexingRule.getConfig`:

```java
PropertyDefinition config = propConfigs.get(propertyName);
if (config != null) {
    return config;
} else if (!namePatterns.isEmpty()) {
    if (NodeStateUtils.isHidden(propertyName)) {
        // hidden properties (eg. ":nodeName") do match the regex,
        // and we should probably ignore them;
        // but doing so would break "bug compatibility"
        // return null;
    }
    for (NamePattern np : namePatterns) {
        if (np.matches(propertyName)) {
            return np.getConfig();
        }
    }
}
return null;
```

Three facts, each load-bearing:

* **The exact map is case-insensitive.** `collectPropConfigs` builds
  `new TreeMap<>(String.CASE_INSENSITIVE_ORDER)`, so a definition naming
  `sling:resourceType` matches a property `sling:resourcetype`. The
  fixture's lower-cased names depend on it.
* **A duplicate leaves one definition, and which one turns on where the
  duplication is.** The loop guards with `!propDefns.containsKey(propName)`
  — the **child's** name — against a map keyed by each definition's
  **`name`**, and then `put`s under that `name`. So two children whose own
  names collide leave the first, because the second is never created; two
  children naming one property through `name` leave the second, because
  its `put` replaces the first; and a child whose name a previous
  definition's `name` already claimed is skipped whole, refusals included.
  The map's values are what `propDefinitions` holds, so an overwritten
  definition is gone from the rule — though it may already have joined one
  of the side lists the constructor fills.
* **A hidden name is tried against the patterns too.** The commented-out
  `return null` is Oak's own note that this is bug compatibility — and it
  is why the synthetic `:nodeName` reaches a catch-all pattern (§3.7).

### 2.4 How a pattern matches

`IndexDefinition.NamePattern`:

```java
if (FulltextIndexConstants.REGEX_ALL_PROPS.equals(pattern)) {
    this.parentPath = "";
    this.pattern = Pattern.compile(pattern);
} else {
    this.parentPath = getParentPath(pattern);
    this.pattern = Pattern.compile(PathUtils.getName(pattern));
}
…
boolean matches(String propertyPath) {
    String parentPath = getParentPath(propertyPath);
    if (!this.parentPath.equals(parentPath)) {
        return false;
    }
    String propertyName = PathUtils.getName(propertyPath);
    return pattern.matcher(propertyName).matches();
}
```

The pattern text is split at its **last `/`** by Oak's own parent-and-name
split, so a leading `/` yields the parent `/` — which never equals a
relative property path's parent, `""` — and matches nothing. That is why
Oak's own catch-all is special-cased:

```java
String REGEX_ALL_PROPS = "^[^\\/]*$";
```

which in the node state is the eight characters `^[^\/]*$`. For it, and
only for it, the parent is `""` and the whole text is the name expression.

The match is then a **parent-path equality** followed by a **whole-string**
regular-expression match — `Matcher.matches`, not `find`.

froe carries no regular-expression engine and will not approximate Java's,
so `documents/name_pattern.rs` implements a **bounded subset** — literals,
`.`, `*`, `+`, `?`, `|`, grouping, character classes with negation, ranges
and escapes, and `^` and `$` at the ends, where `Matcher.matches` makes
them redundant — and refuses everything else by name. That covers the
catch-all above and the patterns AEM's shipped definitions use,
`jcr:content/.*` and `.*Tags` among them; `\d`, `\w`, `{2,3}`, `(?i)`,
`(?:`, a back-reference, a look-around and a nested class are each a named
refusal, as is a quantifier over a quantified group, which is where a
backtracking matcher goes exponential. Task 1005's vectors evaluate a
table of patterns and property paths through this same `NamePattern`
logic inside the image, so every accepted pattern is proved against
Java's own engine and every refused one is proved to be refused rather
than misread.

Precedence, stated once: rule order and property-definition order are the
`:childOrder` of `indexRules` and `properties`, raw child order when that
property is absent; rule resolution tries the primary type before the
mixins and takes the first applicable rule; a name lookup takes the exact,
case-insensitive hit before any pattern; the first matching pattern wins.

### 2.5 The property definition's fields

`PropertyDefinition`, every field the document maker or the rule reads:
`index`, `propertyIndex`, `analyzed`, `nodeScopeIndex`, `ordered`,
`useInExcerpt`, `useInSuggest`, `useInSpellcheck`, `facet`,
`nullCheckEnabled`, `notNullCheckEnabled`, `boost`, `weight`, `type`,
`isRegexp`, `name` (with `relative` and its `ancestors`), `valuePattern`,
`excludeFromAggregation`, `sync`, `unique`, `function`, `dynamicBoost`,
`useInSimilarity`.

froe refuses by name a definition that uses `function`, `dynamicBoost`,
`useInSimilarity` or `similarityTags`: each adds a field branch this plan
does not port, and a silently missing field is a query that stops matching.

---

## 3. The fields, branch by branch

`FulltextDocumentMaker.makeDocument` is the whole of it, with
`LuceneDocumentMaker` supplying the Lucene half of each branch.

### 3.1 The field kinds

`FieldFactory` declares exactly two field types of its own:

```java
OAK_TYPE.setIndexed(true);
OAK_TYPE.setOmitNorms(true);
OAK_TYPE.setStored(true);
OAK_TYPE.setIndexOptions(DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS);
OAK_TYPE.setTokenized(true);

OAK_TYPE_NOT_STORED.setIndexed(true);
OAK_TYPE_NOT_STORED.setOmitNorms(true);
OAK_TYPE_NOT_STORED.setStored(false);
OAK_TYPE_NOT_STORED.setIndexOptions(DOCS_AND_FREQS_AND_POSITIONS);
OAK_TYPE_NOT_STORED.setTokenized(true);
```

**Both omit norms**, and the stored one carries offsets where the unstored
one does not. Lucene's own `TextField` and `StringField` supply the rest:
`TextField` is tokenized with norms and `DOCS_AND_FREQS_AND_POSITIONS`;
`StringField` is untokenized, `DOCS_ONLY`, norms omitted.

### 3.2 The table

| Field | Built by | Kind | Options | Stored | Norms | Value |
| --- | --- | --- | --- | --- | --- | --- |
| `:path` | `newPathField` | `StringField` | `DOCS_ONLY` | **yes** | no | the node's path |
| `full:<name>` | `newPropertyField(name, value, !skipTokenization(name), useInExcerpt)` | `OakTextField`, or `StringField` for a name §3.3 never tokenizes | offsets when stored, positions when not — `DOCS_ONLY` untokenized | `useInExcerpt`, and never when untokenized | **no** | one analyzed property value |
| `:fulltext` | `newFulltextField` | `TextField` | positions | no | **yes** | a `nodeScopeIndex` value, an aggregate value, or the node name |
| `fullnode:<path>` | `newFulltextField(path, value)` | `TextField` | positions | no | **yes** | a `relativeNode` aggregate's value |
| `:suggest` | `newSuggestField` | `OakTextField` unstored | positions | no | **no** | a `useInSuggest` value |
| `:spellcheck` | `newPropertyField(SPELLCHECK, value, true, false)` | `OakTextField` unstored | positions | no | **no** | a `useInSpellcheck` value |
| `:ancestors` | `newAncestorsField` | `TextField` | positions | no | **yes** | the **parent** path |
| `:depth` | `newDepthField` | `IntField` | `DOCS_ONLY` | no | no | the node's own depth |
| `<name>` (typed) | `indexTypedProperty` | `LongField`/`DoubleField`/`StringField` | `DOCS_ONLY` | no | no | one `propertyIndex` value |
| `:dv<name>` | `indexTypeOrderedFields` | doc values | — | — | — | §3.5 |
| `:nodeName` | `indexNodeName` | `StringField` | `DOCS_ONLY` | no | no | the name after its colon |
| `:nullProps` | `indexNullProperty` | `StringField` | `DOCS_ONLY` | no | no | the property's name |
| `:notNullProps` | `indexNotNullProperty` | `StringField` | `DOCS_ONLY` | no | no | the property's name |
| `<name>_facet` | the facet build pass | §5 | | | | |

**`:suggest` omits norms and carries positions without offsets**, which no
landed document stated: it is `newSuggestField`, and that is
`new OakTextField(FieldNames.SUGGEST, …, false)` — the unstored type.

The `full:` prefix is version-2 only:

```java
private String constructAnalyzedPropertyName(String pname) {
    if (definition.getVersion().isAtLeast(IndexFormatVersion.V2)) {
        return FieldNames.createAnalyzedFieldName(pname);
    }
    return pname;
}
```

with `ANALYZED_FIELD_PREFIX = "full:"` and
`FULLTEXT_RELATIVE_NODE = "fullnode:"`.

### 3.3 The per-property pass

`makeDocument` walks

```java
IterableUtils.chainedIterable(state.getProperties(), List.of(nodeNamePS))
```

— **the node's own properties in the order the node state yields them, and
then a synthetic `:nodeName` string property**. Per property:

```java
if (!isVisible(pname) && !FieldNames.NODE_NAME.equals(pname)) continue;
PropertyDefinition pd = indexingRule.getConfig(pname);
if (pd == null || !pd.index) continue;
if (pd.ordered) dirty |= addTypedOrderedFields(document, property, pname, pd);
var indexed = indexProperty(path, document, ctx, state, property, pname, pd);
…
facet |= pd.facet;
```

`isVisible` is `name.charAt(0) != ':'`, so a hidden property is skipped —
except `:nodeName`, which is the one the maker itself added.

**The ordered doc value comes first**, before the per-property pass. Then
`indexProperty`:

```java
if (pd.propertyIndex && pd.includePropertyType(property.getType().tag())) {
    dirty |= addTypedFields(doc, property, pname, pd);
}
…
if (pd.fulltextEnabled() && includeTypeForFullText) {
    for (String value : property.getValue(Type.STRINGS)) {
        if (definition.getPropertyRegex() != null && !definition.getPropertyRegex().matcher(value).find()) continue;
        if (!includePropertyValue(value, pd)) continue;
        if (pd.analyzed && pd.includePropertyType(property.getType().tag())) indexAnalyzedProperty(doc, pname, value, pd);
        if (pd.useInSuggest)     indexSuggestValue(doc, value);
        if (pd.useInSpellcheck)  indexSpellcheckValue(doc, value);
        if (pd.nodeScopeIndex)   { if (isFulltextValuePersistedAtNode(pd)) indexFulltextValue(doc, value); … }
        dirty = true;
    }
}
if (pd.facet && isFacetingEnabled()) dirty |= indexFacets(doc, property, pname, pd);
```

So per value, in this order: `full:<name>`, `:suggest`, `:spellcheck`,
`:fulltext` — then, once per property, the facet field.

A **binary** property never reaches that loop: it is diverted earlier to
§6's text extraction.

#### The names Oak never tokenizes

`indexAnalyzedProperty` does **not** always write an analyzed field:

```java
protected void indexAnalyzedProperty(Document doc, String pname, String value, PropertyDefinition pd) {
    String analyzedPropName = constructAnalyzedPropertyName(pname);
    doc.add(newPropertyField(analyzedPropName, value, !pd.skipTokenization(pname), pd.stored));
}

public static Field newPropertyField(String name, String value, boolean tokenized, boolean stored) {
    if (tokenized) {
        return new OakTextField(name, value, stored);
    }
    return new StringField(name, value, Field.Store.NO);
}
```

and

```java
public boolean skipTokenization(String propertyName) {
    if (isRegexp && IndexHelper.skipTokenization(propertyName)) {
        return true;
    }
    return !analyzed;
}

// IndexHelper
private static final Set<String> NOT_TOKENIZED = SetUtils.toSet("jcr:uuid");
static {
    NOT_TOKENIZED.addAll(UserConstants.USER_PROPERTY_NAMES);
    NOT_TOKENIZED.addAll(UserConstants.GROUP_PROPERTY_NAMES);
}
public static boolean skipTokenization(String name) { return NOT_TOKENIZED.contains(name); }
```

The set, read out of the pinned image rather than assembled from the
constant declarations by hand, is exactly:

```
jcr:uuid
rep:authorizableId
rep:disabled
rep:impersonators
rep:members
rep:password
rep:principalName
```

Three things follow, and each is observable in the fixture's own default
definition — whose one property definition is the catch-all pattern
`^[^\/]*$`, so **every** `jcr:uuid` in a Sling repository reaches it:

* the exclusion applies to a **regular-expression** definition alone. A
  definition that names `jcr:uuid` outright and marks it `analyzed`
  tokenizes it.
* the field written instead is a plain `StringField`: one untokenized
  `DOCS_ONLY` term of the whole value, norms omitted.
* `newPropertyField`'s `stored` argument is **ignored** on that arm, so
  such a field is unstored whatever `useInExcerpt` said — and carries no
  offsets, since it carries no positions.

The second arm, `!analyzed`, is unreachable from this caller: the call
site is already inside the `pd.analyzed` branch.

**No boost reaches this field.** `indexAnalyzedProperty` passes the value
and the two flags and nothing else, and both of `FieldFactory`'s types omit
norms — where Lucene's own `Field.setBoost` throws:

```java
public void setBoost(float boost) {
    if (boost != 1.0f && (type.indexed() == false || type.omitNorms())) {
        throw new IllegalArgumentException("You cannot set an index-time boost: this field is not indexed or omits norms");
    }
```

The one place a property definition's boost is applied at index time is the
aggregate value of §4, whose `TextField` keeps norms. Oak's own rebuild of
the interop fixture's `boost = 2.0` definition confirms it: it completes,
and a boosted `full:` field would have thrown.

### 3.4 The two inclusion tests

```java
protected boolean includePropertyValue(PropertyState property, int i, PropertyDefinition pd) {
    if (property.getType().tag() == PropertyType.BINARY) return true;
    if (pd.valuePattern.matchesAll()) return true;
    return includePropertyValue(property.getValue(Type.STRING, i), pd);
}

protected boolean includePropertyValue(String value, PropertyDefinition pd) {
    return pd.valuePattern.matches(value);
}
```

The **property form** stands in front of the `:dv` field and inside the
typed fields' per-value loop; the **bare form** stands inside the
per-property pass's per-value loop. They agree there — a binary never
reaches that loop, and a match-all pattern implies a match.

**The facet branch is not gated by either.** `indexFacets` is called after
the loop, on the property rather than per value, and reads nothing of the
value pattern. Oak's own rebuild of the interop fixture pins it: a
`valueExcludedPrefixes` of `beta` on a faceted `variantCategory` leaves the
typed field with the `alpha` values alone and still writes
`variantCategory_facet` — the drill-down terms and the sorted-set ordinal
— for every `beta` value.

The pattern is `valuePattern`, `valueIncludedPrefixes` and
`valueExcludedPrefixes`; the prefix forms are the ones plan 0006 already
supports and therefore the ones this plan reaches.

**The definition-level `valueRegex` is a different gate.** It is
`PROP_VALUE_REGEX` on the definition, compiled once, and it is applied with
`Matcher.find` — a *substring* match, not `matches` — and only inside the
per-property fulltext loop. It does not gate the `:dv` field or the typed
fields.

### 3.5 The ordered doc value

`LuceneDocumentMaker.indexTypeOrderedFields`, under `:dv<name>`
(`FieldNames.createDocValFieldName` is `":dv" + name`):

| Declared type | Doc value |
| --- | --- |
| `LONG` | `NumericDocValuesField(name, value)` |
| `DATE` | `NumericDocValuesField(name, FieldFactory.dateToLong(date))` — **the epoch millisecond** |
| `DOUBLE` | `DoubleDocValuesField(name, value)` — **the raw bits of the double** |
| `BOOLEAN` | `SortedDocValuesField(name, "true"/"false")` |
| `STRING` | `SortedDocValuesField(name, truncated bytes)` |

Four rules around them:

* **The type is the rule's declared type, not the property's.**
  `addTypedOrderedFields` overwrites the tag with `pd.getType()` when they
  differ. **So two rules typing one property differently produce one field
  name with two doc-value types**, and Lucene's writer refuses a type
  change — the fulltext editor catches the refusal *per document* and drops
  that document. froe refuses such a definition instead.
* **A multi-valued property is skipped with a warning**, ordered doc values
  being single-valued.
* **A duplicate is dropped, not overwritten**: `if (doc.getField(f.name()) == null)`.
* **A `STRING` doc value is truncated** at `STRING_PROPERTY_MAX_LENGTH`
  (32,766 bytes) before it becomes a `BytesRef`. A `propertyIndex` string
  is **not** truncated: it becomes an unanalyzed term of whatever length,
  and Lucene's own inversion then skips a term above its maximum length
  while keeping the document.

The double is the one to state twice: **the doc value is
`Double.doubleToLongBits` and the indexed term is
`NumericUtils.doubleToSortableLong`**, and the two differ for every
negative double, for `-0.0` and for `NaN`.

### 3.6 The typed fields

`indexTypedProperty`, one per value that passes the property-form test:

```java
if (tag == Type.LONG.tag())         f = new LongField(pname, …, Field.Store.NO);
else if (tag == Type.DATE.tag())    f = new LongField(pname, FieldFactory.dateToLong(date), Field.Store.NO);
else if (tag == Type.DOUBLE.tag())  f = new DoubleField(pname, …, Field.Store.NO);
else if (tag == Type.BOOLEAN.tag()) f = new StringField(pname, "true"/"false", Field.Store.NO);
else if (tag == Type.BINARY.tag())  f = null;   // never call getValue(Type.STRING) on a binary
else                                f = new StringField(pname, …, Field.Store.NO);
```

The field name is the **property's own name**, with no prefix.

### 3.7 The node name, twice

Two different fields carry the node name, and a third path carries it a
third time:

1. **The synthetic `:nodeName` property** goes through the ordinary
   per-property pass (§3.3). Under a catch-all pattern it therefore
   produces a `full::nodeName` field — the `full:` prefix on the name
   `:nodeName` — and, when that definition is `nodeScopeIndex`, a
   `:fulltext` value holding the node name.
2. **`addNodeNameField`**, when the rule sets `nodeNameIndexed`:

   ```java
   int colon = name.indexOf(':');
   String value = colon < 0 ? name : name.substring(colon + 1);
   indexNodeName(doc, value);
   ```

   — the name **after its first colon**, as an untokenized `:nodeName`
   term.
3. **The fulltext node name**, for a fulltext-enabled rule:

   ```java
   Pattern propertyRegex = definition.getPropertyRegex();
   boolean shouldAdd = propertyRegex == null || propertyRegex.matcher(nodeName).find();
   if (shouldAdd) indexFulltextValue(document, nodeName);
   ```

So under the fixture's default definition — whose pattern is the catch-all
of §2.4 — a node named `page` contributes `full::nodeName` holding
`:nodeName`'s value and **two** `:fulltext` values of the node name: one
from the per-property pass and one from this branch.

### 3.8 The markers, the augmentation and the ancestors

After the aggregates:

```java
dirty |= indexNullCheckEnabledProps(path, document, state);
dirty |= indexFunctionRestrictions(path, document, state);
dirty |= indexNotNullCheckEnabledProps(path, document, state);
…
dirty |= indexTopDynamicBoost(document, ctx.collectedBoosts, maxDynamicBoostCount);
dirty |= augmentCustomFields(path, document, state);
```

* `:nullProps` gets the **name** of every `nullCheckEnabled` property the
  node lacks; `:notNullProps` the name of every `notNullCheckEnabled`
  property it has. Both are untokenized `StringField`s.
* **`augmentCustomFields` calls a consumer-registered field provider.**
  froe reproduces none: an AEM deployment that registers one gets fields
  froe cannot know about. **This is an AEM safety invariant** — §9 states
  it again — and the reason a froe-built index is equivalent to Oak's only
  for a deployment with no registered augmentor.

Then, last of all:

```java
if (definition.evaluatePathRestrictions()) {
    indexAncestors(document, path);
}
return finalizeDoc(document, dirty, facet);
```

`indexAncestors` adds `:ancestors` over `PathUtils.getParentPath(path)` and
`:depth` over the node's own path.

### 3.9 Finalization

`LuceneDocumentMaker.finalizeDoc`:

```java
if (facet && isFacetingEnabled()) {
    doc = getFacetsConfig().build(doc);
}
…
// because of LUCENE-5833 we have to merge the suggest fields into a single one
Field suggestField = null;
for (IndexableField f : fields) {
    if (FieldNames.SUGGEST.equals(f.name())) { … merge … }
}
doc.removeFields(FieldNames.SUGGEST);
if (suggestField != null) doc.add(suggestField);
```

Two re-orderings, both of which change the field sequence a writer sees:

* **the facet build pass re-emits the whole document** — §5 — putting the
  facet-derived fields first and the rest after, in their original order;
* **every `:suggest` field is removed and one merged field is added at the
  end**, so `:suggest` is always the document's last field.

### 3.10 The order, stated once

The order fixes positions, offsets and the stored-field sequence, so it is
the whole of what a reproduction must match:

1. `:path`;
2. per property, in the node state's own order, then the synthetic
   `:nodeName`: the ordered doc value, then the typed fields, then per
   value `full:<name>`, `:suggest`, `:spellcheck`, `:fulltext`, then the
   facet field;
3. the aggregates, whose values arrive in each aggregated node's property
   order;
4. `:nullProps`, the function restrictions, `:notNullProps`;
5. the dynamic boost and the consumer-registered augmentation;
6. `:nodeName`;
7. the node name's `:fulltext` value;
8. `:ancestors` and `:depth`;
9. the facet build pass re-emits everything, facet fields first;
10. the merged `:suggest`, last.

A `:fulltext` field therefore holds property values, then aggregate values,
then the node name — and same-name fields are inverted in that order, each
continuing from the previous field's end state as `lucene-oak-analysis.md`
§7 describes.

---

## 4. Aggregates

`Aggregate.java`. An `aggregates/<type>/include*` child declares a `path`
of `/`-separated steps:

```java
public static final String MATCH_ALL = "*";
…
String element = elements[depth];
if (MATCH_ALL.equals(element)) { … }
```

* each step is either `*` or a literal child name — **there is no
  double-star step**, and a non-`*` element is compared literally;
* `maxDepth()` is the number of steps;
* `primaryType` is enforced **on the last step only**:

  ```java
  //As per JR2 the primaryType is enforced on last element
  if (depth == maxDepth() - 1 && primaryType != null && !matchingType(primaryType, nodeState))
  ```

* `relativeNode` changes the field a matched node's text lands in, from
  `:fulltext` to `fullnode:<include path>`:

  ```java
  Field field = result.isRelativeNode() ?
      newFulltextField(result.rootIncludePath, value) : newFulltextField(value);
  if (pd != null) field.setBoost(pd.boost);
  ```

* `reaggregateLimit` bounds how deep a re-aggregation walks.

The matcher is a small state machine over the include's elements, and an
aggregated property contributes through `Aggregate.PropertyInclude` with
the same property-definition rules as §2.5.

### 4.1 `relativeNode` writes **both** fields

The ternary above reads as an either/or, and it is not one: Oak's own
rebuild of a definition whose aggregate names `jcr:content` twice — once
plainly, once with `relativeNode` — writes that child's values into the
page's `:fulltext` **twice** and into `fullnode:jcr:content` once. A second
relative include over a path no other include names writes its child's
values into `:fulltext` as well as into its own `fullnode:` field. So a
relative include contributes to `:fulltext` *beside* its own field, and a
node named by two includes is aggregated once per include.

This is pinned by the oracle rather than by a line of Java: the interop
suite's `lucene_reindex` phase carries both shapes, and froe reproducing
only the `fullnode:` half made Oak's own rebuild differ on the page
document's `:fulltext` positions.

### 4.1.1 Whose rule decides what is excluded

`indexAggregatedNode` resolves the rule covering **the aggregated node** —
`definition.getApplicableIndexingRule(result.nodeState)` — and asks *that*
rule whether a property is `excludeFromAggregation`, not the rule the
document is being made under. Oak's own rebuild pins it: a `meta` child of
type `sling:Folder`, aggregated into an `nt:unstructured` page, loses its
`jcr:title` because the definition's **`sling:Folder`** rule excludes it,
while the `nt:unstructured` rule's definition of the same name does not.

A node no rule covers is aggregated whole.

### 4.1.2 Re-aggregation

An aggregated node whose covering rule declares an aggregate of its own
contributes that aggregate's nodes too — into the **same fields**, a
relative include's `fullnode:<path>` included, and after its own
properties. `reaggregateLimit`, five by default, is how many levels deep
that goes.

Oak's own rebuild pins the field set: the `meta` child above is reached by
a `relativeNode` include, its rule declares `include0 = inner`, and the
grandchild's values appear in the page's `:fulltext` **and** in
`fullnode:meta`.

What is **not** carried up is the re-aggregated rule's own property
includes: `full:inner/jcr:title` appears on the `meta` node's own document
and on no page's.

### 4.2 Property includes: the relative property definitions

A **relative** property definition — a `name` holding a `/`, which is the
shape AEM's own definitions are written in — reaches nothing through
`IndexingRule.getConfig`: that is asked about a node's own property name,
whose parent path is `""`, and §2.4's match begins with a parent-path
equality. Its only path into a document is the aggregate walk:

```java
List<Aggregate.Include> propIncludes = newArrayList();
for (PropertyDefinition pd : propConfigs.values()) {
    if (pd.relative) propIncludes.add(new Aggregate.PropertyInclude(pd));
}
…
includes.addAll(propAggregate.getIncludes());
if (nodeAggregate != null) includes.addAll(nodeAggregate.getIncludes());
```

A `PropertyInclude`'s elements are the definition's **ancestors**; the last
name is the property. At the node its ancestor path ends on, an exact
definition takes the property of that name and a regular-expression one
takes every property whose own name the name expression matches. Each
result is then indexed exactly as a node's own property is, under the
**relative path** as the field name:

```java
public void onResult(PropertyIncludeResult result) {
    if (result.pd.ordered) addTypedOrderedFields(fields, result.propertyState, result.propertyPath, result.pd);
    indexProperty(path, fields, state, result.propertyState, result.propertyPath, result.pd);
}
```

Three things follow, and Oak's own rebuild of the interop fixture pins each
of them — a `jcr:content/jcr:title` definition with `ordered` and a
`jcr:content/.*` pattern over a node carrying a hidden `:childOrder`:

* the fields are `:dv<relative path>`, `<relative path>` and
  `full:<relative path>`, in that order;
* `propertyPath` is the definition's **own** parent path joined with the
  property's **own** name, which differ for a pattern;
* a **hidden** name contributes nothing here, where §2.3's per-property
  pass lets one through to the patterns as bug compatibility;
* the node state `indexProperty` is given is the **document's own** node,
  so a binary reached this way is gated by the root's `jcr:mimeType`
  rather than by its own node's.

Because the combined list puts the property includes first, a child that
ends both kinds contributes its property-include fields before its
aggregated values.

---

## 5. Facets

### 5.1 What the document maker adds

`LuceneDocumentMaker.indexFacetProperty`:

```java
String facetFieldName = FieldNames.createFacetFieldName(pname);   // pname + "_facet"
getFacetsConfig().setIndexFieldName(pname, facetFieldName);
if (tag == Type.STRINGS.tag() && property.isArray()) {
    getFacetsConfig().setMultiValued(pname, true);
    for (String value : property.getValue(Type.STRINGS)) {
        if (value != null && !value.isEmpty()) doc.add(new SortedSetDocValuesFacetField(pname, value));
    }
} else if (tag == Type.STRING.tag()) {
    String value = property.getValue(Type.STRING);
    if (!value.isEmpty()) doc.add(new SortedSetDocValuesFacetField(pname, value));
}
```

An empty value is skipped rather than refused; the facet field's *index*
field name is `<property>_facet`.

**Both arms test the type tag**, and `Type.STRINGS.tag()` *is*
`Type.STRING.tag()` — an array shares its scalar's tag — so the pair reads
"a string array, else a string". A faceted property of any other type adds
**no facet field at all**, while `getFacetsConfig()` is still consulted for
it, so §5.3's `facets` node still appears. Oak's own rebuild of a faceted
`LONG` in the interop fixture writes no `_facet` field for it.

### 5.2 What the build pass turns them into

`FacetsConfig.processSSDVFacetFields`:

```java
FacetLabel cp = new FacetLabel(facetField.dim, facetField.label);
String fullPath = pathToString(cp.components, cp.length);
// For facet counts:
doc.add(new SortedSetDocValuesField(indexFieldName, new BytesRef(fullPath)));
// For drill-down:
doc.add(new StringField(indexFieldName, fullPath, Field.Store.NO));
doc.add(new StringField(indexFieldName, facetField.dim, Field.Store.NO));
```

**Three fields per facet value**, all under `<property>_facet`: one
sorted-set doc value for counting and two unstored drill-down terms — the
escaped full path and the bare dimension.

The path encoding, `FacetsConfig.pathToString`. The source declares the two
characters as literals; they are written here as their Java escapes, since
a control character in a specification is a character nobody can review:

```java
private static final char DELIM_CHAR = '\u001F';
private static final char ESCAPE_CHAR = '\u001E';
…
if (s.length() == 0) throw new IllegalArgumentException("each path component must have length > 0 (got: \"\")");
…
if (ch == DELIM_CHAR || ch == ESCAPE_CHAR) sb.append(ESCAPE_CHAR);
sb.append(ch);
…
sb.append(DELIM_CHAR);
…
sb.setLength(sb.length()-1);   // trim the last delimiter
```

Components joined with **U+001F**, a U+001F or U+001E inside a component
escaped with a **U+001E** prefix, and an **empty component refused** with
an `IllegalArgumentException`.

### 5.3 The configuration that persists

Oak's facet configuration is node-state-backed, so it writes into the
**visible** definition — but in two places only, and `setIndexFieldName` is
not one of them.

`NodeStateFacetsConfig`'s **constructor** is the first:

```java
this.nodeBuilder = nodeBuilder.child(FulltextIndexConstants.PROP_FACETS);
if (!nodeBuilder.hasProperty(JCR_PRIMARYTYPE)) {
    nodeBuilder.setProperty(JCR_PRIMARYTYPE, NT_UNSTRUCTURED, Type.NAME);
}
readMVFacets(nodeBuilder, "");
```

So a `facets` child with `jcr:primaryType = nt:unstructured` as a `NAME`
exists as soon as one facet property made the maker consult the
configuration — the configuration is consulted for every facet property,
whatever its arity.

Its `setMultiValued` override is the second, and it writes **only when the
value is true**:

```java
public synchronized void setMultiValued(String dimName, boolean v) {
    super.setMultiValued(dimName, v);
    if (v) {
        NodeBuilder builder = nodeBuilder;
        for (String element : PathUtils.elements(dimName)) {
            NodeBuilder child = builder.child(element);
            if (!child.hasProperty(JCR_PRIMARYTYPE)) {
                child.setProperty(JCR_PRIMARYTYPE, NT_UNSTRUCTURED, Type.NAME);
            }
            child.setProperty(MULTIVALUED, Boolean.valueOf(true));
            builder = child;
        }
    }
}
```

One child per path element of the dimension, from the `facets` node
downward, each with the same primary type when it has none and each —
not the last alone — carrying `multivalued = true` as a `BOOLEAN`.
`setIndexFieldName` is not overridden and persists nothing, so a
**single-valued** dimension leaves a `facets` node with no child under it
at all. Nothing on the query side reads any of it back.

---

## 6. Binaries

A binary property with `includeTypeForFullText` goes to

```java
List<String> binaryValues = newBinary(property, state, path + "@" + pname);
addBinary(doc, null, binaryValues);
```

and `addBinary` adds each extracted string as a **stored** `:fulltext`
value — or `fullnode:<include path>` for a `relativeNode` aggregate:

```java
if (path != null) doc.add(newFulltextField(path, binaryValue, true));
else              doc.add(newFulltextField(binaryValue, true));
```

**Stored**, unlike a `nodeScopeIndex` property value, which
`indexFulltextValue` adds unstored. Plan 0010's extraction marker takes the
same stored slot.

Oak's own extraction: nothing without `jcr:mimeType`, nothing for a type
outside the Tika-supported set, the `TextExtractionError` marker only from
the exception branches, and an empty extraction stored as the empty string.
The pre-extracted text provider reads a store whose layout, `stripLength`
and `maxExtractLength` are the text store's own.

**An inline segment blob cannot be pre-extracted**: the pre-extracted store
is keyed by the blob's content identity, which an inlined value does not
have.

---

## 7. Index-time bookkeeping

`FulltextIndexEditorContext` around the writer: the definition it builds
and the `refresh` flag that build consumes; the reindex mode it enters,
which writes the `:index-definition` clone and the `:version` property;
and, when the writer closes, the `:status` node with a fresh `uid`,
`lastUpdated`, `indexedNodes` and `reindexCompletionTimestamp`. The
suggester is rebuilt on its own schedule and not as part of a document.

Plan 0008's import already reproduces the `:status`, `:version` and
`:index-definition` bookkeeping; this plan reuses it rather than restating
it.

---

## 8. Worked example

One `nt:file` node at `/content/interop/doc` with a `jcr:content` child,
under the fixture's default definition — `indexRules/nt:base` with a
catch-all property definition that is `analyzed` and `nodeScopeIndex`,
`evaluatePathRestrictions` on, and an aggregate for `nt:file` including
`jcr:content`.

The node's own properties, in node-state order, are
`jcr:primaryType = nt:file` and `jcr:created`.

| # | Field | Kind | Value |
| --- | --- | --- | --- |
| 1 | `:path` | `StringField`, stored, `DOCS_ONLY` | `/content/interop/doc` |
| 2 | `full:jcr:primaryType` | `OakTextField` unstored, no norms | `nt:file` |
| 3 | `:fulltext` | `TextField`, norms | `nt:file` |
| 4 | `full:jcr:created` | `OakTextField` unstored, no norms | the date's string form |
| 5 | `:fulltext` | `TextField`, norms | the date's string form |
| 6 | `full::nodeName` | `OakTextField` unstored, no norms | `doc` |
| 7 | `:fulltext` | `TextField`, norms | `doc` |
| 8 | `:fulltext` | `TextField`, **stored**, norms | the extracted text of `jcr:content/jcr:data` |
| 9 | `:fulltext` | `TextField`, norms | `doc` — the node-name branch of §3.7 |
| 10 | `:ancestors` | `TextField`, norms | `/content/interop` |
| 11 | `:depth` | `IntField`, `DOCS_ONLY` | 3 |

Rows 6 and 7 are the synthetic `:nodeName` reaching the catch-all pattern
through §2.3's bug compatibility; row 9 is the separate fulltext node-name
branch. **The node name is in `:fulltext` twice**, at two different
positions, and a reproduction that adds it once writes a different index.

There is no `:nodeName` field: the rule does not set `nodeNameIndexed`.
There is no `:dv` field: nothing is `ordered`. There is no `:suggest` or
`:spellcheck`: nothing sets them. There is no typed field: nothing is
`propertyIndex`.

---

## 9. AEM safety invariants

* **froe reproduces no consumer-registered field provider** (§3.8). A
  deployment that registers one has fields froe cannot write, and its index
  is not equivalent to Oak's.
* **froe writes as though `oak.lucene.compressing-codec` were unset**
  (§1.1), because it is a property of the JVM that will *read* the index.
* **A definition whose codec verdict is not `oakCodec` is refused by
  name**, not approximated with `Lucene46`.
* **Two rules typing one property differently are refused** (§3.5), because
  Oak's own writer drops every document that reaches the second type.
* **A `propertyIndex` string is not truncated** (§3.5): Lucene's inversion
  skips the over-long term and keeps the document, and froe does the same.
* **The node name reaches `:fulltext` twice** under a catch-all pattern
  (§3.7). Both are part of the index.
* **A document left holding nothing but `:path`** — every value excluded by
  a pattern, no aggregate, no marker — is one Oak's own writer drops
  through `makeDocument` returning `null` under
  `!indexingRule.indexesAllNodesOfMatchingType() && !dirty`. Task 1005
  refuses such a definition by name rather than writing a document Oak
  would not have written.
