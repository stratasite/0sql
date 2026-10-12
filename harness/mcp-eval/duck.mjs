// Run SQL statements, read from stdin as a JSON array, against the demo
// warehouse (DuckDB, read-only) and print one JSON result per statement.
// The grader calls this once per case with the reference and candidate
// statements; it is a separate process because the DuckDB bindings live in
// the demo's node_modules, not here.
//
//   ZSQL_EVAL_DEMO=~/Projects/demo/0sql-dashboard-demo  (default)
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { homedir } from 'node:os';

const demo = process.env.ZSQL_EVAL_DEMO ?? join(homedir(), 'Projects/demo/0sql-dashboard-demo');
const require = createRequire(join(demo, 'package.json'));
const { DuckDBInstance } = require('@duckdb/node-api');

const sqls = JSON.parse(readFileSync(0, 'utf8'));
const instance = await DuckDBInstance.fromCache(join(demo, 'warehouse/cs_warehouse.duckdb'), { access_mode: 'READ_ONLY' });
const conn = await instance.connect();
const out = [];
for (const sql of sqls) {
  try {
    const reader = await conn.runAndReadAll(sql);
    out.push({ columns: reader.columnNames(), rows: reader.getRowObjectsJson() });
  } catch (e) {
    out.push({ error: String(e.message ?? e) });
  }
}
process.stdout.write(JSON.stringify(out));
