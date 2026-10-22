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
# 0sql never connects to your warehouse. The server has no database driver in\n\
# it at all: it takes a query spec and gives back SQL text. Running that\n\
# SQL against your data stays inside your network, with your credentials,\n\
# and nothing about the rows ever reaches the service.\n\
#\n\
# So the settings below are two different things.\n\
#\n\
# The model needs `adapter` and `tier`, and those are the only keys the\n\
# planner reads. The adapter picks the SQL dialect to generate; the tier\n\
# ranks datasources when more than one could answer a query. They are\n\
# compiled into the deployed model.\n\
#\n\
# Anything else here (host, port, database, username, schema, file) is\n\
# for the tools that run the SQL on your side; 0sql never reads it. A\n\
# deploy does upload this file, with the secrets taken out, but every\n\
# one of those keys is ignored and none of them is kept: the deployed\n\
# model holds the adapter, the tier and the name.\n\
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
#   catalog: awsdatacatalog       # optional\n\
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
        "# Joins between this datasource's tables, by the names in their table files.\n\
# Cardinality is required; many_to_one is the common shape (fact to dimension).\n\
datasource: {datasource}\n\
\n\
# orders_customer:\n\
#   left: Orders\n\
#   right: Customers\n\
#   sql: left.customer_id = right.id\n\
#   cardinality: many_to_one        # many_to_one | one_to_many | one_to_one\n\
#   join: inner                     # inner | left | right\n\
#   # allow_measure_expansion: false\n"
    )
}

pub fn test_yml(name: &str) -> String {
    format!(
        "# A planner test: project these fields and compare the SQL. Runs on every deploy.\n\
name: {name}\n\
projections:\n\
  - Category\n\
  - Revenue\n\
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
  dialect, `tier` ranks datasources when more than one could answer.
  **Never put a password, token or key in this file.** Secrets belong in
  `.zsql`, which is gitignored and never deployed.
- `models/<domain>/tbl.<name>.yml` — a table with its dimensions and
  measures. `models/<domain>/rel.<domain>.yml` — the joins between them.
- `security.yml` — row-level policies.
- `tests/*.yml` — planner tests, run on every deploy.
- `samples/values.jsonl.gz` — sampled dimension values. Customer data;
  gitignored by default, and deployed with the model.

## Commands

    zsql check            # load the model and report problems
    zsql test             # run tests/*.yml
    zsql sql 'revenue by category last month'
    zsql fields -q cost   # find a field
    zsql repl             # explore interactively

Run `zsql check` after every edit. A model that does not load cannot be
deployed.

## Authoring fields

Give every field a `name` a person would use, a `description`, and
`synonyms` for the other words people say. Those three are what makes a
request resolve, and they deserve more care than anything else here.

A measure's `expression.sql` carries its aggregate (`sum(revenue)`). A
dimension's does not.

## Sampling: enable it only for short string dimensions

Sampled values let a request name a value and have it resolve: knowing that
`Region` holds "California" is what connects a question to a filter.

**Enable sampling only on a string dimension whose values are short — under
255 characters, and in practice far shorter: a status, a category, a region,
a channel, a product name.**

Leave it off everywhere else.

- **Long or unbounded text** — `notes`, `comments`, `description`,
  `varchar(max)`, `text`, `clob`. Sampling these fills the catalogue with
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
# zsql: local secrets and sampled data\n\
.zsql\n\
.strata\n\
samples/values.jsonl.gz\n";
