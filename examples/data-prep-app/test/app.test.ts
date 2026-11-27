// The app against a stand-in for zsqld: what it forwards, and what its
// access check keeps from ever reaching zsqld.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import Fastify from "fastify";
import { buildApp } from "../src/app.js";
import type { Config } from "../src/config.js";

interface Seen {
  method: string;
  url: string;
  headers: Record<string, unknown>;
  body: string;
}

const ID = "ds_0123456789abcdef";
const SRC = "src_0123456789abcdef";
const DATASET = { id: ID, name: "budget", live: 1, made: 1, rows: 2, columns: [] };
const SOURCE = { id: SRC, name: "budget", kind: "upload", snapshot: 1, columns: [], read: 1 };

async function fakeZsqld() {
  const seen: Seen[] = [];
  let uploads = 0;
  const z = Fastify();
  z.addContentTypeParser("*", { parseAs: "string" }, (_req, body, done) => done(null, body));
  z.all("/*", async (req, reply) => {
    seen.push({ method: req.method, url: req.url, headers: req.headers, body: typeof req.body === "string" ? req.body : JSON.stringify(req.body ?? "") });
    const path = req.url.split("?")[0]!;
    if (path.endsWith("/datasets/ds_bad0000000000000")) {
      return reply.code(422).send({ error: { class: "Unprocessable", message: "column 'amount' is integer and arrived as text" } });
    }
    if (req.method === "POST" && path === "/orgs/acme/projects/shop/sources") {
      // The first upload is ann's dataset; each later one is new.
      const n = uploads++;
      const id = (base: string) => (n === 0 ? base : `${base.slice(0, -1)}${n}`);
      return reply.code(201).send({ source: { ...SOURCE, id: id(SRC) }, snapshot: { number: 1 }, dataset: { ...DATASET, id: id(ID) } });
    }
    if (path === "/orgs/acme/projects/shop/datasets") return { datasets: [DATASET, { ...DATASET, id: "ds_ffffffffffffffff", name: "someone else's" }] };
    if (path.endsWith("/rows")) return { version: 1, columns: [], total: 0, offset: 0, rows: [] };
    if (req.method === "POST" && path.endsWith("/runs")) {
      const asked = (typeof req.body === "string" ? JSON.parse(req.body) : req.body) as { id: string; graph: { nodes: { kind: string; steps?: unknown[] }[] } };
      if (asked.graph.nodes.some((n) => n.kind === "steps" && JSON.stringify(n.steps).includes("gone"))) {
        return reply.code(422).send({
          error: { class: "Refused", message: "node shape: step 1 (trim_val) reads column 'gone', which is not there" },
          problems: [{ node: "shape", code: "missing_column", message: "step 1 (trim_val) reads column 'gone', which is not there", column: "gone" }],
        });
      }
      return { id: asked.id, dataset: ID, state: "succeeded", version: 2, reads: [], summary: "Made version 2." };
    }
    if (path.includes("/runs/")) return { id: path.split("/").at(-1), dataset: ID, state: "succeeded", reads: [], summary: "" };
    return DATASET;
  });
  await z.listen({ port: 0, host: "127.0.0.1" });
  const port = (z.server.address() as { port: number }).port;
  return { z, seen, url: `http://127.0.0.1:${port}` };
}

function config(zsqldUrl: string): Config {
  return {
    port: 0,
    zsqldUrl,
    org: "acme",
    project: "shop",
    branch: "main",
    sessionSecret: "a test secret that is long enough",
    dataDir: mkdtempSync(join(tmpdir(), "data-prep-app-")),
    maxUploadBytes: 1024 * 1024,
  };
}

type App = Awaited<ReturnType<typeof buildApp>>;

/** The last run the app sent zsqld. */
function lastRun(seen: Seen[]): Seen {
  const runs = seen.filter((c) => c.url.endsWith("/runs"));
  assert.ok(runs.length, "a run was sent");
  return runs[runs.length - 1]!;
}

async function signIn(app: App, name: string): Promise<string> {
  const res = await app.inject({ method: "POST", url: "/api/session", payload: { name } });
  assert.equal(res.statusCode, 200);
  const cookie = res.cookies.find((c) => c.name === "session");
  assert.ok(cookie);
  return `session=${cookie.value}`;
}

