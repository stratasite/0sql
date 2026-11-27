// A typed client for 0sql's data prep API: sources, datasets, runs of
// the graphs your application keeps, previews, and blends with the
// model. It is the part of this example meant to be copied: it has no
// dependency beyond Node's fetch.
//
// It runs on your backend, never in a browser. zsqld keeps no user: a
// source or a dataset is its project's, addressed by its id, and zsqld
// answers whoever reaches it. Which user may read or change which is
// your application's to decide, before it calls anything here; see
// `src/registry.ts` for how this example decides.
//
// Your application owns each dataset's pipeline: the graph, its node
// ids, its versions, the user's edits inside it, and when it runs. zsqld
// checks a graph it is sent, runs it, keeps the output, and says what
// each node did. See docs/ingestion.md in the 0sql repository.

import { Readable } from "node:stream";

// ------------------------------------------------------------------ types

export type Role = "dimension" | "measure";

/** `boolean`, `integer`, `decimal(<scale>)`, `double`, `date`, `timestamp` or `text`. */
export type ColumnType = string;

export interface Column {
  name: string;
  type: ColumnType;
  role: Role;
}

/** Why a job did not finish: the process died with it (`crash`), or the server was stopped while it ran (`stop`); nothing is resumed. */
export interface Interrupted {
  job: "upload" | "push" | "pull" | "run" | "delete";
  started: string;
  found: string;
  cause: "crash" | "stop";
  reason: string;
}

/** Where data comes from, and what came: what the "add an input" explorer lists. */
export interface Source {
  id: string;
  name: string;
  description?: string;
  kind: "upload" | "push" | "query";
  created: string;
  updated: string;
  /** The newest snapshot; 0 before the first. */
  snapshot: number;
  columns: Column[];
  query?: { branch: string; spec?: unknown; expr?: string };
  /** Columns read as other types, for every graph that reads it. */
  types?: Record<string, ColumnType>;
  /** The version of how it is read. */
  read: number;
  interrupted?: Interrupted;
}

export interface Snapshot {
  source: string;
  number: number;
  kind: Source["kind"];
  taken: string;
  file_name?: string;
  sha256: string;
  bytes: number;
  parse?: Record<string, unknown>;
  origin?: unknown;
  columns: Column[];
  rows: number;
  changes?: string[];
}

export type RunState = "running" | "suspended" | "succeeded" | "failed" | "held" | "interrupted";

/** What is browsed, shared and blended. */
export interface Dataset {
  id: string;
  name: string;
  description?: string;
  created: string;
  updated: string;
  /** The output version it answers with, once a run made one. */
  live: number | null;
  made: number;
  columns: Column[];
  rows: number;
  /** What the run that made the live output let through: a warning, an orphaned edit. */
  notes?: string[];
  last_run?: { id: string; state: RunState; at: string };
  backup?: { at: string; failed?: string };
  /** A job a restart found in flight and did not run again; show `reason`. */
  interrupted?: Interrupted;
}

export interface Page {
  version?: number;
  snapshot?: number;
  columns: Column[];
  total: number;
  offset: number;
  rows: unknown[][];
}

/** One step of a `steps` node. Each is a closed shape; see docs/ingestion.md, "Steps". */
export type Step =
  | { op: "rename_column"; column: string; to: string }
  | { op: "retype_column"; column: string; type: ColumnType; format?: string }
  | { op: "remove_columns"; columns: string[] }
  | { op: "reorder_columns"; columns: string[] }
  | { op: "add_column"; column: string; formula: string }
  | { op: "update_column"; column: string; formula: string }
  | { op: "filter"; formula: string }
  | { op: "sort"; by: { column: string; desc?: boolean }[] }
  | { op: "dedupe"; columns?: string[] }
  | { op: "fill_empty"; column: string; value: unknown }
  | { op: "fill_down"; columns: string[] }
  | { op: "unpivot"; columns?: string[]; keep?: string[]; name: string; value: string; keep_empty?: boolean }
  | { op: "find_and_replace_val"; column: string; find: string; with: string; whole?: boolean }
  | { op: "remap_val"; column: string; values: { from: unknown; to: unknown }[] }
  | { op: "trim_val"; columns: string[] }
  | { op: "upper_case" | "lower_case" | "title_case" | "snake_case" | "camel_case"; columns: string[] }
  | {
      op: "group";
      by: string[];
      aggregates: { column?: string; fn: "sum" | "min" | "max" | "avg" | "count" | "count_distinct"; as: string }[];
    }
  | { op: "sql"; query: string };

