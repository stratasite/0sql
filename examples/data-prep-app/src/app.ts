// The app's backend: it signs users in, decides who may use which
// dataset, keeps each dataset's pipeline, and sends zsqld what it
// allows. The browser talks only to this; it never sees zsqld.
//
// zsqld keeps no user, so this is where access control lives. Every
// route on a dataset goes through `allowed()` first, which says whether
// the user signed in may read it or change it (src/registry.ts); a
// request that is not allowed never reaches zsqld.
//
// zsqld keeps no pipeline either: it runs the graph it is sent. This
// app keeps each dataset's graph and its versions (src/pipelines.ts),
// places the user's grid edits in it, and decides when it runs: when a
// user saves a change, edits a cell, or uploads a new file.

import Fastify, { type FastifyInstance, type FastifyRequest } from "fastify";
import cookie from "@fastify/cookie";
import multipart from "@fastify/multipart";
import staticFiles from "@fastify/static";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import type { Config } from "./config.js";
import { randomUUID } from "node:crypto";
import { Pipelines } from "./pipelines.js";
import { Registry, type Access } from "./registry.js";
import { type BlendRequest, DataPrep, type Edit, type Graph, type Problem, type RunRecord, withRetry, ZsqlError } from "./zsql.js";

/** Who is signed in. In your app, this is your own session. */
export interface User {
  name: string;
}

const SESSION = "session";
/** How long a sign-in lasts. */
const SESSION_SECONDS = 8 * 60 * 60;
const NAME = /^[a-z0-9][a-z0-9_-]{0,39}$/;

/**
 * Who may use datasets at all: if datasets are for developers only,
 * this is where your app says so. Everyone, here.
 */
export function mayUseDatasets(_user: User): boolean {
  return true;
}

/** A refusal of this app's own, before anything reaches zsqld, or a graph zsqld refused with its problems. */
class Refused extends Error {
  constructor(
    readonly status: number,
    readonly kind: string,
    message: string,
    readonly problems?: Problem[],
  ) {
    super(message);
  }
}

