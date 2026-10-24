//! What `zsql init` and `zsql new` write.

pub fn project_yml(name: &str, uid: &str) -> String {
    format!(
        "# zsql project. The uid is what the server knows the project as; keep it stable.\n\
name: {name}\n\
uid: {uid}\n\
description: \n\
\n\
# The branch that production reads from. Deploys go to the checked-out git\n\
# branch, so this is the branch to merge into when a change is ready.\n\
production_branch: main\n\
\n\
# The server `zsql deploy` sends to, when it is not the local one; the\n\
# api key lives in .zsql, never here.\n\
# server: http://127.0.0.1:3699\n"
    )
}

pub const DATASOURCES_YML: &str = "\
# The warehouses this project's tables live in. One entry per datasource,\n\
# keyed by a short id that table files refer to.\n\
#\n\
# The server never connects to your warehouse. It has no database driver\n\
# in it at all: it takes a query spec and gives back SQL text, and nothing\n\
# about your rows ever reaches it. The zsql command on this machine does\n\
# connect, read-only, when you ask it to: `zsql catalog` reads the\n\
# catalog a model is drafted from, and `zsql sample` reads the values of\n\
# the dimensions you turn sampling on for. Give it a credential with no\n\
# write grant.\n\
#\n\
# So the settings below are two different things.\n\
#\n\
# The model needs `adapter` and `tier`, and those are the only keys the\n\
# planner reads. The adapter picks the SQL dialect to generate; the tier\n\
# ranks datasources when more than one could answer a query. They are\n\
# compiled into the deployed model.\n\
#\n\
# Everything else here (host, port, database, username, schema, file and\n\
# the keys each warehouse adds) is for the zsql command and for your own\n\
# tools. A deploy does upload this file, with the secrets taken out, but\n\
# every one of those keys is ignored and none of them is kept: the\n\
# deployed model holds the adapter, the tier and the name.\n\
#\n\
# Secrets never leave this machine. Passwords, tokens and keys go in\n\
# .zsql under the same datasource key, or in ZSQL_DS_<KEY>_<FIELD>:\n\
# ZSQL_DS_WAREHOUSE_PASSWORD, ZSQL_DS_SNOW_PERSONAL_ACCESS_TOKEN,\n\
# ZSQL_DS_SNOW_PRIVATE_KEY. `zsql deploy` strips them from what it sends,\n\
# and .zsql is gitignored and never deployed.\n\
#\n\
# warehouse:\n\
#   name: Warehouse\n\
#   adapter: postgres            # the dialect to generate; duckdb | postgres | snowflake | bigquery | databricks | trino | mysql | sqlserver | ...\n\
#   tier: hot                    # hot | warm | cold; the planner prefers hotter tiers\n\
#   host: localhost              # from here down: for your own tools; the server ignores it\n\
#   port: 5432\n\
#   database: analytics\n\
#   username: analyst\n\
#   schema: public\n\
#   sslmode: verify-full          # disable | prefer | require | verify-ca | verify-full, libpq's meanings; verify-full unless said\n\
#   ca_file: ./ca.pem             # a PEM of certificates to trust besides the platform's, for a private CA\n\
#\n\
# redshift:\n\
#   name: Redshift\n\
#   adapter: redshift             # Postgres on the wire: host, port 5439, database, username, password in .zsql, sslmode\n\
#   tier: warm\n\
#   host: cluster.abc123.us-east-1.redshift.amazonaws.com\n\
#   database: analytics\n\
#   username: zsql\n\
\n\
# snow:\n\
#   name: Snowflake\n\
#   adapter: snowflake\n\
#   tier: warm\n\
#   account: myorg-myaccount      # the account identifier: <account>.snowflakecomputing.com\n\
#   database: ANALYTICS           # as you would type it unquoted; \"Mixed Case\" in quotes to send it as written\n\
#   warehouse: COMPUTE_WH         # optional; your user's default otherwise\n\
#   schema: PUBLIC                # optional; introspect reads every schema without it\n\
#   role: ANALYST                 # optional\n\
#   # a personal access token goes in .zsql as personal_access_token; or a key pair:\n\
#   username: zsql                # the user the key is registered to\n\
#   private_key_file: ./rsa_key.p8   # PKCS#8 PEM, unencrypted; or the PEM itself in .zsql as private_key\n\
\n\
# lake:\n\
#   name: Databricks\n\
#   adapter: databricks\n\
#   tier: warm\n\
#   host: adb-1234567890123456.7.azuredatabricks.net   # the workspace\n\
#   warehouse: abc123def456      # the SQL warehouse id\n\
#   catalog: main                 # Unity Catalog; introspect reads its information_schema\n\
#   schema: default               # optional\n\
#   # a personal access token goes in .zsql as personal_access_token; or a service principal:\n\
#   client_id: 00000000-0000-0000-0000-000000000000   # with oauth_client_secret in .zsql\n\
\n\
# bq:\n\
#   name: BigQuery\n\
#   adapter: bigquery\n\
#   tier: warm\n\
#   project: my-project\n\
#   dataset: analytics            # optional; introspect reads the whole region without it\n\
#   location: US                  # the region, for a whole-region read\n\
#   key_file: ./service-account.json   # or GOOGLE_APPLICATION_CREDENTIALS, or `gcloud auth application-default login`\n\
\n\
# lakehouse:\n\
#   name: Athena\n\
#   adapter: athena\n\
#   tier: cold\n\
#   region: us-east-1             # or AWS_REGION, or the profile's config\n\
#   database: default             # the Glue database; introspect reads it\n\
#   catalog: AwsDataCatalog       # optional\n\
#   workgroup: primary            # optional\n\
#   output_location: s3://my-athena-results/zsql/   # unless the workgroup sets one\n\
#   # keys: access_key_id and secret_access_key in .zsql (session_token too), or AWS_ACCESS_KEY_ID and\n\
#   # AWS_SECRET_ACCESS_KEY, or a profile in ~/.aws/credentials (profile: name, or AWS_PROFILE)\n\
\n\
# local:\n\
#   name: Local DuckDB\n\
#   adapter: duckdb\n\
#   tier: hot\n\
#   file: ./data/analytics.duckdb\n";

