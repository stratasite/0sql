// A typed client for 0sql's data prep API: datasets, their recipes,
// edits and lineage, and blends with the model. It is the part of this
// example meant to be copied: it has no dependency beyond Node's fetch.
//
// It runs on your backend, never in a browser. zsqld keeps no user: a
// dataset is its project's, addressed by its id, and zsqld answers
// whoever reaches it. Which user may read or change which dataset is
// your application's to decide, before it calls anything here; see
// `src/registry.ts` for how this example decides.
//
// See docs/ingestion.md in the 0sql repository for what each call means.

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

export interface SourceState {
  id: string;
  kind: "upload" | "semantic";
  snapshot: number;
  columns: Column[];
  query?: { branch: string; spec?: unknown; expr?: string };
}

export interface Unresolved {
  column: string;
  step: number;
  op: string;
  edit?: string;
  dataset?: string;
}

export interface Dataset {
  id: string;
  name: string;
  created: string;
  updated: string;
  /** The recipe's latest version: what a change is made `against`. */
  recipe: number;
  /** The version the dataset answers with. */
  live: number | null;
  made: number;
  sources: SourceState[];
  columns: Column[];
  rows: number;
  /** What the last job said: a column gone, a type widened, a cascade. */
  notes?: string[];
  /** Set when columns the recipe reads are gone upstream; `live` is older. */
  held?: { since: string; unresolved: Unresolved[] };
  /** How much of the edit log is in force. */
  edits: number;
  backup?: { at: string; failed?: string };
  /** A semantic query still running for it, or one that failed. */
  pending?: { result: string; since: string; failed?: string; interrupted?: Interrupted };
  /** A job a restart found in flight and did not run again; show `reason`. Ends with the dataset's next job. */
  interrupted?: Interrupted;
}

/** Why a job did not finish: the process died with it (`crash`), or the server was stopped while it ran (`stop`); nothing is resumed. */
export interface Interrupted {
  job: "replace" | "refresh" | "create" | "change" | "confirm" | "edit" | "delete" | "update" | "cascade";
  started: string;
  found: string;
  cause: "crash" | "stop";
  reason: string;
}

export interface Page {
  version: number;
  columns: Column[];
  total: number;
  offset: number;
  rows: unknown[][];
}

/** One step of a recipe. Each is a closed shape; see docs/ingestion.md, "Steps". */
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
  | { op: "sql"; query: string }
  | { op: "append"; dataset: string }
  | {
      op: "join";
      dataset: string;
      on: { left: string; right: string }[];
      kind?: "left" | "inner";
      columns?: string[];
      prefix?: string;
    };

export interface Recipe {
  version: number;
  created: string;
  roles?: Record<string, Role>;
  steps?: Step[];
  key?: string[];
}

/** A new recipe, whole, made against the version the user was looking at. */
export interface RecipeChange {
  against: number;
  roles?: Record<string, Role>;
  steps?: Step[];
  key?: string[];
}

/** An edit as the grid makes it. */
export type Edit =
  | {
      op: "cell";
      column: string;
      /** The row as the grid showed it. */
      row?: Record<string, unknown>;
      /** For a dimension remap: the value changed, when no row is sent. */
      from?: unknown;
      to: unknown;
      /**
       * Which rows: the columns whose values must be the row's. `[]` is
       * every row with the value (a remap of the column); some columns,
       * the rows that agree on them; the row's whole identity, that row.
       * Left out: a dimension edit is of the column, a measure edit of the row.
       */
      match?: string[];
      /** For a measure: every row `match` names, when it names several. */
      all?: boolean;
    }
  | { op: "delete"; row: Record<string, unknown>; all?: boolean }
  | { op: "add"; values: Record<string, unknown> }
  | { op: "undo"; edit: string };

export interface EditOutcome {
  id: string;
  state: "applied" | "orphaned" | "ambiguous" | "flagged" | "conflict";
  detail?: string;
}