/** A user's edit, carried inline in an `edits` node, naming its rows by value. */
export type Edit =
  | { id: string; kind: "remap"; column: string; from: unknown; to: unknown }
  | { id: string; kind: "set"; column: string; key: Record<string, unknown>; to: unknown; was?: unknown; all?: boolean }
  | { id: string; kind: "delete"; key: Record<string, unknown>; all?: boolean }
  | { id: string; kind: "add"; key: Record<string, unknown>; values?: Record<string, unknown> };

/** What an input reads: the newest, a snapshot or version, a pull first, or a pull when older than `max_age` seconds. */
export type Read = "latest" | "pull" | { snapshot: number } | { version: number } | { max_age: number };

/** A node of a graph. Its id is yours, and everything a run says is keyed by it. */
export type GraphNode = { id: string; note?: string } & (
  | { kind: "source"; source: string; read?: Read; types?: Record<string, ColumnType> }
  | { kind: "dataset"; dataset: string; read?: Read }
  | { kind: "steps"; input: string; steps: Step[] }
  | {
      kind: "join";
      left: string;
      right: string;
      on: { left: string; right: string }[];
      type?: "left" | "inner";
      columns?: string[];
      prefix?: string;
    }
  | { kind: "append"; inputs: string[] }
  | { kind: "sql"; inputs: Record<string, string>; query: string }
  | { kind: "edits"; input: string; key?: string[]; edits: Edit[] }
  | {
      kind: "output";
      input: string;
      roles?: Record<string, Role>;
      key?: string[];
      checks?: { check: "unique" | "not_null"; columns: string[]; on_fail?: "hold" | "warn" }[];
    }
);

/** A pipeline: from its inputs to one output. */
export interface Graph {
  nodes: GraphNode[];
}

/** Something wrong with a graph, said of its node. */
export interface Problem {
  node?: string;
  code: "graph" | "not_found" | "invalid" | "missing_column" | "read" | "context" | "data";
  message: string;
  column?: string;
  /** Columns that may be the one meant, and why. */
  suggestions?: { column: string; because: string }[];
}

/** A source or dataset a graph reads: check each against who is asking. */
export interface ReadRef {
  kind: "source" | "dataset";
  id: string;
}

export interface InputPlan {
  node: string;
  kind: "source" | "dataset";
  id: string;
  name: string;
  will: "read" | "pull";
  number?: number;
  taken?: string;
  age_secs?: number;
  why: string;
}

export interface EditOutcome {
  id: string;
  state: "applied" | "orphaned" | "ambiguous" | "flagged" | "conflict";
  detail?: string;
}

/** What one node did: numbers counted by the run, and a summary written from them. */
export interface NodeRecord {
  id: string;
  kind: GraphNode["kind"];
  rows_in?: number[];
  rows: number;
  columns: number;
  added?: string[];
  removed?: string[];
  retyped?: { column: string; from: ColumnType; to: ColumnType }[];
  steps?: { step: number; op: string; rows: number; changed?: number; said: string }[];
  join?: { matched: number; unmatched_left: number; unmatched_right: number; sample?: boolean };
  append?: { only_in_some?: string[] };
  edits?: EditOutcome[];
  checks?: { check: string; columns: string[]; on_fail: string; passed: boolean; failing?: number }[];
  warnings?: string[];
  summary: string;
  profile?: { column: string; type: ColumnType; role: Role; nulls: number; distinct: number; min?: string; max?: string }[];
  ms: number;
}

export interface RunRecord {
  id: string;
  dataset: string;
  state: RunState;
  started: string;
  ended?: string;
  context: string;
  reads: ReadRef[];
  inputs?: InputPlan[];
  version?: number;
  nodes?: NodeRecord[];
  problems?: Problem[];
  error?: string;
  summary: string;
  interrupted?: Interrupted;
}

export interface Checked {
  ok: boolean;
  reads: ReadRef[];
  inputs: InputPlan[];
  nodes: { id: string; kind: string; columns: Column[] }[];
  problems: Problem[];
  said: string;
}

export interface Previewed {
  node: string;
  columns: Column[];
  rows: unknown[][];
  nodes: NodeRecord[];
  problems: Problem[];
  reads: ReadRef[];
  inputs: InputPlan[];
  /** How many rows of each input it read, at most; none when it read them all. */
  sample: number | null;
}