pub const SECURITY_YML: &str = "\
# Row-level security policies. A policy fires when a query projects a field\n\
# it names, by tag or by name; it then resolves which values of its context\n\
# dimension the caller's security context allows and filters rows to them\n\
# (filter_data) or masks the field outside them (mask_data). A fired policy\n\
# that resolves nothing denies unless it says `unresolved: allow`.\n\
policies: []\n\
#  - name: Call center PII\n\
#    mode: filter_data            # filter_data | mask_data\n\
#    mask_value: \"#######\"        # mask_data only\n\
#    triggers:\n\
#      field_tags: [pii]\n\
#      # field_names: [Agent Name]\n\
#    context_dimension: Call Center ID\n\
#    permission_resolution:\n\
#      source: groups             # groups | user\n\
#      value_from: tag            # groups: name | tag; user: email | tag\n\
#      tag_key: call_center_id\n\
#      unresolved: deny           # deny | allow\n\
#    bypass:\n\
#      system_admin: true\n\
#      project_admin: false\n";

pub fn table_yml(name: &str, physical_name: &str, datasource: &str) -> String {
    // No datasource configured yet: the key stays, with a note, so the file
    // is one edit away from loading rather than silently wrong.
    let datasource = if datasource.is_empty() { "              # the datasource's key, from datasources.yml" } else { datasource };
    format!(
        "# A table and the dimensions and measures it holds. Joins between tables\n\
# live in rel.*.yml files.\n\
name: {name}\n\
physical_name: {physical_name}\n\
datasource: {datasource}\n\
# The planner prefers the cheapest table that answers a question. Dimension\n\
# tables low, big facts high.\n\
cost: 10\n\
\n\
# For a snapshot table (inventory, balances), the date dimension it snapshots on.\n\
# snapshot: Date\n\
\n\
# Data the table holds, so the planner prefers it for filters inside the\n\
# range and avoids it for filters outside. Predicates: between,\n\
# greater_than, greater_than_or_equal_to, less_than, less_than_or_equal_to\n\
# (number or date dimensions) and in_list (any dimension).\n\
# partitions:\n\
#   - dimension: Date\n\
#     predicate: between\n\
#     filter_value: 2y\n\
#     filter_value_end: 1d\n\
#   - dimension: Region\n\
#     predicate: in_list\n\
#     filter_value: US, EU\n\
\n\
fields:\n\
  # - type: dimension                 # dimension | measure\n\
  #   name: Category\n\
  #   description: Product category\n\
  #   data_type: string               # string | integer | bigint | decimal | date | date_time | boolean\n\
  #   synonyms: [dept]\n\
  #   tags: []                        # policy triggers, e.g. [pii]\n\
  #   enable_sampling: true           # collect this dimension's values into samples/values.jsonl.gz\n\
  #   sample_size: 1000               # how many values, most frequent first; at most 20000\n\
  #   sample_sql: |                   # your own query instead of the generated one\n\
  #     SELECT category, COUNT(*) FROM products GROUP BY 1 ORDER BY 2 DESC\n\
  #   expression:\n\
  #     sql: category\n\
  #     primary_key: false\n\
  #\n\
  # - type: measure\n\
  #   name: Revenue\n\
  #   data_type: decimal\n\
  #   format: currency:2\n\
  #   expression:\n\
  #     sql: sum(revenue)\n"
    )
}