export interface CsvOptions {
  /** Shown in the app; the file's name without `.csv` when not said. */
  name?: string;
  /** Kept in the lineage. */
  fileName?: string;
  delimiter?: string;
  quote?: string;
  header?: boolean;
  maxBadRows?: number;
}

export interface Preview {
  columns: Column[];
  rows: unknown[][];
  unresolved: string[];
  sample: number;
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

// ------------------------------------------------------------------ errors

/** A refusal from zsqld, in its own words. `message` is written to be shown to a user. */
export class ZsqlError extends Error {
  constructor(
    readonly status: number,
    /** `Invalid`, `NotFound`, `Conflict`, `Unprocessable`, `Busy`, `Internal`, ... */
    readonly kind: string,
    message: string,
    /** Seconds to wait before asking again, when zsqld said. */
    readonly retryAfter?: number,
  ) {
    super(message);
    this.name = "ZsqlError";
  }

  /**
   * Busy (503: nothing was done) and a dataset busy with another job
   * (409 Conflict) are worth one more try. A 503 Timeout is not: the work
   * goes on after the answer and may well land, and an edit sent again
   * would be made twice. Read the dataset again instead.
   */
  get retryable(): boolean {
    return (this.status === 503 && this.kind === "Busy") || (this.status === 409 && /busy/.test(this.message));
  }
}

// ------------------------------------------------------------------ client

export interface ClientOptions {
  /** Where zsqld is: `http://127.0.0.1:3699`. */
  baseUrl: string;
  /** The org the datasets are in; `default` when not said. */
  org?: string;
  /**
   * How long a call may take. zsqld's deadline for data prep is 90 seconds,
   * so a little more than that, to read its own answer rather than give up first.
   */
  timeoutMs?: number;
  fetch?: typeof fetch;
}

/** zsqld's data prep, for an org. {@link DataPrep.project} gives the calls on one project's datasets. */
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

  /** The calls on one project's datasets. */
  project(project: string): ProjectClient {
    const base = `/projects/${encodeURIComponent(project)}`;
    return new ProjectClient(<T>(path: string, init?: Call) => this.call<T>(base + path, init));
  }

  /** Brings back from the object store every dataset of the org this node lacks. */
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
      const err = (parsed as { error?: { class?: string; message?: string } } | undefined)?.error;
      const retry = res.headers.get("retry-after");
      throw new ZsqlError(res.status, err?.class ?? "Http", err?.message ?? (text || res.statusText), retry ? Number(retry) : undefined);
    }
    // A 202 (a query still running) is an answer too: the caller reads `pending`.
    return parsed as T;
  }
}

interface Call {
  method?: string;
  json?: unknown;
  csv?: Readable | Buffer | string;
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

/** One project's datasets. Made by {@link DataPrep.project}. */
export class ProjectClient {
  constructor(private readonly call: <T>(path: string, init?: Call) => Promise<T>) {}

  async list(): Promise<Dataset[]> {
    return (await this.call<{ datasets: Dataset[] }>(`/datasets`)).datasets;
  }

  /** A new dataset from a CSV. The project must be deployed on some branch. */
  create(csv: Readable | Buffer | string, options: CsvOptions = {}): Promise<Dataset> {
    return this.call(`/datasets${csvQuery(options)}`, { method: "POST", csv });
  }

