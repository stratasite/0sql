# 0sql and Apache Ossie: what each can say, and a plan to meet

An analysis and a proposed plan, 2026-10-18, not yet the plan of record (that is `docs/introspection.md`). The question asked: is 0sql's
semantic model a superset of what Apache Ossie (incubating, formerly
Open Semantic Interchange) can express, a subset, or something else;
what gaps should 0sql fill first; and how should it integrate with
Ossie over time. The execution engine investigation in
`docs/execution-engine.md` is paused in favour of this.

## Ossie, as it actually stands

Most writing about Ossie is a release behind, so the facts first, each
checked against the spec repository on the day.

- The released specification is **0.1.1**, dated 2025-12-11 in its
  own version history, publicly launched on 2026-01-27 by Snowflake,
  Salesforce and partners. There is no v1.0. The repository's `main`
  is **0.2.0.dev0**, marked a draft whose schema may change.
- The project entered the Apache Incubator in June 2026 and announced
  it on 2026-07-10. It has made **no Apache release** yet; the
  incubator's status page records the podling as lacking signing keys.
  Spec changes need a seven-day discussion and a vote with three
  binding +1s.
- Two shapes therefore exist. 0.1.1 wraps an array of models under
  `semantic_model` with root `dialects` and `vendors` lists; 0.2.0.dev0
  is one model per document with the eight keys at the root. The object
  model underneath is otherwise the same, plus a `datatype` enum and an
  `OSSIE_SQL_2026` dialect in the draft.
- Product-native support exists in exactly two places: Snowflake's
  `SYSTEM$CREATE_SEMANTIC_VIEW_FROM_OSSIE_YAML` and
  `SYSTEM$READ_OSSIE_YAML_FROM_SEMANTIC_VIEW` (0.1.1 only), and Cube's
  import and export shipped on 2026-10-16. Twenty converters live in
  the repository (dbt, Databricks Metric Views, GoodData, Salesforce,
  Polaris, Sigma, ThoughtSpot, Power BI, Omni, Hex and others), each
  with a published list of what it drops.
- Every object in the JSON Schema is `additionalProperties: false`
  except the `ai_context` object. Anything a vendor needs beyond the
  core goes into `custom_extensions: [{vendor_name, data}]`, where
  `data` is a JSON-encoded string.

### The whole core model

```yaml
version: "0.2.0.dev0"             # or "0.1.1" with the wrapper
name:                             # the only identifier at every level
description:
ai_context:                       # string, or {instructions, synonyms, examples, ...}
datasets:
  - name:
    source:                       # "database.schema.table" or a query, free string
    primary_key: [col, ...]       # single or composite
    unique_keys: [[col, ...]]
    description:
    ai_context:
    fields:
      - name:
        expression:
          dialects: [{dialect: SNOWFLAKE|ANSI_SQL|OSSIE_SQL_2026|..., expression: "..."}]
        dimension: {is_time: bool} # present = a dimension; absent = a fact column
        label:                    # "for categorization"
        description:
        datatype:                 # String Integer Decimal Float Boolean Date Time DateTime DateTimeTz Opaque
        ai_context:
        custom_extensions: [{vendor_name, data}]
    custom_extensions:
relationships:
  - name:
    from:                         # the many side
    to:                           # the one side
    from_columns: [..]
    to_columns: [..]              # positional, same length
    ai_context:
    custom_extensions:
metrics:
  - name:
    expression: {dialects: [...]} # aggregate SQL over dataset.field references
    description:
    datatype:
    ai_context:
    custom_extensions:
custom_extensions:
```

Fields are scalar: "use scalar SQL expressions (no aggregations)". A
field without the `dimension` key is a fact column. Aggregation lives
only in model-level `metrics`, whose expression is "full SQL with
aggregate functions" over `dataset.field` names and may span datasets.
Relationships are foreign-key equi-joins, many-to-one or one-to-one by
inference from `primary_key`; there is no cardinality key, no join
type, no arbitrary condition. The only dialect whose semantics the spec
defines is `OSSIE_SQL_2026`, an ANSI SQL:2003 subset with no SELECT,
WHERE, GROUP BY or subqueries; every other dialect value is opaque and
"implementations MAY ignore them". That is the entire model. There is
also a separate Ontology document type (concepts, verbalised
relationships) with a mapping document that binds concepts to a
semantic model; it is young and out of scope here.

## 0sql, in the same frame