pub fn relation_yml(datasource: &str) -> String {
    format!(
        "# Joins between this datasource's tables, keyed by a name of your own.\n\
# `left` and `right` are the names in the table files, not physical names.\n\
# `sql` is always written `left.column = right.column`; several columns are\n\
# ANDed. `cardinality` is required, and `many_to_one` is the common shape:\n\
# many fact rows to one dimension row, with the dimension on the right.\n\
#\n\
# `zsql generate --relationships <domain>/<name> --infer` writes this file\n\
# from the table files it finds, by matching key column names. `zsql check`\n\
# tells you whether what you write here loads.\n\
datasource: {datasource}\n\
{}",
        relation_examples()
    )
}

/// The forms a join takes, commented, for the bottom of a joins file.
pub fn relation_examples() -> &'static str {
    "\n\
# A fact to its dimension, the usual case:\n\
# transactions_household:\n\
#   left: Fact Transactions\n\
#   right: Dim Household\n\
#   sql: left.household_key = right.household_key\n\
#   cardinality: many_to_one        # many_to_one | one_to_many | one_to_one\n\
#   join: inner                     # inner | left | right; left keeps the\n\
#                                   # left rows that have no match\n\
\n\
# A composite key, ANDed:\n\
# sales_store_day:\n\
#   left: Fact Sales\n\
#   right: Dim Store Day\n\
#   sql: left.store_id = right.store_id AND left.day_id = right.day_id\n\
#   cardinality: many_to_one\n\
\n\
# A dimension that extends another, one row each:\n\
# household_demographics:\n\
#   left: Dim Household\n\
#   right: Dim Demographics\n\
#   sql: left.household_key = right.household_key\n\
#   cardinality: one_to_one\n\
#   join: left\n\
\n\
# A bridge whose own measures may be aggregated through the join, which\n\
# double counts unless you mean it:\n\
# basket_lines:\n\
#   left: Dim Basket\n\
#   right: Fact Lines\n\
#   sql: left.basket_id = right.basket_id\n\
#   cardinality: one_to_many\n\
#   allow_measure_expansion: true\n\
#   description: a basket's lines, for counting items per basket\n"
}

pub fn test_yml(name: &str, fields: Option<(Option<String>, Option<String>)>) -> String {
    let (dim, msr) = fields.unwrap_or((None, None));
    let projections = match (dim, msr) {
        (Some(d), Some(m)) => format!("  - {d}\n  - {m}\n"),
        (Some(d), None) => format!("  - {d}\n"),
        (None, Some(m)) => format!("  - {m}\n"),
        (None, None) => "  # - A Dimension   # name fields of this model; `zsql fields` lists them\n  # - A Measure\n".to_string(),
    };
    format!(
        "# A planner test: project these fields and compare the SQL. Runs on every deploy.\n\
name: {name}\n\
projections:\n\
{projections}\
# assert_sql: |\n\
#   SELECT ...\n\
assert_regex: SELECT\n"
    )
}

/// Guidance for an agent working on a project's model, written into the
/// project by `zsql init`. The sampling rule lives here rather than in the
/// loader: the modeller decides, and `enable_sampling: true` is obeyed
/// whatever the field looks like. A raw string, because the `\n\`
/// continuation the other templates use eats the leading whitespace that
/// markdown lists need.
pub const AGENTS_MD: &str = r#"# Working on this 0sql model

This project is a semantic model. 0sql resolves a request in business terms
against it and returns one SQL statement, or a refusal that says why. The
model is the source of truth: if a question cannot be answered, the model is
what to change.

## Layout

- `project.yml` — the project's name, uid and production branch.
- `datasources.yml` — one entry per warehouse: `adapter` picks the SQL
  dialect, `tier` ranks datasources when more than one could answer; the
  other keys are how the `zsql` command on this machine connects.
  **Never put a password, token or key in this file.** Secrets belong in
  `.zsql`, which is gitignored and never deployed.
- `catalog/<datasource>.json` — what the warehouse holds, as `zsql catalog`
  read it. Committed: it is schema, not data, and `zsql check` validates
  the model against it.
