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
const DATASET = { id: ID, name: "budget", recipe: 1, live: 1, rows: 2, columns: [], sources: [], edits: 0 };

async function fakeZsqld() {
  const seen: Seen[] = [];
  const z = Fastify();
  z.addContentTypeParser("*", { parseAs: "string" }, (_req, body, done) => done(null, body));
  z.all("/*", async (req, reply) => {
    seen.push({ method: req.method, url: req.url, headers: req.headers, body: typeof req.body === "string" ? req.body : JSON.stringify(req.body ?? "") });
    const path = req.url.split("?")[0]!;
    if (path.endsWith("/datasets/ds_bad0000000000000")) {
      return reply.code(422).send({ error: { class: "Unprocessable", message: "column 'amount' is integer and arrived as text" } });
    }
    if (req.method === "POST" && path === "/orgs/acme/projects/shop/datasets") return reply.code(201).send(DATASET);
    if (path === "/orgs/acme/projects/shop/datasets") return { datasets: [DATASET, { ...DATASET, id: "ds_ffffffffffffffff", name: "someone else's" }] };
    if (path.endsWith("/rows")) return { version: 1, columns: [], total: 0, offset: 0, rows: [] };
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

test("an upload is streamed to zsqld as it came, with no user on it", async () => {
  const { app, ann, seen, done } = await setUp();
  const res = await importAsAnn(app, ann);
  assert.equal(res.json().access, "owner");
  const call = seen.at(-1)!;
  assert.equal(call.url, "/orgs/acme/projects/shop/datasets?name=Q1&file_name=budget.csv");
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
    { method: "POST" as const, url: `/api/datasets/${ID}/edits`, payload: { against: 1, edits: [] } },
    { method: "POST" as const, url: `/api/datasets/${ID}/recipe`, payload: { against: 1 } },
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
