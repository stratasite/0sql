-- The table a server keeps its runs in when ZSQL_DB is set: one row per
-- statement it was asked to run. The application that fronts the server
-- creates it; the server only checks at startup that it is there and
-- has these columns, and says what is missing.
--
-- docs/execution.md says what each column is. A credential is never in
-- any of them.

CREATE TABLE IF NOT EXISTS zsql_results (
    id                 text PRIMARY KEY,
    type               text NOT NULL CHECK (type IN ('semantic', 'sql')),
    status             text NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    error              text,
    row_count          bigint,
    cache_key          text,
    dirty              boolean NOT NULL DEFAULT false,
    tables             text[] NOT NULL DEFAULT '{}',
    org_uid            text NOT NULL DEFAULT 'default',
    project_uid        text NOT NULL,
    branch             text NOT NULL,
    datasource_uid     text NOT NULL,
    sql                text NOT NULL,
    spec               jsonb,
    expr               text,
    row_limit          bigint,
    hostname           text NOT NULL,
    object_store_path  text,
    created_at         timestamptz NOT NULL,
    queued_at          timestamptz NOT NULL,
    started_at         timestamptz,
    finished_at        timestamptz,
    updated_at         timestamptz NOT NULL,
    cancelled_at       timestamptz,
    expires_at         timestamptz
);

-- An identical question finds the newest result that still answers.
CREATE INDEX IF NOT EXISTS zsql_results_cached
    ON zsql_results (cache_key, finished_at DESC)
    WHERE status = 'succeeded' AND NOT dirty;

-- One run of a question in flight at a time, across every node: a
-- second node's insert is refused and it joins the first.
CREATE UNIQUE INDEX IF NOT EXISTS zsql_results_in_flight
    ON zsql_results (cache_key)
    WHERE status IN ('queued', 'running') AND cache_key IS NOT NULL;

-- Invalidation by the tables a result read.
CREATE INDEX IF NOT EXISTS zsql_results_tables ON zsql_results USING gin (tables);

-- What cleanup looks for: results past their time, and runs nobody is
-- running any more.
CREATE INDEX IF NOT EXISTS zsql_results_expires ON zsql_results (expires_at) WHERE status = 'succeeded';
CREATE INDEX IF NOT EXISTS zsql_results_alive ON zsql_results (updated_at) WHERE status IN ('queued', 'running');