export interface CsvOptions {
  /** Shown in the app; the file's name without `.csv` when not said. */
  name?: string;
  description?: string;
  /** Kept in the snapshot. */
  fileName?: string;
  delimiter?: string;
  quote?: string;
  header?: boolean;
  maxBadRows?: number;
}

export interface BlendJoin {
  /** The attached dataset's alias. */
  join: string;
  /** `model`, or another attached alias. */
  to: string;
  on: { left: string; right: string }[];
  kind: "filter" | "enrich";
}

export interface BlendRequest {
  /** A spec whose projections may name `alias.column`. */
  spec: { projections: { field: string; alias?: string }[]; [k: string]: unknown };
  datasets: { id: string; as: string }[];
  blend: BlendJoin[];
  context?: unknown;
  connection?: Record<string, unknown>;
  wait?: number;
  fresh?: boolean;
  offset?: number;
  limit?: number;
}

export interface Blended {
  columns: { name: string; type: string; arrow_type: string }[];
  rows: unknown[][];
  total: number;
  offset: number;
  complete: boolean;
  cache: "hit" | "miss";
  model: { result: string; sql: string };
  datasets: { id: string; version: number }[];
}

/** A field of the model, as `fields` lists it. */
export interface ModelField {
  uid: string;
  name: string;
  kind: "dimension" | "measure";
  data_type: string;
  tables: string[];
  hidden: boolean;
  description?: string;
}

// ------------------------------------------------------------------ errors

/** A refusal from zsqld, in its own words. `message` is written to be shown to a user. */
export class ZsqlError extends Error {
  constructor(
    readonly status: number,
    /** `Invalid`, `NotFound`, `Conflict`, `Unprocessable`, `Refused`, `Busy`, `Internal`, ... */
    readonly kind: string,
    message: string,
    /** Seconds to wait before asking again, when zsqld said. */
    readonly retryAfter?: number,
    /** For a graph refused before it ran: each problem, of its node. */
    readonly problems?: Problem[],
  ) {
    super(message);
    this.name = "ZsqlError";
  }

  /**
   * Busy (503: nothing was done) is worth one more try. A run is
   * different: send it again with the same id and zsqld answers what the
   * first did, so a retry never makes anything twice.
   */
  get retryable(): boolean {
    return this.status === 503 && this.kind === "Busy";
  }
}

// ------------------------------------------------------------------ client

export interface ClientOptions {
  /** Where zsqld is: `http://127.0.0.1:3699`. */
  baseUrl: string;
  /** The org; `default` when not said. */
  org?: string;
  /** How long a call may take. A run longer than its `wait` is answered 202, and read again by its id. */
  timeoutMs?: number;
  fetch?: typeof fetch;
}

interface Call {
  method?: string;
  json?: unknown;
  csv?: Readable | Buffer | string;
}

/** zsqld's data prep, for an org. {@link DataPrep.project} gives the calls on one project. */
export class DataPrep {
  readonly #base: string;
  readonly #timeout: number;
  readonly #fetch: typeof fetch;

  constructor(options: ClientOptions) {
    const org = options.org && options.org !== "default" ? `/orgs/${encodeURIComponent(options.org)}` : "";
    this.#base = options.baseUrl.replace(/\/+$/, "") + org;
    this.#timeout = options.timeoutMs ?? 100_000;
    this.#fetch = options.fetch ?? fetch;
  }

  /** The calls on one project's sources, datasets and runs. */
  project(project: string): ProjectClient {
    const base = `/projects/${encodeURIComponent(project)}`;
    return new ProjectClient(<T>(path: string, init?: Call) => this.call<T>(base + path, init));
  }

  /** Brings back from the object store every source and dataset of the org this node lacks. */
  async restore(): Promise<{ restored: string[] }> {
    return this.call("/datasets/restore", { method: "POST", json: {} });
  }