What a 0sql project carries, from the loader (`crates/project/src/lib.rs`)
and the snapshot (`crates/semantic/src/snapshot.rs`):

- **project.yml**: name, uid, production_branch. The `zsql init`
  template also writes a `description:` key, which the docs record as
  "written by the template, ignored by the loader"; the snapshot's
  project record has no field for it and nothing serves it.
- **datasources.yml**: per key: name, adapter (one of 13 dialects),
  tier (hot, warm, cold), description, query_timeout,
  extra_query_params.
- **tbl.*.yml**: name, physical_name, datasource, cost, description,
  tags, snapshot (the date dimension a snapshot fact is snapped by),
  partitions (dimension, predicate, filter_value, filter_value_end),
  imports, fields. A `metadata` key is accepted and ignored.
- **field**: type (dimension or measure), name, description,
  data_type (string, integer, decimal, date, date_time, boolean,
  bigint, binary), hidden, tags, grains, format, synonyms,
  enable_sampling, sample_size, sample_sql (sampling; `display_type`,
  `secure` and the listing keys left on 2026-10-21), snapshot (ending or
  beginning), exclusion_type and
  exclusions (level-of-detail exclude rules with filter modes),
  inclusions (level-of-detail include rules with an outer
  aggregation), extended_blend_group, expression {sql, array, lookup,
  primary_key}. A measure's expression carries its own aggregate; a
  compound measure references other fields as `[Name]`.
- **rel.*.yml**: left, right, sql (any condition over `left.` and
  `right.`), cardinality (one_to_many, many_to_one, one_to_one), join
  (inner, left, right), allow_measure_expansion, description.
- **security.yml**: policies with mode (filter_data, mask_data),
  mask_value, triggers (field_tags, field_names), context_dimension,
  permission_resolution (source, value_from, tag_key, unresolved),
  bypass.