export async function buildApp(config: Config, options: { fetch?: typeof fetch; logger?: boolean } = {}): Promise<FastifyInstance> {
  const app = Fastify({ logger: options.logger ?? true, bodyLimit: 4 * 1024 * 1024 });
  await app.register(cookie, { secret: config.sessionSecret });
  await app.register(multipart, { limits: { fileSize: config.maxUploadBytes, files: 1 } });
  const here = dirname(fileURLToPath(import.meta.url));
  await app.register(staticFiles, { root: join(here, "..", "public") });

  const zsql = new DataPrep({ baseUrl: config.zsqldUrl, org: config.org, ...(options.fetch ? { fetch: options.fetch } : {}) }).project(
    config.project,
  );
  const registry = new Registry(join(config.dataDir, "registry.json"));
  const pipelines = new Pipelines(join(config.dataDir, "pipelines.json"));

  // ------------------------------------------------------------ the session

  function user(req: FastifyRequest): User | undefined {
    const raw = req.cookies[SESSION];
    if (!raw) return undefined;
    const signed = req.unsignCookie(raw);
    return signed.valid && signed.value && NAME.test(signed.value) ? { name: signed.value } : undefined;
  }

  function signedIn(req: FastifyRequest): User {
    const u = user(req);
    if (!u) throw new Refused(401, "Unauthorized", "sign in first");
    if (!mayUseDatasets(u)) throw new Refused(403, "Forbidden", "datasets are not open to you");
    return u;
  }

  /**
   * The access check: the user signed in, and what they may do with
   * dataset or source `id`. `need` is what the route does. One the user
   * may not see at all answers as if it were not there.
   */
  function allowed(req: FastifyRequest, id: string, need: Access): { user: User; access: Access } {
    const u = signedIn(req);
    const access = registry.access(u.name, id);
    if (!access) throw new Refused(404, "NotFound", `no dataset or source ${id}`);
    if (need === "owner" && access !== "owner") {
      throw new Refused(403, "Forbidden", "this is shared with you to read; only its owner may change it, or use its sources");
    }
    return { user: u, access };
  }

  // A DEMO SIGN-IN: anyone may sign in as any name, with no password.
  // Replace this route with your own sign-in; nothing below it changes.
  app.post<{ Body: { name?: string } }>("/api/session", async (req, reply) => {
    const name = (req.body?.name ?? "").trim().toLowerCase();
    if (!NAME.test(name)) throw new Refused(400, "Invalid", "a name is lower-case letters, digits, - and _");
    reply.setCookie(SESSION, name, { signed: true, httpOnly: true, sameSite: "lax", secure: "auto", maxAge: SESSION_SECONDS, path: "/" });
    return { name };
  });

  app.delete("/api/session", async (_req, reply) => {
    reply.clearCookie(SESSION, { path: "/" });
    return { signedOut: true };
  });

  app.get("/api/me", async (req) => ({ user: user(req) ?? null, project: config.project, org: config.org, branch: config.branch }));

  // ------------------------------------------------------------ errors

  // This app's refusals, and zsqld's, go back with their status and their
  // words, which are written for a user to read. Anything else is this
  // app's fault, or zsqld out of reach.
  app.setErrorHandler((err, req, reply) => {
    if (err instanceof Refused) return reply.code(err.status).send({ error: { class: err.kind, message: err.message }, problems: err.problems });
    if (err instanceof ZsqlError) {
      if (err.retryAfter) void reply.header("retry-after", String(err.retryAfter));
      return reply.code(err.status).send({ error: { class: err.kind, message: err.message }, problems: err.problems });
    }
    const status = (err as { statusCode?: number }).statusCode;
    const message = err instanceof Error ? err.message : String(err);
    if (status && status < 500) return reply.code(status).send({ error: { class: "Invalid", message } });
    req.log.error(err);
    return reply.code(502).send({ error: { class: "Upstream", message: "the datasets service could not be reached" } });
  });

  // ------------------------------------------------------------ datasets

  /** The uploaded file, streamed: never held whole here. */
  async function upload(req: FastifyRequest) {
    const file = await req.file();
    if (!file) throw new Refused(400, "Invalid", "send the CSV as a file field");
    return file;
  }

  /** A dataset as the browser sees it: with what this user may do, its owner, and, for the owner, who it is shared with. */
  function shown<T extends { id: string }>(d: T, access: Access) {
    const e = registry.entry(d.id);
    return { ...d, access, owner: e?.owner, sharedWith: access === "owner" ? (e?.sharedWith ?? []) : undefined, pipeline: pipelines.latest(d.id)?.version };
  }

  /**
   * What a graph reads, each one the user must be allowed to read: a
   * source they imported, or a dataset they may read. A preview would
   * show it, and a run would copy it into the dataset. zsqld says the
   * same in `reads` on every answer; this checks before it is asked.
   */
  function readsAllowed(req: FastifyRequest, graph: Graph | undefined): void {
    if (!graph || !Array.isArray(graph.nodes)) throw new Refused(400, "Invalid", "a pipeline is a graph: {\"nodes\": [...]}");
    for (const n of graph.nodes) {
      if (n.kind === "source") allowed(req, n.source, "owner");
      if (n.kind === "dataset") allowed(req, n.dataset, "reader");
    }
  }

  /** The graph of a dataset that is a source as it is. */
  const asItIs = (source: string): Graph => ({
    nodes: [
      { id: "source", kind: "source", source },
      { id: "output", kind: "output", input: "source" },
    ],
  });

  /**
   * Runs a graph into a dataset, under a new run id of this app's. A
   * graph zsqld refuses before running is answered 422 with its
   * problems, and not kept; one that runs is kept as the pipeline's next
   * version, whatever it came to, so the user sees what it did.
   */
  async function runAndKeep(user: User, dataset: string, graph: Graph) {
    const id = `run-${randomUUID()}`;
    let run: RunRecord;
    try {
      // Sent again with the same id, a run is answered, never made twice.
      run = await withRetry(() => zsql.run({ id, dataset, graph, context: contextOf(user) }));
    } catch (e) {
      if (e instanceof ZsqlError && e.problems) throw new Refused(422, "Refused", e.message, e.problems);
      throw e;
    }
    const version = pipelines.save(dataset, graph, user.name, id);
    return { run, pipeline: version.version };
  }

  /**
   * The security context a run's query sources are pulled as: the user
   * signed in, from your session. Never take one from a browser.
   */
  function contextOf(_user: User): unknown {
    return undefined;
  }

  // zsqld lists every dataset of the project; the user sees the ones this
  // app lets them.
  app.get("/api/datasets", async (req) => {
    const u = signedIn(req);
    const visible = registry.visible(u.name);
    const all = await withRetry(() => zsql.list());
    return { datasets: all.filter((d) => visible.has(d.id)).map((d) => shown(d, visible.get(d.id)!)) };
  });

  // An upload used as it is: a source, and a dataset over it that copies nothing.
  app.post<{ Querystring: { name?: string } }>("/api/datasets", async (req, reply) => {
    const u = signedIn(req);
    const file = await upload(req);
    // A streamed body cannot be sent twice, so an upload is not retried.
    const made = await zsql.createUpload(file.file, { name: req.query.name || undefined, fileName: file.filename, dataset: true });
    const d = made.dataset!;
    registry.add(made.source.id, u.name, { kind: "source" });
    registry.add(d.id, u.name, { kind: "dataset", source: made.source.id });
    pipelines.save(d.id, asItIs(made.source.id), u.name);
    return reply.code(201).send(shown(d, "owner"));
  });

  // The explorer: the sources a user may build a pipeline on.
  app.get("/api/sources", async (req) => {
    const u = signedIn(req);
    const visible = registry.visible(u.name, "source");
    const all = await withRetry(() => zsql.sources());
    return { sources: all.filter((s) => visible.has(s.id)) };
  });

  app.get<{ Params: { id: string } }>("/api/datasets/:id", async (req) => {
    const { access } = allowed(req, req.params.id, "reader");
    return shown(await withRetry(() => zsql.get(req.params.id)), access);
  });

  // A new version of the file: a snapshot of its source, then this
  // app runs the dataset's pipeline over it. zsqld runs nothing on its
  // own; what else reads the source is run when its owner says.
  app.put<{ Params: { id: string } }>("/api/datasets/:id/file", async (req) => {
    const { user } = allowed(req, req.params.id, "owner");
    const source = registry.entry(req.params.id)?.source;
    if (!source) throw new Refused(400, "Invalid", "this dataset was not imported from a file; change its pipeline instead");
    const file = await upload(req);
    await zsql.addFile(source, file.file, { fileName: file.filename });
    const graph = pipelines.latest(req.params.id)?.graph ?? asItIs(source);
    const done = await runAndKeep(user, req.params.id, graph);
    return { ...done, dataset: shown(await zsql.get(req.params.id), "owner") };
  });

  // A dataset goes, and its file's source with it unless another
  // pipeline reads it: which ones do is this app's to know, since it
  // keeps the pipelines.
  app.delete<{ Params: { id: string } }>("/api/datasets/:id", async (req) => {
    allowed(req, req.params.id, "owner");
    const id = req.params.id;
    const readers = pipelines.readers(id).filter((d) => d !== id);
    if (readers.length) throw new Refused(409, "Conflict", `the pipelines of ${readers.join(", ")} read this dataset; change them first`);
    const done = await zsql.delete(id);
    const source = registry.entry(id)?.source;
    pipelines.remove(id);
    registry.remove(id);
    if (source && pipelines.readers(source).length === 0) {
      await zsql.deleteSource(source);
      registry.remove(source);
    }
    return done;
  });

  app.post<{ Params: { id: string }; Body: { user?: string } }>("/api/datasets/:id/share", async (req) => {
    allowed(req, req.params.id, "owner");
    const who = (req.body?.user ?? "").trim().toLowerCase();
    if (!NAME.test(who)) throw new Refused(400, "Invalid", "share with a user's name");
    registry.share(req.params.id, who);
    return { sharedWith: registry.entry(req.params.id)?.sharedWith ?? [] };
  });

  app.delete<{ Params: { id: string; user: string } }>("/api/datasets/:id/share/:user", async (req) => {
    allowed(req, req.params.id, "owner");
    registry.unshare(req.params.id, req.params.user);
    return { sharedWith: registry.entry(req.params.id)?.sharedWith ?? [] };
  });

  app.get<{ Params: { id: string }; Querystring: { offset?: string; limit?: string; sort?: string; desc?: string; filter?: string; version?: string } }>(
    "/api/datasets/:id/rows",
    async (req) => {
      allowed(req, req.params.id, "reader");
      const q = req.query;
      return withRetry(() =>
        zsql.rows(req.params.id, {
          offset: q.offset ? Number(q.offset) : undefined,
          limit: q.limit ? Number(q.limit) : undefined,
          version: q.version ? Number(q.version) : undefined,
          sort: q.sort ? [{ column: q.sort, desc: q.desc === "true" }] : undefined,
          filter: q.filter || undefined,
        }),
      );
    },
  );

  // The pipeline, as this app keeps it, and what its last run did there.
  app.get<{ Params: { id: string } }>("/api/datasets/:id/pipeline", async (req) => {
    allowed(req, req.params.id, "reader");
    const v = pipelines.latest(req.params.id);
    if (!v) throw new Refused(404, "NotFound", "this dataset has no pipeline");
    const run = v.run ? await withRetry(() => zsql.runRecord(v.run!)).catch(() => undefined) : undefined;
    return { ...v, lastRun: run };
  });

  // A new version of the pipeline, made against the one the user saw, and run.
  app.put<{ Params: { id: string }; Body: { graph: Graph; against?: number } }>("/api/datasets/:id/pipeline", async (req) => {
    const { user } = allowed(req, req.params.id, "owner");
    readsAllowed(req, req.body?.graph);
    const latest = pipelines.latest(req.params.id)?.version ?? 0;
    if (req.body.against !== undefined && req.body.against !== latest) {
      throw new Refused(409, "Conflict", `this was made against version ${req.body.against} of the pipeline, and it is at ${latest} now; read it again`);
    }
    const done = await runAndKeep(user, req.params.id, req.body.graph);
    return { ...done, dataset: shown(await zsql.get(req.params.id), "owner") };
  });

  // A graph, or one node of it, over a sample: kept nowhere.
  app.post<{ Params: { id: string }; Body: { graph?: Graph; node?: string; limit?: number } }>("/api/datasets/:id/preview", async (req) => {
    const { user } = allowed(req, req.params.id, "owner");
    const graph = req.body?.graph ?? pipelines.latest(req.params.id)?.graph;
    readsAllowed(req, graph);
    return withRetry(() => zsql.preview({ graph: graph!, node: req.body?.node, limit: req.body?.limit, context: contextOf(user) }));
  });

  // A dry run: what a run would read and do, and what is wrong.
  app.post<{ Params: { id: string }; Body: { graph: Graph } }>("/api/datasets/:id/check", async (req) => {
    const { user } = allowed(req, req.params.id, "owner");
    readsAllowed(req, req.body?.graph);
    return withRetry(() => zsql.check(req.body.graph, contextOf(user)));
  });

  /**
   * Edits made in the grid go in an `edits` node this app places just
   * before the output, made the first time, with the key the browser
   * names rows by. Each edit names its row by value, so it holds when
   * what is upstream changes.
   */
  app.post<{ Params: { id: string }; Body: { edits?: Omit<Edit, "id">[]; undo?: string; key?: string[] } }>("/api/datasets/:id/edits", async (req) => {
    const { user } = allowed(req, req.params.id, "owner");
    const latest = pipelines.latest(req.params.id);
    if (!latest) throw new Refused(404, "NotFound", "this dataset has no pipeline");
    const graph: Graph = structuredClone(latest.graph);
    const output = graph.nodes.find((n) => n.kind === "output");
    if (!output || output.kind !== "output") throw new Refused(400, "Invalid", "the pipeline has no output");
    let node = graph.nodes.find((n) => n.kind === "edits" && n.id === "edits");
    if (!node) {
      node = { id: "edits", kind: "edits", input: output.input, key: req.body?.key ?? [], edits: [], note: "edits made in the grid" };
      graph.nodes.splice(graph.nodes.indexOf(output), 0, node);
      output.input = "edits";
    }
    if (node.kind !== "edits") throw new Refused(400, "Invalid", "node edits is not an edits node");
    if (req.body?.undo) {
      node.edits = node.edits.filter((e) => e.id !== req.body.undo);
    }
    const next = node.edits.reduce((m, e) => Math.max(m, Number(e.id.replace(/^e/, "")) || 0), 0);
    (req.body?.edits ?? []).forEach((e, i) => node!.kind === "edits" && node!.edits.push({ ...e, id: `e${next + i + 1}` } as Edit));
    const done = await runAndKeep(user, req.params.id, graph);
    return { ...done, dataset: shown(await zsql.get(req.params.id), "owner") };
  });

  // A run's record: what each node did.
  app.get<{ Params: { id: string; run: string } }>("/api/datasets/:id/runs/:run", async (req) => {
    allowed(req, req.params.id, "reader");
    const r = await withRetry(() => zsql.runRecord(req.params.run));
    if (r.dataset !== req.params.id) throw new Refused(404, "NotFound", `no run ${req.params.run} of this dataset`);
    return r;
  });

  app.get<{ Params: { id: string } }>("/api/datasets/:id/lineage", async (req) => {
    allowed(req, req.params.id, "reader");
    return { lineage: await zsql.lineage(req.params.id) };
  });

  // The model's fields, for writing a blend: the model is the project's, not any user's.
  app.get("/api/fields", async (req) => {
    signedIn(req);
    return { fields: (await withRetry(() => zsql.fields(config.branch))).filter((f) => !f.hidden) };
  });

  // A blend reads every dataset it attaches: each must be one the user may read.
  // The request to zsqld is built here, from what a user may choose: the
  // spec, the datasets and the joins, and a page. Never pass on a
  // browser's `context` (it would choose its own security context) or
  // `connection` (it would point zsqld at a host of its choosing); set
  // `context` from your session, for the user signed in.
  app.post<{ Body: BlendRequest }>("/api/blend", async (req) => {
    signedIn(req);
    const b = req.body ?? ({} as BlendRequest);
    for (const d of b.datasets ?? []) allowed(req, d.id, "reader");
    const asked: BlendRequest = { spec: b.spec, datasets: b.datasets, blend: b.blend, offset: b.offset, limit: b.limit };
    return withRetry(() => zsql.blend(config.branch, asked));
  });

  return app;
}
