// The app's backend: it signs users in, decides who may use which
// dataset, and forwards what it allows to zsqld. The browser talks only
// to this; it never sees zsqld.
//
// zsqld keeps no user, so this is where access control lives. Every
// route on a dataset goes through `allowed()` first, which says whether
// the user signed in may read it or change it (src/registry.ts); a
// request that is not allowed never reaches zsqld.

import Fastify, { type FastifyInstance, type FastifyRequest } from "fastify";
import cookie from "@fastify/cookie";
import multipart from "@fastify/multipart";
import staticFiles from "@fastify/static";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import type { Config } from "./config.js";
import { Registry, type Access } from "./registry.js";
import { type BlendRequest, DataPrep, type Edit, type RecipeChange, type Step, withRetry, ZsqlError } from "./zsql.js";

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

/** A refusal of this app's own, before anything reaches zsqld. */
class Refused extends Error {
  constructor(
    readonly status: number,
    readonly kind: string,
    message: string,
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
   * dataset `id`. `need` is what the route does. A dataset the user may
   * not see at all answers as if it were not there.
   */
  function allowed(req: FastifyRequest, id: string, need: Access): { user: User; access: Access } {
    const u = signedIn(req);
    const access = registry.access(u.name, id);
    if (!access) throw new Refused(404, "NotFound", `no dataset ${id}`);
    if (need === "owner" && access !== "owner") {
      throw new Refused(403, "Forbidden", "this dataset is shared with you to read; only its owner may change it");
    }
    return { user: u, access };
  }

  /**
   * The datasets a recipe's steps read (a join's, an append's), each one
   * the user must be allowed to read: a step reads another dataset whole,
   * so a preview would show it and a recipe would copy it. A recipe
   * saved keeps reading what it named when zsqld runs it again on a new
   * file upstream; take a dataset out of the recipes that read it when
   * you take away someone's access to it.
   */
  function readsAllowed(req: FastifyRequest, steps: Step[] | undefined): void {
    for (const step of steps ?? []) {
      const read = (step as { dataset?: unknown }).dataset;
      if (typeof read === "string") allowed(req, read, "reader");
    }
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
    if (err instanceof Refused) return reply.code(err.status).send({ error: { class: err.kind, message: err.message } });
    if (err instanceof ZsqlError) {
      if (err.retryAfter) void reply.header("retry-after", String(err.retryAfter));
      return reply.code(err.status).send({ error: { class: err.kind, message: err.message } });
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
    return { ...d, access, owner: e?.owner, sharedWith: access === "owner" ? (e?.sharedWith ?? []) : undefined };
  }

  // zsqld lists every dataset of the project; the user sees the ones this
  // app lets them.
  app.get("/api/datasets", async (req) => {
    const u = signedIn(req);
    const visible = registry.visible(u.name);
    const all = await withRetry(() => zsql.list());
    return { datasets: all.filter((d) => visible.has(d.id)).map((d) => shown(d, visible.get(d.id)!)) };
  });

  app.post<{ Querystring: { name?: string } }>("/api/datasets", async (req, reply) => {
    const u = signedIn(req);
    const file = await upload(req);
    // A streamed body cannot be sent twice, so an upload is not retried.
    const d = await zsql.create(file.file, { name: req.query.name || undefined, fileName: file.filename });
    registry.add(d.id, u.name);
    return reply.code(201).send(shown(d, "owner"));
  });

  app.get<{ Params: { id: string } }>("/api/datasets/:id", async (req) => {
    const { access } = allowed(req, req.params.id, "reader");
    return shown(await withRetry(() => zsql.get(req.params.id)), access);
  });

  app.put<{ Params: { id: string } }>("/api/datasets/:id/file", async (req) => {
    allowed(req, req.params.id, "owner");
    const file = await upload(req);
    return shown(await zsql.replace(req.params.id, file.file, { fileName: file.filename }), "owner");
  });

  app.delete<{ Params: { id: string }; Querystring: { confirm?: string } }>("/api/datasets/:id", async (req) => {
    allowed(req, req.params.id, "owner");
    const done = await zsql.delete(req.params.id, req.query.confirm === "true");
    registry.remove(req.params.id);
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

  app.get<{ Params: { id: string } }>("/api/datasets/:id/recipe", async (req) => {
    allowed(req, req.params.id, "reader");
    return withRetry(() => zsql.recipe(req.params.id));
  });

  app.post<{ Params: { id: string }; Body: RecipeChange }>("/api/datasets/:id/recipe", async (req) => {
    allowed(req, req.params.id, "owner");
    readsAllowed(req, req.body?.steps);
    return shown(await withRetry(() => zsql.changeRecipe(req.params.id, req.body)), "owner");
  });

  app.post<{ Params: { id: string }; Body: { steps?: Step[]; limit?: number } }>("/api/datasets/:id/preview", async (req) => {
    allowed(req, req.params.id, "owner");
    readsAllowed(req, req.body?.steps);
    return withRetry(() => zsql.preview(req.params.id, req.body ?? {}));
  });

  app.get<{ Params: { id: string }; Querystring: { column: string } }>("/api/datasets/:id/impact", async (req) => {
    allowed(req, req.params.id, "owner");
    return withRetry(() => zsql.impact(req.params.id, req.query.column));
  });

  app.post<{ Params: { id: string }; Body: { against: number } }>("/api/datasets/:id/confirm", async (req) => {
    allowed(req, req.params.id, "owner");
    return shown(await withRetry(() => zsql.confirm(req.params.id, req.body.against)), "owner");
  });

  app.get<{ Params: { id: string } }>("/api/datasets/:id/edits", async (req) => {
    allowed(req, req.params.id, "reader");
    return withRetry(() => zsql.editLog(req.params.id));
  });

  app.post<{ Params: { id: string }; Body: { against: number; edits: Edit[] } }>("/api/datasets/:id/edits", async (req) => {
    allowed(req, req.params.id, "owner");
    const done = await withRetry(() => zsql.edit(req.params.id, req.body.against, req.body.edits));
    return { ...done, dataset: shown(done.dataset, "owner") };
  });

  app.get<{ Params: { id: string } }>("/api/datasets/:id/lineage", async (req) => {
    allowed(req, req.params.id, "reader");
    const [lineage, source] = await Promise.all([zsql.lineage(req.params.id), zsql.source(req.params.id)]);
    return { lineage, source };
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