function multipart(fileName: string, content: string) {
  const boundary = "----zsqltest";
  const payload =
    `--${boundary}\r\nContent-Disposition: form-data; name="file"; filename="${fileName}"\r\nContent-Type: text/csv\r\n\r\n` +
    `${content}\r\n--${boundary}--\r\n`;
  return { payload, headers: { "content-type": `multipart/form-data; boundary=${boundary}` } };
}

async function setUp() {
  const fake = await fakeZsqld();
  const cfg = config(fake.url);
  const app = await buildApp(cfg, { logger: false });
  const ann = await signIn(app, "ann");
  const bob = await signIn(app, "bob");
  const done = async () => {
    await app.close();
    await fake.z.close();
    rmSync(cfg.dataDir, { recursive: true, force: true });
  };
  return { ...fake, app, ann, bob, done };
}

async function importAsAnn(app: App, ann: string) {
  const up = multipart("budget.csv", "region,amount\nEMEA,100\n");
  const res = await app.inject({ method: "POST", url: "/api/datasets?name=Q1", payload: up.payload, headers: { ...up.headers, cookie: ann } });
  assert.equal(res.statusCode, 201, res.body);
  return res;
}

test("an upload is streamed to zsqld as it came, as a source and a dataset over it, with no user on it", async () => {
  const { app, ann, seen, done } = await setUp();
  const res = await importAsAnn(app, ann);
  assert.equal(res.json().access, "owner");
  assert.equal(res.json().pipeline, 1, "the app keeps its pipeline: the source as it is");
  const call = seen.at(-1)!;
  assert.equal(call.url, "/orgs/acme/projects/shop/sources?kind=upload&dataset=true&name=Q1&file_name=budget.csv");
  assert.equal(call.headers["content-type"], "text/csv");
  assert.equal(call.body, "region,amount\nEMEA,100\n");
  assert.ok(!Object.keys(call.headers).some((h) => h.includes("owner")), "zsqld is told of no user");
  await done();
});

test("a user sees only the datasets this app lets them; zsqld is never asked for another's", async () => {
  const { app, ann, bob, seen, done } = await setUp();
  await importAsAnn(app, ann);
  const listed = await app.inject({ method: "GET", url: "/api/datasets", headers: { cookie: ann } });
  assert.deepEqual(listed.json().datasets.map((d: { id: string }) => d.id), [ID], "zsqld listed two; ann sees hers");
  const calls = seen.length;
  for (const url of [`/api/datasets/${ID}`, `/api/datasets/${ID}/rows`, `/api/datasets/${ID}/lineage`]) {
    const res = await app.inject({ method: "GET", url, headers: { cookie: bob } });
    assert.equal(res.statusCode, 404, url);
  }
  const blend = await app.inject({ method: "POST", url: "/api/blend", headers: { cookie: bob }, payload: { spec: { projections: [] }, datasets: [{ id: ID, as: "b" }], blend: [] } });
  assert.equal(blend.statusCode, 404);
  assert.equal(seen.length, calls, "nothing of bob's reached zsqld");
  // No session is no access, and a cookie this app did not sign is none.
  assert.equal((await app.inject({ method: "GET", url: "/api/datasets" })).statusCode, 401);
  assert.equal((await app.inject({ method: "GET", url: "/api/datasets", headers: { cookie: "session=ann" } })).statusCode, 401);
  await done();
});

test("a dataset shared to read is read, and not changed", async () => {
  const { app, ann, bob, seen, done } = await setUp();
  await importAsAnn(app, ann);
  const shared = await app.inject({ method: "POST", url: `/api/datasets/${ID}/share`, headers: { cookie: ann }, payload: { user: "bob" } });
  assert.deepEqual(shared.json(), { sharedWith: ["bob"] });
  const rows = await app.inject({ method: "GET", url: `/api/datasets/${ID}/rows`, headers: { cookie: bob } });
  assert.equal(rows.statusCode, 200, rows.body);
  const one = await app.inject({ method: "GET", url: `/api/datasets/${ID}`, headers: { cookie: bob } });
  assert.equal(one.json().access, "reader");
  assert.equal(one.json().sharedWith, undefined, "only the owner is told who else reads it");
  const calls = seen.length;
  const writes = [
    { method: "POST" as const, url: `/api/datasets/${ID}/edits`, payload: { edits: [] } },
    { method: "PUT" as const, url: `/api/datasets/${ID}/pipeline`, payload: { graph: { nodes: [] } } },
    { method: "POST" as const, url: `/api/datasets/${ID}/preview`, payload: {} },
    { method: "DELETE" as const, url: `/api/datasets/${ID}` },
    { method: "POST" as const, url: `/api/datasets/${ID}/share`, payload: { user: "eve" } },
  ];
  for (const w of writes) {
    const res = await app.inject({ ...w, headers: { cookie: bob } });
    assert.equal(res.statusCode, 403, `${w.method} ${w.url}`);
  }
  assert.equal(seen.length, calls, "no write of bob's reached zsqld");
  await app.inject({ method: "DELETE", url: `/api/datasets/${ID}/share/bob`, headers: { cookie: ann } });
  assert.equal((await app.inject({ method: "GET", url: `/api/datasets/${ID}`, headers: { cookie: bob } })).statusCode, 404);
  await done();
});