  /** @internal */
  async call<T>(path: string, init: Call = {}): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    let body: BodyInit | undefined;
    let duplex: "half" | undefined;
    if (init.json !== undefined) {
      headers["content-type"] = "application/json";
      body = JSON.stringify(init.json);
    } else if (init.csv !== undefined) {
      headers["content-type"] = "text/csv";
      if (init.csv instanceof Readable) {
        // Streamed through: the file is never held whole on this side.
        body = Readable.toWeb(init.csv) as unknown as BodyInit;
        duplex = "half";
      } else {
        body = init.csv as BodyInit;
      }
    }
    const res = await this.#fetch(this.#base + path, {
      method: init.method ?? "GET",
      headers,
      body,
      signal: AbortSignal.timeout(this.#timeout),
      ...(duplex ? { duplex } : {}),
    } as RequestInit);
    const text = await res.text();
    const parsed = text ? safeJson(text) : undefined;
    if (!res.ok) {
      const p = parsed as { error?: { class?: string; message?: string }; problems?: Problem[] } | undefined;
      const retry = res.headers.get("retry-after");
      throw new ZsqlError(res.status, p?.error?.class ?? "Http", p?.error?.message ?? (text || res.statusText), retry ? Number(retry) : undefined, p?.problems);
    }
    // A 202 (a run still going, a preview still pulling) is an answer too.
    return parsed as T;
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function query(params: Record<string, string | number | boolean | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== "") q.set(k, String(v));
  const s = q.toString();
  return s ? `?${s}` : "";
}

const p = (id: string) => encodeURIComponent(id);

function view(v: { offset?: number; limit?: number; sort?: { column: string; desc?: boolean }[]; filter?: string }) {
  const sort = v.sort?.map((k) => `${k.column}${k.desc ? ":desc" : ""}`).join(",");
  return { offset: v.offset, limit: v.limit, sort, filter: v.filter };
}

/** One project's sources, datasets and runs. Made by {@link DataPrep.project}. */
export class ProjectClient {
  constructor(private readonly call: <T>(path: string, init?: Call) => Promise<T>) {}

  // ---------------------------------------------------------- sources

  async sources(): Promise<Source[]> {
    return (await this.call<{ sources: Source[] }>(`/sources`)).sources;
  }

  /**
   * A new upload source from a CSV. With `dataset`, also a dataset over
   * it as it is: one thing to list, which copies nothing. The project
   * must be deployed on some branch.
   */
  createUpload(
    csv: Readable | Buffer | string,
    options: CsvOptions & { dataset?: boolean } = {},
  ): Promise<{ source: Source; snapshot: Snapshot; dataset?: Dataset }> {
    return this.call(`/sources${csvQuery(options, { kind: "upload", dataset: options.dataset || undefined })}`, { method: "POST", csv });
  }

  /** A new push source: rows your app or an agent got from a tool, and where from. */
  createPush(
    rows: Record<string, unknown>[],
    options: { name?: string; description?: string; origin?: unknown; dataset?: boolean } = {},
  ): Promise<{ source: Source; snapshot: Snapshot; dataset?: Dataset }> {
    const q = query({ kind: "push", name: options.name, description: options.description, dataset: options.dataset || undefined });
    return this.call(`/sources${q}`, { method: "POST", json: { rows, origin: options.origin } });
  }

  /** A new query source: a spec on a branch, pulled by a run under the context the run is sent with. */
  createQuery(q: { branch: string; spec?: unknown; expr?: string }, options: { name?: string; description?: string } = {}): Promise<{ source: Source }> {
    return this.call(`/sources${query({ kind: "query", name: options.name, description: options.description })}`, { method: "POST", json: q });
  }

  source(id: string): Promise<Source> {
    return this.call(`/sources/${p(id)}`);
  }

  /** A source's name, description, or the types it is read with (a new version of how it is read). */
  changeSource(id: string, change: { name?: string; description?: string; types?: Record<string, ColumnType> }): Promise<Source> {
    return this.call(`/sources/${p(id)}`, { method: "PATCH", json: change });
  }

  /** A hard delete. A dataset whose output is one of its snapshots gets those rows first. */
  deleteSource(id: string): Promise<{ removed: string; datasets_given_their_rows: string[] }> {
    return this.call(`/sources/${p(id)}`, { method: "DELETE" });
  }

  /** A new file for an upload source. It runs nothing: run the graphs that read it when your app decides. */
  addFile(id: string, csv: Readable | Buffer | string, options: Omit<CsvOptions, "name" | "description"> = {}): Promise<{ source: Source; snapshot: Snapshot }> {
    return this.call(`/sources/${p(id)}/snapshots${csvQuery(options, {})}`, { method: "POST", csv });
  }

  /** New rows for a push source. */
  addRows(id: string, rows: Record<string, unknown>[], origin?: unknown): Promise<{ source: Source; snapshot: Snapshot }> {
    return this.call(`/sources/${p(id)}/snapshots`, { method: "POST", json: { rows, origin } });
  }