- `models/<domain>/tbl.<name>.yml` — a table with its dimensions and
  measures. `models/<domain>/rel.<name>.yml` — the joins between them.
  The `<domain>` directories are the model's subject areas and need not
  mirror the warehouse's schemas; `zsql generate <domain>/ ...` picks one.
- `security.yml` — row-level policies.
- `tests/*.yml` — planner tests, run on every deploy.
- `samples/values.jsonl.gz` — sampled dimension values. Customer data;
  gitignored by default, and deployed with the model.

## Commands

The flow, in order. Nothing prompts when stdin is not a terminal, so every
step below is its non-interactive form; the flag forms are what you use.

    zsql --agent <command>                     # JSON from every command that answers with data, and never a prompt
    zsql datasource list                       # what datasources.yml holds, and whether each has its secret
    zsql datasource auth -d wh --secret-stdin  # set or replace its secret in .zsql
    zsql datasource test -d wh                 # open the connection; list the schemas it can see
    zsql catalog read -d wh --schema public    # read a schema into catalog/wh.json (never --all on a big warehouse)
    zsql catalog read -d wh sales.orders       # or one table by name
    zsql catalog search orders [--live] --json # find tables, in the file or in the warehouse
    zsql catalog show orders --json            # one table's columns, keys and comment, from the file
    zsql catalog sync -d wh                    # re-read what the file records; drift is printed
    zsql generate orders customers             # draft models for named tables; --schema S or --all for more
    zsql generate orders --new-columns         # add the columns a model lacks
    zsql generate orders --overwrite           # draft it again, losing what is in the file
    zsql generate shopping/ --all              # draft into models/shopping/, whatever the warehouse's schemas are
    zsql generate returns --empty              # an empty model from the template, no catalog behind it
    zsql generate --relationships main/sales --infer   # the joins file, inferred from the table files there
    zsql generate --relationships main/sales   # the same file as a template, to write by hand
    zsql check                                 # load the model, check it against the catalog, run the tests
    zsql sample --yes [Status Region]          # collect sampled values; asks first on a terminal
    zsql sql 'revenue by region, order date >= 28d'
    zsql run 'revenue by region' --format jsonl
    zsql fields cost                           # find a field
    zsql test                                  # run tests/*.yml; --new NAME writes one from the model's own fields

Run `zsql check` after every edit. A model that does not load cannot be
deployed. `zsql generate` with no table and no terminal refuses and says
the three ways to name tables; it never drafts every table unless told
`--all`.

## Authoring fields

Give every field a `name` a person would use, a `description`, and
`synonyms` for the other words people say. Those three are what makes a
request resolve, and they deserve more care than anything else here.

A measure's `expression.sql` carries its aggregate (`sum(revenue)`). A
dimension's does not.

A field named on several tables is one field, with one description, one set
of synonyms and one type; the tables only say where it is mapped. Describe
it on the table it belongs to and leave the others bare. A later file fills
in only what an earlier one left out, so two files that both describe it
differently leave the answer to whichever loads last. `zsql check` warns
when that happens, and `zsql generate` writes the description once.

## Sampling: enable it only for short string dimensions

Sampled values let a request name a value and have it resolve: knowing that
`Region` holds "California" is what connects a question to a filter.

**Enable sampling only on a string dimension whose values are short — under
255 characters, and in practice far shorter: a status, a category, a region,
a channel, a product name.**

Leave it off everywhere else.

- **Long or unbounded text** — `notes`, `comments`, `description`,
  `varchar(max)`, `text`, `clob`. Sampling these fills the catalog with
  prose that matches every request a little and nothing well, which makes
  resolution worse than no samples at all. This is the mistake to avoid.
- **High-cardinality keys and identifiers** — an id, an order number, an
  email, an account number. Millions of values nobody will ever name, and
  customer data you did not need to copy.
- **Numbers, dates and booleans.** A request filters these by range, not by
  value.
- **Anything a security policy covers, and anything sensitive.** A sample
  leaves the warehouse and travels with the model.

Nothing enforces this. `enable_sampling: true` is obeyed whatever the field
is, because you know the data and the loader does not. `zsql sample` warns
when what it collected looks like long text, and a warning there means the
field wants turning off, or a `sample_sql` that trims and filters what it
returns.

`sample_size` defaults to 1000 and may not exceed 20000. Values come back
most frequent first, so a smaller number keeps the ones that matter.
"#;

pub const GITIGNORE: &str = "\
# zsql: local secrets and sampled data. catalog/ is meant to be committed.\n\
.zsql\n\
.strata\n\
samples/values.jsonl.gz\n";