test("a refusal comes back with zsqld's status and words", async () => {
  const fake = await fakeZsqld();
  const cfg = config(fake.url);
  // ann owns the dataset zsqld refuses, by this app's records.
  const { Registry } = await import("../src/registry.js");
  new Registry(join(cfg.dataDir, "registry.json")).add("ds_bad0000000000000", "ann");
  const app = await buildApp(cfg, { logger: false });
  const cookie = await signIn(app, "ann");
  const res = await app.inject({ method: "GET", url: "/api/datasets/ds_bad0000000000000", headers: { cookie } });
  assert.equal(res.statusCode, 422);
  assert.deepEqual(res.json(), { error: { class: "Unprocessable", message: "column 'amount' is integer and arrived as text" } });
  await app.close();
  await fake.z.close();
  rmSync(cfg.dataDir, { recursive: true, force: true });
});

test("zsqld out of reach is a 502, not a crash", async () => {
  const cfg = config("http://127.0.0.1:9");
  const app = await buildApp(cfg, { logger: false });
  const cookie = await signIn(app, "ann");
  assert.equal((await app.inject({ method: "GET", url: "/api/datasets", headers: { cookie } })).statusCode, 502);
  await app.close();
  rmSync(cfg.dataDir, { recursive: true, force: true });
});

test("a graph that reads a dataset or a source needs the user to be allowed it", async () => {
  const { app, ann, bob, seen, done } = await setUp();
  await importAsAnn(app, ann);
  await app.inject({ method: "POST", url: `/api/datasets/${ID}/share`, headers: { cookie: ann }, payload: { user: "bob" } });
  // bob makes his own dataset, then reaches for what he may not.
  const up = multipart("mine.csv", "a\n1\n");
  const mine = await app.inject({ method: "POST", url: "/api/datasets", payload: up.payload, headers: { ...up.headers, cookie: bob } });
  assert.equal(mine.statusCode, 201, mine.body);
  const theirs = "ds_eeeeeeeeeeeeeeee";
  const calls = seen.length;
  const graph = (input: Record<string, unknown>) => ({ nodes: [{ id: "in", ...input }, { id: "out", kind: "output", input: "in" }] });
  for (const [url, method, payload] of [
    [`/api/datasets/${ID}/preview`, "POST", { graph: graph({ kind: "dataset", dataset: theirs }) }],
    [`/api/datasets/${ID}/pipeline`, "PUT", { graph: graph({ kind: "dataset", dataset: theirs }) }],
  ] as const) {
    const res = await app.inject({ method, url, headers: { cookie: ann }, payload });
    assert.equal(res.statusCode, 404, url);
  }
  // A dataset shared with bob he may build on; ann's source, under it, he may not even see.
  const own = mine.json().id as string;
  const ok = await app.inject({ method: "PUT", url: `/api/datasets/${own}/pipeline`, headers: { cookie: bob }, payload: { graph: graph({ kind: "dataset", dataset: ID }) } });
  assert.equal(ok.statusCode, 200, ok.body);
  const no = await app.inject({ method: "PUT", url: `/api/datasets/${own}/pipeline`, headers: { cookie: bob }, payload: { graph: graph({ kind: "source", source: SRC }) } });
  assert.equal(no.statusCode, 404, no.body);
  assert.equal(seen.length, calls + 2, "only bob's allowed run reached zsqld, and the dataset read after it");
  await done();
});