  async snapshots(id: string): Promise<Snapshot[]> {
    return (await this.call<{ snapshots: Snapshot[] }>(`/sources/${p(id)}/snapshots`)).snapshots;
  }

  sourceRows(id: string, v: { snapshot?: number; offset?: number; limit?: number; sort?: { column: string; desc?: boolean }[]; filter?: string } = {}): Promise<Page> {
    return this.call(`/sources/${p(id)}/rows${query({ snapshot: v.snapshot, ...view(v) })}`);
  }

  // --------------------------------------------------------- datasets

  async list(): Promise<Dataset[]> {
    return (await this.call<{ datasets: Dataset[] }>(`/datasets`)).datasets;
  }

  /** A new dataset: over a source as it is, or empty until a run makes its output. */
  createDataset(d: { name?: string; description?: string; source?: string }): Promise<Dataset> {
    return this.call(`/datasets`, { method: "POST", json: d });
  }

  get(id: string): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}`);
  }

  delete(id: string): Promise<{ removed: string }> {
    return this.call(`/datasets/${p(id)}`, { method: "DELETE" });
  }

  /** A page of the live output (or `version`), sorted and filtered for viewing only. */
  rows(id: string, v: { version?: number; offset?: number; limit?: number; sort?: { column: string; desc?: boolean }[]; filter?: string } = {}): Promise<Page> {
    return this.call(`/datasets/${p(id)}/rows${query({ version: v.version, ...view(v) })}`);
  }

  lineage(id: string, version?: number): Promise<Record<string, unknown>> {
    return this.call(`/datasets/${p(id)}/lineage${query({ version })}`);
  }

  /** An output version: its manifest, lineage, the run that made it, and the graph it ran. */
  version(id: string, k: number): Promise<{ manifest: Record<string, unknown>; lineage?: unknown; run?: RunRecord; graph?: Graph }> {
    return this.call(`/datasets/${p(id)}/versions/${k}`);
  }

  // ------------------------------------------------------------- runs

  /**
   * Runs a graph into a dataset. `id` is yours: send the same id again
   * and zsqld answers what the first did, never running it twice. A
   * graph refused before it runs throws, with `problems`. `context` is
   * the security context of the user it runs for: its query sources are
   * pulled, and read, as that context only.
   */
  run(ask: { id: string; dataset: string; graph: Graph; context?: unknown; connection?: Record<string, unknown>; wait?: number }): Promise<RunRecord> {
    return this.call(`/runs`, { method: "POST", json: ask });
  }

  /** A dry run: what a run would read and pull, each node's columns, every problem. Nothing runs. */
  check(graph: Graph, context?: unknown): Promise<Checked> {
    return this.call(`/runs?check=true`, { method: "POST", json: { graph, context } });
  }

  runRecord(id: string): Promise<RunRecord> {
    return this.call(`/runs/${p(id)}`);
  }

  /** A graph run up to `node`, over a sample unless `whole`, kept nowhere. */
  preview(ask: { graph: Graph; node?: string; whole?: boolean; limit?: number; context?: unknown; connection?: Record<string, unknown>; wait?: number }): Promise<Previewed> {
    return this.call(`/preview`, { method: "POST", json: ask });
  }

  // ----------------------------------------------------------- blends

  /** The model's fields on a branch: what a blend's projections and join keys name. */
  async fields(branch: string): Promise<ModelField[]> {
    return (await this.call<{ fields: ModelField[] }>(`/branches/${p(branch)}/fields`)).fields;
  }

  /** The model's part run in the warehouse, the datasets joined to it as `blend` says. */
  blend(branch: string, ask: BlendRequest): Promise<Blended> {
    return this.call(`/branches/${p(branch)}/blend`, { method: "POST", json: ask });
  }
}

function csvQuery(o: CsvOptions, extra: Record<string, string | boolean | undefined>): string {
  return query({
    ...extra,
    name: o.name,
    description: o.description,
    file_name: o.fileName,
    delimiter: o.delimiter,
    quote: o.quote,
    header: o.header,
    max_bad_rows: o.maxBadRows,
  });
}

/** Calls `work` again once, after the wait zsqld asked for, when it was busy. */
export async function withRetry<T>(work: () => Promise<T>): Promise<T> {
  try {
    return await work();
  } catch (e) {
    if (!(e instanceof ZsqlError) || !e.retryable) throw e;
    await new Promise((r) => setTimeout(r, Math.min((e.retryAfter ?? 1) * 1000, 5000)));
    return work();
  }
}
