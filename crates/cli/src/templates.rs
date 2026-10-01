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
# A self-hosted zsqld, if any. The hosted service is used otherwise; the\n\
# api key lives in .zsql, never here.\n\
# server: http://127.0.0.1:3699\n"
    )
}

pub const DATASOURCES_YML: &str = "\
# The warehouses this project's tables live in. One entry per datasource,\n\
# keyed by a short id that table files refer to. Connection details that\n\
# are secrets (password, tokens, keys) go in .zsql under the same key, or\n\
# in ZSQL_DS_<KEY>_PASSWORD; they are never deployed.\n\
#\n\
# warehouse:\n\
#   name: Warehouse\n\
#   adapter: postgres            # duckdb | postgres | snowflake | bigquery | databricks | trino | mysql | sqlserver | ...\n\
#   tier: hot                    # hot | warm | cold; the planner prefers hotter tiers\n\
#   host: localhost\n\
#   port: 5432\n\
#   database: analytics\n\
#   username: analyst\n\
#   schema: public\n\
#\n\
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
  #   values: sample                  # sample | full | none, for `zsql values sample`\n\
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

pub const GITIGNORE: &str = "\
# zsql: local secrets and sampled data\n\
.zsql\n\
.strata\n\
samples/values.jsonl.gz\n";

pub const SAMPLES_README: &str = "\
Sample data for this project. `zsql values sample` writes values.jsonl.gz\n\
here (the top values of each listable dimension); it is customer data and\n\
is gitignored by default.\n";