test("the app keeps each pipeline's versions, and a refused graph is not kept", async () => {
  const { app, ann, seen, done } = await setUp();
  await importAsAnn(app, ann);
  const g = (steps: unknown[]) => ({
    nodes: [
      { id: "in", kind: "source", source: SRC },
      { id: "shape", kind: "steps", input: "in", steps },
      { id: "out", kind: "output", input: "shape" },
    ],
  });
  const ok = await app.inject({ method: "PUT", url: `/api/datasets/${ID}/pipeline`, headers: { cookie: ann }, payload: { graph: g([]), against: 1 } });
  assert.equal(ok.statusCode, 200, ok.body);
  assert.equal(ok.json().pipeline, 2);
  const sent = JSON.parse(lastRun(seen).body) as { id: string; dataset: string };
  assert.match(sent.id, /^run-/, "a run id of the app's own, which a retry sends again");
  assert.equal(sent.dataset, ID);
  const stale = await app.inject({ method: "PUT", url: `/api/datasets/${ID}/pipeline`, headers: { cookie: ann }, payload: { graph: g([]), against: 1 } });
  assert.equal(stale.statusCode, 409);
  const bad = await app.inject({ method: "PUT", url: `/api/datasets/${ID}/pipeline`, headers: { cookie: ann }, payload: { graph: g([{ op: "trim_val", columns: ["gone"] }]) } });
  assert.equal(bad.statusCode, 422, bad.body);
  assert.equal(bad.json().problems[0].node, "shape");
  const now = await app.inject({ method: "GET", url: `/api/datasets/${ID}/pipeline`, headers: { cookie: ann } });
  assert.equal(now.json().version, 2, "the refused graph was not kept");
  await done();
});

test("a grid edit goes in an edits node before the output, by key, and is run", async () => {
  const { app, ann, seen, done } = await setUp();
  await importAsAnn(app, ann);
  const res = await app.inject({
    method: "POST",
    url: `/api/datasets/${ID}/edits`,
    headers: { cookie: ann },
    payload: { key: ["region"], edits: [{ kind: "set", column: "amount", key: { region: "EMEA" }, to: 120, was: 100 }] },
  });
  assert.equal(res.statusCode, 200, res.body);
  const sent = JSON.parse(lastRun(seen).body) as { graph: { nodes: { id: string; kind: string; input?: string; edits?: { id: string }[] }[] } };
  assert.deepEqual(sent.graph.nodes.map((n) => [n.id, n.kind, n.input]), [["source", "source", undefined], ["edits", "edits", "source"], ["output", "output", "edits"]]);
  assert.equal(sent.graph.nodes[1]!.edits![0]!.id, "e1");
  const undo = await app.inject({ method: "POST", url: `/api/datasets/${ID}/edits`, headers: { cookie: ann }, payload: { undo: "e1" } });
  assert.equal(undo.statusCode, 200, undo.body);
  const after = JSON.parse(lastRun(seen).body) as { graph: { nodes: { edits?: unknown[] }[] } };
  assert.deepEqual(after.graph.nodes[1]!.edits, [], "undone is left out of the graph");
  await done();
});

test("a blend is sent with what a user may choose, and never their context or connection", async () => {
  const { app, ann, seen, done } = await setUp();
  await importAsAnn(app, ann);
  const payload = {
    spec: { projections: [{ field: "region" }] },
    datasets: [{ id: ID, as: "b" }],
    blend: [],
    context: { user_id: "admin" },
    connection: { host: "10.0.0.5" },
    fresh: true,
  };
  const res = await app.inject({ method: "POST", url: "/api/blend", headers: { cookie: ann }, payload });
  assert.equal(res.statusCode, 200, res.body);
  const sent = JSON.parse(seen.at(-1)!.body) as Record<string, unknown>;
  assert.deepEqual(Object.keys(sent).sort(), ["blend", "datasets", "spec"]);
  await done();
});

test("an id that names an object's own property is no dataset", async () => {
  const { app, ann, done } = await setUp();
  for (const id of ["__proto__", "constructor", "toString"]) {
    const res = await app.inject({ method: "GET", url: `/api/datasets/${id}`, headers: { cookie: ann } });
    assert.equal(res.statusCode, 404, id);
  }
  await done();
});