- **tests/*.yml**: planner assertions.

And what the planner does with it that no file states: universe
formation, fan-out-safe blending across facts at different grains,
routing by tier, cost and partition, refusal with nearest fields.

## The comparison

### Superset, subset, or something else

Something else, and the shape of the difference is clean enough to
state in one sentence: **0sql is a superset of Ossie on everything that
decides what SQL comes back, and a subset of Ossie on the metadata that
travels between tools.** Ossie's core is a lossy projection of a 0sql
model; a 0sql project is Ossie's core plus a planner's worth of
semantics that Ossie's own roadmap lists as open work.

Concept by concept, in three groups.

### Both have it

| Concept | Ossie | 0sql | Mapping notes |
|---|---|---|---|
| Model name, description | `name`, `description` | project.yml `name`; a `description` key the template writes and the loader ignores | 0sql needs to load and serve the description it already asks for |
| Dataset | `datasets[].name`, `source` | table `name`, `physical_name` | `source` as a query: 0sql interpolates `physical_name` raw into FROM, so `(select ...)` works today by accident, not by contract |
| Dimension | field with `dimension` | `type: dimension` | direct |
| Fact column | field without `dimension` | no equivalent as a field; the column appears inside a measure's `sum(col)` | import: a fact column becomes a hidden dimension, or nothing, until a metric uses it |
| Time role | `dimension.is_time` | `data_type: date/date_time` plus `grains` | an `Integer` or `String` field flagged `is_time` has no 0sql form |
| Expression | `expression.dialects[]`, several per field | one `sql` per table, in the datasource's dialect | import picks the dialect matching the adapter, else `ANSI_SQL` or `OSSIE_SQL_2026`; export emits the adapter's dialect where Ossie names it (SNOWFLAKE, DATABRICKS, BIGQUERY), `ANSI_SQL` when the expression is portable, and the 0sql dialect in an extension otherwise |
| Data type | 10 values | 8 values | Ossie has Time, DateTimeTz, Float, Opaque that 0sql lacks; 0sql has bigint and binary that Ossie folds into Integer and Opaque |
| Description | everywhere | datasource, table, field, join | 0sql lacks it on the project and on policies |
| Synonyms | `ai_context.synonyms` on every object | `synonyms` on fields only | 0sql needs them on tables, and arguably on datasources |
| Categorisation | `label` on a field | `tags` on fields and tables | direct, many-to-one |
| Primary key | `primary_key: [..]`, composite | `expression.primary_key: true` on one dimension | 0sql has no composite key and no key at table level |
| Relationship | `from` (many), `to` (one), column lists | join with `sql`, `cardinality`, `join` type | Ossie to 0sql is lossless (many_to_one, inner, equi-join); 0sql to Ossie loses join type and any non-equi or compound condition |
| Metric | model-level aggregate SQL over `dataset.field` | table-bound measure with its aggregate, or compound measure over `[Name]` references | the load-bearing mapping, below |
| Derived metric | identifier references in metric expressions, semantics undefined | compound measures, chained, cycle-checked | 0sql is the stricter, defined form |
| AI examples | `ai_context.examples` | none | 0sql needs this; it is what the MCP evaluation found instructions and examples do most for |
| AI instructions | `ai_context.instructions` at every level | none | same |
| Extensions | `custom_extensions` | none; unknown keys are warned and dropped | 0sql needs a preserved passthrough |

### 0sql has it, Ossie does not

Each of these is either absent from Ossie's core or an open roadmap
item there. On export they go into a `custom_extensions` entry with
`vendor_name: "0SQL"`; on import from another vendor's document they
are simply not present, and the planner falls back to its defaults.

| 0sql concept | Ossie status |
|---|---|
| Datasources, adapters, tiers, multi-datasource projects | none; `source` is a free string, one model has no notion of where it runs |
| Table cost, partitions, routing | none; Databricks and Cube stash materialisation and pre-aggregations |
| Table-level measures with their own aggregate | open: dataset-level measures (#29), `aggregation_method` (#19) |
| Join cardinality and join type | open: explicit cardinality (#50), inner join (#11); every shipped converter is working around it |
| Non-equi and compound join conditions | none; Snowflake drops ASOF and RANGE joins, Omni stashes non-equi joins |
| Fan-out safety, grain | open: entity/grain (#12); Cube emits `FANOUT_UNSAFE_METRIC` |
| Level of detail: exclusions with filter modes, inclusions with an outer aggregation | none; nothing in the roadmap names it |
| Snapshot (semi-additive) measures | none; Snowflake and Hex stash non-additive dimensions |
| Extended blend groups (role-playing dates aligned across facts) | none |
| Row-level security and masking policies | none; ThoughtSpot's converter marks RLS as unrepresentable and security-bearing; proposals #87, #58, #59 |
| Segments, named filters, default filters | open and contested: semantic filters (#5); maintainer favours a minimal spec |
| Format, hidden, value sampling | none; extended metadata PR #287 proposes `display_format` and `default_aggregation` |
| Grains on a time dimension | none; dbt's converter assigns every time dimension DAY |
| Period-over-period, windows, contribution | query-time decorators in 0sql; Ossie permits `OVER()` in strings but has no grain or window declaration, and dbt's converter reports `CUMULATIVE_SEMANTICS_LOSS` |
| Stable uids, branches, deploy history | open: stable ids (#31), ownership (#13, #53) |
| Planner tests | none |

### Ossie has it, 0sql does not

These are the gaps to fill, and every one of them is worth having
whether or not Ossie existed.

| Ossie concept | What 0sql would add |
|---|---|
| Composite primary keys and unique keys on a dataset | Nothing in the model. 0sql asks the modeller for cardinality and join type directly, which says more than a key does, so inferring from keys would only confirm what was declared. Keys are interchange data: carried through in `extensions`, derived from the join condition on export. The single-column `primary_key: true` on a field stays as it is |
| `ai_context` on model, dataset, field, relationship, metric: instructions, synonyms, examples | `instructions` and `examples` on project, table and field; `synonyms` on tables; `description` on the project. These feed `/mcp` and `fields` directly, where the evaluation harness showed instruction text is the lever |
| `datatype` Time, DateTimeTz, Float, Opaque | `time`, `date_time_tz` (or a tz flag on date_time), `float`; `binary` already stands for opaque |
| Model-level metrics not tied to a dataset | Nothing, deliberately. A 0sql compound measure's host table is the declaration of the finest grain it is answerable at, since the host fixes the universes the measure is reachable from. Ossie has no grain concept, which is why its metrics float free and why its roadmap lists entity and grain as open. The importer must choose a host (below); the exporter records it in the extension |
| `source` as a query | a separate table key holding a SELECT, with `physical_name` optional when it is present. Today a parenthesised query in `physical_name` happens to work because the loader interpolates it verbatim into FROM, but nothing promises it. Design notes below |
| Several dialects per expression | not needed as a model feature, since the datasource fixes the dialect; needed only in the importer's dialect choice |
| `custom_extensions` round-trip | an `extensions` (or the already-accepted `metadata`) key on datasource, table, field, join and policy, preserved into the snapshot and returned by the API, so a document that passed through 0sql loses nothing another tool put there |
| `is_time` on a non-date field | low value; an integer year can be a dimension with `grains` only if 0sql relaxes the date requirement. Defer |
| Ontology and mapping documents | out of scope until the ontology spec stabilises |

### The metric mapping, which decides whether import is lossless

Ossie's metric is `SUM(orders.amount) / COUNT(DISTINCT customers.id)`:
model-level, aggregates inline, possibly spanning datasets. 0sql's is
two table-bound measures and a compound measure. The import algorithm:

1. Parse the metric expression. For each aggregate call whose arguments
   reference columns of one dataset, synthesise a measure on that
   table: name derived from the metric and the aggregate
   (`Orders Amount Sum`), hidden, expression the aggregate call with
   the dataset qualifier stripped.
2. Replace each aggregate call in the formula with `[that measure]`.
   The remainder is a compound measure carrying the metric's name,
   description, datatype and ai_context.
3. A metric that is a single aggregate over one dataset collapses to
   one visible measure on that table, no compound needed.
4. Choose the compound measure's host table. In 0sql the host is the
   grain declaration: it fixes which universes reach the measure and so
   the finest grain it answers at. Ossie carries no such thing, so the
   importer decides and the loss report says what it decided. When the
   components sit on one dataset, that dataset. When they sit on
   several datasets joined along one path, the many-side dataset, which
   is the finest grain the formula is safe at. When they sit on
   unrelated facts, there is no right answer; refuse that metric by
   name and ask the modeller to place it.
5. A metric whose aggregate mixes columns of two datasets inside one
   call (`SUM(a.x * b.y)`) is a row-level cross-dataset aggregate. 0sql
   can express it only if a join exists; emit it on the many-side
   table with the join's column, or refuse with the reason.

Steps 4 and 5 are where import needs a human or is lossy; step 5 is
the case Ossie's own converters flag as fan-out-unsafe. Everything
else round-trips. Export is the reverse: a visible measure becomes a
metric with its columns qualified by the dataset name; a compound
measure becomes a metric with its `[Name]` references expanded into
the component aggregates and its host table recorded in the `0SQL`
extension, so a round trip restores the grain. The expanded form is
exactly the multi-dataset expression Ossie permits and most consumers
cannot execute safely. 0sql can, which is the strategic point below.

### Expression portability

Ossie's one defined dialect is an ANSI SQL:2003 subset. 0sql's
expression parser already classifies tokens against each dialect's
reserved words and aggregate list (`crates/spec/src/expr.rs`, the
`dialects/*.yml` function patterns). A portability check, "does this
expression use only functions and operators every dialect renders the
same", is a small addition that lets export emit `OSSIE_SQL_2026` or
`ANSI_SQL` for the majority of column references, arithmetic, CASE
and standard aggregates, and the vendor dialect only where needed.
Ossie names SNOWFLAKE, DATABRICKS and BIGQUERY among 0sql's adapters;
POSTGRES, CLICKHOUSE, REDSHIFT and DUCKDB are open pull requests
(#558, #598, #491, #229). The others have no Ossie name and travel as
an extension.

## Why this matters more than it looks

Three facts line up.

Ossie's roadmap is a list of things 0sql already does: dataset-level
measures, explicit cardinality, entity and grain, semantic filters,
cumulative metrics, and a "semantic query language and reference
engine" under Future. The working group that owns metric language and
relationships is where those decisions will be made over the next
year, and today no participant brings a planner that blends across
facts without fan-out; the converters all emit warnings where 0sql
emits correct SQL.

Snowflake's semantic views export to Ossie with one system function,
and Databricks has a converter for Metric Views. A customer who has
invested in a warehouse-native semantic layer can hand 0sql that
document and get the same model planned for every other warehouse they
own. That is the clearest answer to "what does a cross-platform planner
offer a Snowflake shop" that exists, and it costs an importer.

And the critique of Ossie from every direction is the same: it carries
definitions, not governance, grain or execution. 0sql's model carries
exactly those. Being the consumer that reads Ossie and *adds* the
missing layer, visibly and with a loss report, is a position no
converter author has, because every converter is a bridge between two
tools that each lack the layer.

## The plan

Four phases. The first is worth doing on its own and does not mention
Ossie in a single file name.

### Phase 1: fill the gaps, for 0sql's own sake

Each item is a model change with a loader rule, a snapshot field with a
serde default (so `FORMAT` stays at 1), API exposure where the field is
descriptive, and a docs page in 0sql-marketing. No planner change
except where noted. Two items from the first draft were removed after
review: composite keys (interchange data, not model data, since the
modeller already declares cardinality) and a file for table-less
metrics (a compound measure's host table is its grain declaration and
must stay).

1. **Load and serve the project description, and add instructions.**
   project.yml already carries `description` from the template; the
   loader ignores it. Read it into the snapshot's project record,
   return it from `list_projects` and the MCP handshake, and add
   `instructions` beside it. The MCP evaluation found instruction text
   is the lever that closed the gap to the demo; today it has nowhere
   to live in the model.
2. **`ai_context` equivalents on tables and fields: unsettled, needs
   further discussion.** The draft proposed `synonyms` and
   `instructions` on tables and `instructions` and `examples` on fields
   and tables, returned by `fields`, `tables` and `explore` and folded
   into the MCP tool descriptions. Whether model files are the right
   home for agent-facing prose, which objects should carry it, how it
   relates to the server-level MCP instructions the evaluation tuned,
   and how it fits under the 2,000-character cap are all open. Only
   the import mapping is fixed: whatever shape is chosen, Ossie's
   `ai_context` lands in it, and until then it rides in `extensions`.
3. **Data types.** `time`, `float`, and timezone-aware `date_time_tz`,
   with each dialect's literal and truncation patterns extended in
   `dialects/*.yml`. This is the one item with a planner cost; `float`
   can alias `decimal` on day one.
4. **Extension passthrough.** An `extensions` mapping on datasource,
   table, field, join and policy, carried into the snapshot as JSON,
   never read by the planner, returned by the API. The accepted and
   ignored `metadata` key on tables is the natural name to promote.
   Composite and unique keys from Ossie ride here.
5. **Tables defined by a query.** A separate key, not an overloading
   of `physical_name`. Design notes:
   - *Name.* Candidates raised: `source_sql`, `table_from`,
     `table_as_query`. Recommendation: **`sql`**. In 0sql's files `sql`
     already means "a fragment in the datasource's dialect, passed
     through" on a field's expression, on a join and on an inclusion's
     aggregation; a table-level `sql` reads the same way and pairs
     with `physical_name` the way Cube's `sql` pairs with `sql_table`
     and LookML's `derived_table.sql` with `sql_table_name`. If a bare
     `sql` reads too terse at the top of a table file, `source_sql` is
     the fallback; it says the same thing in two words.
   - *Exclusivity.* Exactly one of `physical_name` and `sql` is
     required; both set is refused by name. `physical_name` becomes
     optional, which it has never been, so the loader's required-key
     error and the templates change with it.
   - *Rendering.* The query is wrapped in parentheses and aliased
     exactly where `physical_name` is interpolated today, in the root
     FROM and in every join. One golden per dialect holds it there.
   - *Validation.* 0sql never connects, so `physical_name` is not
     checked against the warehouse either; the difference is that a
     query's output columns are not knowable from a name. Shape checks
     that are possible: a single SELECT or WITH statement, no trailing
     semicolon, no DDL or DML. Column checks: when the SELECT list is
     explicit, the field expressions' column references can be checked
     against its output aliases; when it uses `*` or the list cannot
     be parsed, skip the check and warn once. The expression parser is
     not applied to the query itself; it is passed through like a
     join condition.
   - *Interactions.* `snapshot`, `partitions` and `cost` work
     unchanged; a query table should probably warn when `cost` is
     absent, as every table does. `zsql values sample`, `show` and the
     scaffolding templates need to display and run the query form.
     Imports of field files are unaffected.

Order of value: 1 first, because it is small and improves agent
behaviour now; 4 next, because import depends on it; 3 and 5 as the
importer reaches them; 2 only after it is discussed and settled. Nothing here is implemented; this is the list a decision
would start from.

### Phase 2: `zsql ossie import` and `zsql ossie export`

A new `crates/ossie` crate with the Ossie schema as serde types for
both 0.1.1 (wrapper) and 0.2.0.dev0 (flat), and two CLI commands.

- **Import** writes a project directory: one datasource whose adapter
  is given on the command line (Ossie has no notion of where a model
  runs), a table per dataset, dimensions from fields with `dimension`,
  measures and compound measures from metrics by the algorithm above,
  joins from relationships as `many_to_one` inner equi-joins, every
  `ai_context` into the Phase 1 fields, every foreign
  `custom_extensions` entry into `extensions`, and a **loss report**
  naming each thing that could not be carried and why. The adapter
  selects which `dialects[]` entry to take; a field with no usable
  dialect is refused by name, not skipped silently as Snowflake's
  function does.
- **Export** writes one Ossie document per datasource (a document has
  no datasource concept, and joins do not cross datasources in 0sql
  either), with `0SQL` extensions carrying tiers, cost, partitions,
  cardinality, join type, exclusions, inclusions, snapshots, blend
  groups, formats, policies and uids, and a portability check choosing
  `OSSIE_SQL_2026`, the vendor dialect or an extension per expression.
- **Round-trip goldens**: export each fixture project, import it,
  compare snapshots; import each Ossie example (`tpcds_semantic_model
  .yaml` is in their repository and 0sql has a TPC-DS fixture), plan the
  same specs against both and compare SQL.

### Phase 3: Ossie as a model source, and the converter upstream

- `zsql serve` and `zsql deploy` accept an Ossie document directly,
  importing in memory, so the path from `SYSTEM$READ_OSSIE_YAML_FROM_
  SEMANTIC_VIEW` to planning on a second warehouse is one command. A
  worked page on 0sql.io: "Bring your Snowflake semantic view, plan it
  for Databricks."
- Contribute the converter to `apache/ossie-converters`, which is where
  the project now directs new spokes and which is empty today. It makes
  0sql a named participant with a shipped artifact, which is what the
  working groups weigh.
- Join the Metric Language and Relationships working group with
  concrete proposals drawn from 0sql's model, each one already
  implemented and tested: dataset-level measures (#29) in the shape of
  0sql's table measures; explicit cardinality and join type (#50, #11);
  grain and level of detail (#12) in the shape of exclusions and
  inclusions; semantic filters (#5) in the shape of segments; the
  POSTGRES, CLICKHOUSE, REDSHIFT and DUCKDB dialect names (#558, #598,
  #491, #229) that 0sql has function tables for.

### Phase 4: the reference engine

Ossie's roadmap names "a semantic query language and reference engine"
as future work, and the critiques all say the YAML does not carry a
planner. 0sql is a planner that reads the YAML. The ambition is for
0sql to be the thing the specification points at when it needs to say
what a multi-dataset metric means: the engine whose SQL defines the
semantics of `OSSIE_SQL_2026` metrics over relationships. That is a
community outcome, not a feature, and it is earned by Phases 2 and 3.

## What this does not decide

- How 0sql should carry agent-facing context (instructions, examples,
  synonyms beyond fields). Marked unsettled above; it needs its own
  discussion before any key is added.

- Whether `security.yml` policies should be proposed to Ossie or kept
  as an extension. The project's maintainers favour a minimal core and
  the converters treat security as unrepresentable by design; propose
  tags and PII attributes (#58, #59) and keep policies in the
  extension.
- Whether to engage with the Ontology document type. Watch it; the
  Financial Services working group is its first real user.
- The 0.1.1 versus 0.2.0.dev0 question resolves itself: read both,
  write the draft shape by default and 0.1.1 behind a flag, because
  Snowflake's functions accept only 0.1.1 until they move.

## Sources

- Spec: github.com/apache/ossie/blob/main/core-spec/spec.md, /ossie-schema.json, /expression_language.md; tag `osi-0.1.1-rc1`
- Governance: incubator.apache.org/projects/ossie.html, incubator.apache.org/clutch/ossie.html, ossie.apache.org/updates
- Roadmap: github.com/apache/ossie/blob/main/ROADMAP.md; issues #5, #11, #12, #19, #29, #50, #52, #58, #59, #87, #462; discussion #325
- Converters: github.com/apache/ossie/tree/main/converters (README, dbt, databricks, snowflake, gooddata, cube, omni, sigma, microsoft); github.com/apache/ossie-converters
- Snowflake: docs.snowflake.com/en/sql-reference/functions/system_read_ossie_yaml_from_semantic_view; Cube: cube.dev/changelog/2026-10-16-changelog-apache-ossie-integration
- Critiques: datapace.ai/blog/what-is-apache-ossie, colrows.com/blogs/open-semantic-interchange, vertexjournal.com/reviews/osi-fifty-logos-one-yaml, arxiv.org/abs/2608.26157
- 0sql: crates/project/src/lib.rs (loader keys), crates/semantic/src/snapshot.rs, 0sql-marketing docs under semantic-model, advanced, security and query