  /** A new snapshot of the file; the dataset's recipe and edits run over it again. */
  replace(id: string, csv: Readable | Buffer | string, options: Omit<CsvOptions, "name"> = {}): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}/upload${csvQuery(options)}`, { method: "PUT", csv });
  }

  /** A dataset, or, for one whose query is still running, what it waits on. */
  get(id: string): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}`);
  }

  /** A hard delete. One others read is refused unless `confirm`. */
  delete(id: string, confirm = false): Promise<{ removed: string }> {
    return this.call(`/datasets/${p(id)}${query({ confirm: confirm || undefined })}`, { method: "DELETE" });
  }

  /** A page of the live version (or `version`), sorted and filtered for viewing only. */
  rows(
    id: string,
    view: { offset?: number; limit?: number; version?: number; sort?: { column: string; desc?: boolean }[]; filter?: string } = {},
  ): Promise<Page> {
    const sort = view.sort?.map((k) => `${k.column}${k.desc ? ":desc" : ""}`).join(",");
    return this.call(
      `/datasets/${p(id)}/rows${query({ offset: view.offset, limit: view.limit, version: view.version, sort, filter: view.filter })}`,
    );
  }

  recipe(id: string, version?: number): Promise<Recipe> {
    return this.call(`/datasets/${p(id)}/recipe${query({ version })}`);
  }

  /** A new recipe version. 409 when `against` is not the latest: read it again. */
  changeRecipe(id: string, change: RecipeChange): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}/recipe`, { method: "POST", json: change });
  }

  /** Steps run over a sample, kept nowhere: what the grid shows while a step is made. */
  preview(id: string, ask: { steps?: Step[]; roles?: Record<string, Role>; upto?: number; limit?: number }): Promise<Preview> {
    return this.call(`/datasets/${p(id)}/preview`, { method: "POST", json: ask });
  }

  /** What reads a column, before it is removed. */
  impact(id: string, column: string): Promise<{ column: string; readers: Unresolved[]; said: string[] }> {
    return this.call(`/datasets/${p(id)}/impact${query({ column })}`);
  }

  /** Past a hold: a recipe version without the steps that held it. */
  confirm(id: string, against: number): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}/confirm`, { method: "POST", json: { against } });
  }

  /** Cell edits, as one entry of the log. */
  edit(id: string, against: number, edits: Edit[]): Promise<{ dataset: Dataset; outcomes: EditOutcome[] }> {
    return this.call(`/datasets/${p(id)}/edits`, { method: "POST", json: { against, edits } });
  }

  editLog(id: string): Promise<{ entries: unknown[]; outcomes: EditOutcome[] }> {
    return this.call(`/datasets/${p(id)}/edits`);
  }

  lineage(id: string, version?: number): Promise<Record<string, unknown>> {
    return this.call(`/datasets/${p(id)}/lineage${query({ version })}`);
  }

  /** How a source's snapshot was parsed: delimiter, header, bad rows, renames. */
  source(id: string, source = "s1", snapshot?: number): Promise<Record<string, unknown>> {
    return this.call(`/datasets/${p(id)}/sources/${p(source)}${query({ snapshot })}`);
  }

  downstream(id: string): Promise<{ downstream: { id: string; name: string; live: number | null }[] }> {
    return this.call(`/datasets/${p(id)}/downstream`);
  }

  /** A dataset made from a semantic query, run fresh under `context`. A 202 answer has `pending`. */
  fromQuery(
    ask: { branch: string; spec?: unknown; expr?: string; name?: string; context?: unknown; connection?: Record<string, unknown>; wait?: number },
  ): Promise<Dataset> {
    return this.call(`/datasets/query`, { method: "POST", json: ask });
  }

  refresh(id: string, ask: { context?: unknown; connection?: Record<string, unknown>; wait?: number } = {}): Promise<Dataset> {
    return this.call(`/datasets/${p(id)}/refresh`, { method: "POST", json: ask });
  }

  /** The model's fields on a branch: what a blend's projections and join keys name. */
  async fields(branch: string): Promise<ModelField[]> {
    return (await this.call<{ fields: ModelField[] }>(`/branches/${p(branch)}/fields`)).fields;
  }

  /** The model's part run in the warehouse, the datasets joined to it as `blend` says. */
  blend(branch: string, ask: BlendRequest): Promise<Blended> {
    return this.call(`/branches/${p(branch)}/blend`, { method: "POST", json: ask });
  }
}

function csvQuery(o: CsvOptions): string {
  return query({
    name: o.name,
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
