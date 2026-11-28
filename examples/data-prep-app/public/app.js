// The browser side: it talks only to this app's /api, never to zsqld.
// Kept to plain DOM so the backend, which is the reference, stays the
// thing to read.

const $ = (s) => document.querySelector(s);
const PAGE = 50;
const state = { me: null, current: null, offset: 0, sort: null, desc: false, filter: "", page: null, editing: null };

async function api(path, init = {}) {
  const res = await fetch(path, init);
  const body = res.status === 204 ? null : await res.json().catch(() => null);
  if (!res.ok) {
    const e = new Error(body?.error?.message ?? `${res.status} ${res.statusText}`);
    e.problems = body?.problems;
    throw e;
  }
  return { status: res.status, body };
}

const json = (method, body) => ({ method, headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

function shout(e) {
  const box = $("#error");
  box.textContent = e instanceof Error ? e.message : String(e);
  box.hidden = false;
  clearTimeout(shout.t);
  shout.t = setTimeout(() => (box.hidden = true), 8000);
}

const guarded = (fn) => (...args) => fn(...args).catch(shout);

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of children.flat()) e.append(c instanceof Node ? c : document.createTextNode(c ?? ""));
  return e;
}

const shownValue = (v) => (v === null || v === undefined ? "∅" : typeof v === "string" ? v : JSON.stringify(v));
const owner = () => state.current?.access === "owner";

// ------------------------------------------------------------------ session

async function load() {
  const { body } = await api("/api/me");
  state.me = body;
  $("#where").textContent = `${body.org}/${body.project}`;
  $("#signin").hidden = !!body.user;
  $("#signedin").hidden = !body.user;
  $("#app").hidden = !body.user;
  if (body.user) {
    $("#who").textContent = body.user.name;
    await refreshList();
  }
}

$("#signin").addEventListener("submit", guarded(async (e) => {
  e.preventDefault();
  await api("/api/session", json("POST", { name: new FormData(e.target).get("name") }));
  await load();
}));

$("#signout").addEventListener("click", guarded(async () => {
  await api("/api/session", { method: "DELETE" });
  state.current = null;
  $("#dataset").hidden = true;
  await load();
}));

// ------------------------------------------------------------------ the list

async function refreshList() {
  const { body } = await api("/api/datasets");
  const ul = $("#datasets");
  ul.replaceChildren(
    ...body.datasets.map((d) =>
      el("li", { class: d.id === state.current?.id ? "on" : "", onclick: guarded(() => open(d.id)) },
        d.name, el("small", {}, `${d.rows} rows · v${d.live ?? "-"}${badRun(d) ? ` · ${d.last_run.state}` : ""}${d.interrupted ? " · interrupted" : ""}${d.access === "reader" ? ` · shared by ${d.owner}` : ""}`)),
    ),
  );
  if (!body.datasets.length) ul.append(el("li", { class: "muted" }, "none yet: import a CSV"));
}

$("#upload").addEventListener("submit", guarded(async (e) => {
  e.preventDefault();
  const form = new FormData(e.target);
  const name = form.get("name");
  const file = new FormData();
  file.append("file", form.get("file"));
  const { body } = await api(`/api/datasets${name ? `?name=${encodeURIComponent(name)}` : ""}`, { method: "POST", body: file });
  e.target.reset();
  await refreshList();
  await open(body.id);
}));

// ------------------------------------------------------------------ a dataset

/** A last run that did not make the live output: failed, held, or cut short. */
const badRun = (d) => d.last_run && d.last_run.state !== "succeeded" && d.last_run.state !== "running";

async function open(id) {
  const { body } = await api(`/api/datasets/${id}`);
  state.current = null;
  state.offset = 0;
  state.sort = null;
  state.filter = "";
  $("#filter").value = "";
  $("#dataset").hidden = false;
  show(body);
  await refreshList();
  await grid();
  tab(document.querySelector(".tabs .on").dataset.tab);
}

/** Shows a dataset's header. What a change answers with carries no access of its own: it was the owner's. */
function show(d) {
  const same = state.current?.id === d.id;
  d.access ??= same ? state.current.access : "owner";
  d.sharedWith ??= same ? state.current.sharedWith : [];
  d.owner ??= same ? state.current.owner : undefined;
  state.current = d;
  $("#dataset").classList.toggle("reading", d.access === "reader");
  sharing(d);
  $("#d-name").textContent = d.name;
  $("#d-meta").textContent = `${d.rows} rows · version ${d.live ?? "-"} · pipeline v${d.pipeline ?? "-"}`;
  // A job a restart found in flight: its reason, first.
  const said = d.interrupted ? [el("li", {}, el("strong", {}, "Interrupted: "), d.interrupted.reason)] : [];
  $("#notes").replaceChildren(...said, ...(d.notes ?? []).map((n) => el("li", {}, n)));
  const held = $("#held");
  held.hidden = !badRun(d);
  if (badRun(d)) {
    held.replaceChildren(el("strong", {}, `The last run ${d.last_run.state === "held" ? "was held" : d.last_run.state}. `),
      `Version ${d.live ?? "-"} is still the one shown. `, el("button", { class: "link", onclick: () => tab("pipeline") }, "See what each step did"));
  }
}

/** What a run came to, beside the pipeline, with every problem by node. */
function said(run) {
  const box = $("#run");
  if (!run) return (box.hidden = true);
  box.hidden = false;
  box.className = `banner ${run.state === "succeeded" ? "" : "warn"}`;
  const problems = (run.problems ?? []).map((p) => el("li", {}, p.node ? `${p.node}: ` : "", p.message,
    p.suggestions?.length ? ` (did you mean ${p.suggestions.map((s) => `'${s.column}'`).join(" or ")}?)` : ""));
  box.replaceChildren(el("strong", {}, run.summary), problems.length ? el("ul", {}, problems) : "");
}

/** A graph refused before it ran: its problems, as a run that failed would show them. */
async function running(work) {
  try {
    return await work();
  } catch (e) {
    if (e.problems) said({ state: "failed", summary: "Not run: zsqld refused the graph.", problems: e.problems });
    throw e;
  }
}

/** Who the dataset is shared with, for its owner; whose it is, for anyone else. */
function sharing(d) {
  const box = $("#sharing");
  if (d.access === "reader") return box.replaceChildren(`Shared with you by ${d.owner}, to read.`);
  const input = el("input", { placeholder: "share with, e.g. bob" });
  const chips = (d.sharedWith ?? []).map((u) =>
    el("span", { class: "chip" }, u, el("button", { title: "stop sharing", onclick: guarded(() => unshare(u)) }, "×")));
  box.replaceChildren("Shared with:", ...(chips.length ? chips : [el("span", {}, "nobody")]), input,
    el("button", { onclick: guarded(() => share(input.value)) }, "Share"));
}

async function share(user) {
  const { body } = await api(`/api/datasets/${state.current.id}/share`, json("POST", { user }));
  state.current.sharedWith = body.sharedWith;
  sharing(state.current);
}

async function unshare(user) {
  const { body } = await api(`/api/datasets/${state.current.id}/share/${encodeURIComponent(user)}`, { method: "DELETE" });
  state.current.sharedWith = body.sharedWith;
  sharing(state.current);
}

async function refreshAll() {
  await grid();
  const on = document.querySelector(".tabs .on").dataset.tab;
  if (on !== "grid") tab(on);
}

$("#replace").addEventListener("change", guarded(async (e) => {
  const file = new FormData();
  file.append("file", e.target.files[0]);
  e.target.value = "";
  const { body } = await api(`/api/datasets/${state.current.id}/file`, { method: "PUT", body: file });
  show(body.dataset);
  said(body.run);
  await refreshList();
  await refreshAll();
}));

$("#delete").addEventListener("click", guarded(async () => {
  // Refused when another pipeline reads it: this app knows, since it keeps them.
  await api(`/api/datasets/${state.current.id}`, { method: "DELETE" });
  state.current = null;
  $("#dataset").hidden = true;
  await refreshList();
}));

// ------------------------------------------------------------------ the grid

async function grid() {
  const d = state.current;
  const q = new URLSearchParams({ offset: state.offset, limit: PAGE });
  if (state.sort) q.set("sort", state.sort), q.set("desc", state.desc);
  if (state.filter) q.set("filter", state.filter);
  const { body: page } = await api(`/api/datasets/${d.id}/rows?${q}`);
  state.page = page;
  $("#pageinfo").textContent = ` ${page.total ? state.offset + 1 : 0}–${Math.min(state.offset + PAGE, page.total)} of ${page.total} `;
  const head = el("tr", {}, page.columns.map((c) =>
    el("th", { onclick: guarded(() => sortBy(c.name)) },
      c.name, state.sort === c.name ? (state.desc ? " ↓" : " ↑") : "",
      el("small", {}, c.type, " · ", el("span", { class: "role", title: "switch the role", onclick: guarded((e) => toggleRole(e, c)) }, c.role)),
    ),
  ));
  const numeric = (c) => ["integer", "double"].includes(c.type) || c.type.startsWith("decimal");
  const rows = page.rows.map((r) => el("tr", {}, r.map((v, i) =>
    el("td", { class: `${v === null ? "null" : ""} ${numeric(page.columns[i]) ? "num" : ""}`, onclick: () => startEdit(r, i) }, shownValue(v)))));
  $("#grid").replaceChildren(el("thead", {}, head), el("tbody", {}, rows));
}

/** The columns an edit names a row by: every dimension, as the edits node's key. */
const keyColumns = (columns) => columns.filter((c) => c.role === "dimension").map((c) => c.name);

/** Opens the edit panel on a cell: its new value, and which rows the edit is for. */
function startEdit(row, i) {
  if (!owner()) return;
  const columns = state.page.columns;
  const column = columns[i];
  const dimension = column.role === "dimension";
  state.editing = { row, column, columns };
  $("#ce-column").textContent = `${column.name} (${column.role})`;
  $("#ce-was").textContent = `was ${shownValue(row[i])}`;
  $("#ce-to").value = row[i] ?? "";
  $("#ce-every").textContent = `every row where ${column.name} is ${shownValue(row[i])}`;
  $("#ce-every-label").hidden = !dimension;
  $("#ce-key").textContent = keyColumns(columns).join(", ") || "(no dimension: make one a dimension first)";
  scope(dimension ? "every" : "row").checked = true;
  $("#cell-edit").hidden = false;
  $("#ce-to").focus();
}

const scope = (v) => document.querySelector(`#cell-edit input[name=scope][value=${v}]`);

$("#ce-cancel").addEventListener("click", () => ($("#cell-edit").hidden = true));

$("#cell-edit").addEventListener("submit", guarded(async (e) => {
  e.preventDefault();
  const { row, column, columns } = state.editing;
  const text = $("#ce-to").value;
  const to = text === "" ? null : text;
  const was = row[columns.indexOf(column)];
  const chosen = document.querySelector("#cell-edit input[name=scope]:checked").value;
  const key = keyColumns(columns);
  let edit;
  if (chosen === "every") {
    edit = { kind: "remap", column: column.name, from: was, to };
  } else {
    if (!key.length) throw new Error("no column is a dimension, so no row can be named; make one a dimension first");
    edit = { kind: "set", column: column.name, key: Object.fromEntries(key.map((k) => [k, row[columns.findIndex((c) => c.name === k)]])), to, was };
  }
  const { body } = await running(() => api(`/api/datasets/${state.current.id}/edits`, json("POST", { key, edits: [edit] })));
  $("#cell-edit").hidden = true;
  show(body.dataset);
  const fix = body.run.nodes?.find((n) => n.id === "edits");
  $("#outcomes").replaceChildren(...(fix?.edits ?? []).filter((o) => o.state !== "applied").map((o) =>
    el("div", { class: "banner warn" }, `edit ${o.id}: ${o.state}${o.detail ? ` (${o.detail})` : ""}`)));
  if (body.run.state !== "succeeded") said(body.run), tab("pipeline");
  await grid();
}));

async function sortBy(name) {
  state.desc = state.sort === name ? !state.desc : false;
  state.sort = name;
  await grid();
}

async function toggleRole(e, column) {
  e.stopPropagation();
  if (!owner()) return;
  const { body: p } = await api(`/api/datasets/${state.current.id}/pipeline`);
  const graph = structuredClone(p.graph);
  const out = graph.nodes.find((n) => n.kind === "output");
  out.roles = { ...(out.roles ?? {}), [column.name]: column.role === "dimension" ? "measure" : "dimension" };
  await saveGraph(graph, p.version);
}

$("#filter").addEventListener("keydown", guarded(async (e) => {
  if (e.key !== "Enter") return;
  state.filter = e.target.value.trim();
  state.offset = 0;
  await grid();
}));
$("#prev").addEventListener("click", guarded(async () => { state.offset = Math.max(0, state.offset - PAGE); await grid(); }));
$("#next").addEventListener("click", guarded(async () => { state.offset += PAGE; await grid(); }));

// ------------------------------------------------------------------ the tabs

function tab(name) {
  document.querySelectorAll(".tabs button").forEach((b) => b.classList.toggle("on", b.dataset.tab === name));
  document.querySelectorAll("[data-pane]").forEach((p) => (p.hidden = p.dataset.pane !== name));
  ({ pipeline: showPipeline, lineage: showLineage, blend: showBlend })[name]?.().catch(shout);
}
document.querySelectorAll(".tabs button").forEach((b) => b.addEventListener("click", () => tab(b.dataset.tab)));

// ------------------------------------------------------------------ the pipeline

function stepSummary(s) {
  const cols = (c) => c.join(", ");
  switch (s.op) {
    case "rename_column": return `rename ${s.column} to ${s.to}`;
    case "retype_column": return `make ${s.column} ${s.type}${s.format ? ` (${s.format})` : ""}`;
    case "remove_columns": return `remove ${cols(s.columns)}`;
    case "reorder_columns": return `put ${cols(s.columns)} first`;
    case "add_column": return `add ${s.column} = ${s.formula}`;
    case "update_column": return `set ${s.column} = ${s.formula}`;
    case "filter": return `keep rows where ${s.formula}`;
    case "sort": return `sort by ${s.by.map((k) => `${k.column}${k.desc ? " ↓" : ""}`).join(", ")}`;
    case "dedupe": return `remove duplicates${s.columns?.length ? ` on ${cols(s.columns)}` : ""}`;
    case "fill_empty": return `fill empty ${s.column} with ${shownValue(s.value)}`;
    case "fill_down": return `fill empty ${cols(s.columns)} from the row above`;
    case "unpivot": return `unpivot ${s.keep?.length ? `every column but ${cols(s.keep)}` : cols(s.columns)} into ${s.name} and ${s.value}${s.keep_empty ? ", keeping empty values" : ""}`;
    case "find_and_replace_val": return `in ${s.column}, replace "${s.find}" with "${s.with}"${s.whole ? " (whole value)" : ""}`;
    case "remap_val": return `in ${s.column}, ${s.values.map((m) => `${shownValue(m.from)} → ${shownValue(m.to)}`).join(", ")}`;
    case "trim_val": return `trim ${cols(s.columns)}`;
    case "upper_case": case "lower_case": case "title_case": case "snake_case": case "camel_case":
      return `${s.op.replace("_", " ")} ${cols(s.columns)}`;
    case "group": return `group by ${cols(s.by)}: ${s.aggregates.map((a) => `${a.as} = ${a.fn}(${a.column ?? "*"})`).join(", ")}`;
    case "sql": return `sql: ${s.query.length > 90 ? `${s.query.slice(0, 90)}…` : s.query}`;
    default: return JSON.stringify(s);
  }
}

function editSummary(x) {
  const where = (key, column) => {
    const others = Object.entries(key).filter(([k]) => k !== column);
    return others.length ? ` where ${others.map(([k, v]) => `${k} = ${shownValue(v)}`).join(", ")}` : "";
  };
  switch (x.kind) {
    case "remap": return `${x.column}: every ${shownValue(x.from)} → ${shownValue(x.to)}`;
    case "set": return `${x.column}: ${shownValue(x.was ?? x.key[x.column])} → ${shownValue(x.to)}${where(x.key, x.column)}${x.all ? " (every such row)" : ""}`;
    case "delete": return `delete the row${where(x.key)}`;
    case "add": return `add the row ${JSON.stringify({ ...x.key, ...x.values })}`;
  }
  return JSON.stringify(x);
}

/** A node's inputs, in the order it reads them. */
const inputsOf = (n) => n.kind === "join" ? [n.left, n.right] : n.kind === "append" ? n.inputs : n.kind === "sql" ? Object.values(n.inputs) : n.input ? [n.input] : [];

/** The nodes in an order where each comes after what it reads. */
function ordered(graph) {
  const done = new Set();
  const out = [];
  let left = [...graph.nodes];
  while (left.length) {
    const ready = left.filter((n) => inputsOf(n).every((i) => done.has(i)));
    if (!ready.length) return [...out, ...left];
    for (const n of ready) done.add(n.id), out.push(n);
    left = left.filter((n) => !done.has(n.id));
  }
  return out;
}

function nodeTitle(n) {
  switch (n.kind) {
    case "source": return `source ${state.names[n.source] ?? n.source}${n.read && n.read !== "latest" ? ` (${JSON.stringify(n.read)})` : ""}`;
    case "dataset": return `dataset ${state.names[n.dataset] ?? n.dataset}`;
    case "join": return `${n.type ?? "left"} join of ${n.left} and ${n.right} on ${n.on.map((k) => `${k.left} = ${k.right}`).join(", ")}`;
    case "append": return `append ${n.inputs.join(", ")}`;
    case "sql": return `sql over ${Object.entries(n.inputs).map(([a, i]) => `${i} as ${a}`).join(", ")}`;
    case "steps": return `${n.steps.length} step${n.steps.length === 1 ? "" : "s"} over ${n.input}`;
    case "edits": return `over ${n.input}, rows named by ${(n.key ?? []).join(", ") || "nothing"}`;
    case "output": return `the output${n.key?.length ? `, one row per ${n.key.join(", ")}` : ""}`;
    default: return n.kind;
  }
}

async function showPipeline() {
  const id = state.current.id;
  const { body: p } = await api(`/api/datasets/${id}/pipeline`);
  state.pipeline = p;
  state.names ??= {};
  for (const s of (await api("/api/sources")).body.sources) state.names[s.id] = s.name;
  for (const d of (await api("/api/datasets")).body.datasets) state.names[d.id] = d.name;
  $("#graph").value = JSON.stringify(p.graph, null, 2);
  said(p.lastRun);
  const records = Object.fromEntries((p.lastRun?.nodes ?? []).map((r) => [r.id, r]));
  const items = ordered(p.graph).map((n) => {
    const r = records[n.id];
    const children = [];
    if (n.kind === "steps") {
      children.push(el("ol", { class: "edits" }, n.steps.map((s, i) => el("li", {},
        stepSummary(s), r?.steps?.find((x) => x.step === i + 1) ? el("span", { class: "muted" }, ` · ${r.steps.find((x) => x.step === i + 1).said}`) : "",
        owner() ? el("button", { class: "link", title: "remove this step", onclick: guarded(() => removeStep(n.id, i)) }, " remove") : ""))));
    }
    if (n.kind === "edits") {
      const stateOf = Object.fromEntries((r?.edits ?? []).map((o) => [o.id, o]));
      children.push(el("ul", { class: "edits" }, n.edits.map((x) => {
        const o = stateOf[x.id];
        return el("li", {}, el("code", {}, x.id), " ", editSummary(x), " ",
          o ? el("span", { class: `state ${o.state}`, title: o.detail ?? "" }, o.state) : "",
          owner() ? el("button", { class: "link", onclick: guarded(() => undo(x.id)) }, " undo") : "");
      })));
    }
    return el("li", { class: n.kind === "source" || n.kind === "dataset" ? "source" : "" },
      el("span", { class: "kind" }, n.kind), el("code", {}, n.id), " ", nodeTitle(n),
      n.note ? el("div", { class: "muted" }, n.note) : "",
      r ? el("div", {}, r.summary) : "", ...children);
  });
  $("#pipeline").replaceChildren(...items);
  $("#preview-grid").replaceChildren();
  const options = Object.entries(state.names).filter(([k]) => k !== id && !p.graph.nodes.some((n) => n.source === k || n.dataset === k));
  $("#join-with").replaceChildren(...options.map(([k, name]) => el("option", { value: k }, `${k.startsWith("src_") ? "source" : "dataset"} ${name}`)));
}

/** Saves a graph as the pipeline's next version, and runs it. */
async function saveGraph(graph, against = state.pipeline?.version) {
  const { body } = await running(() => api(`/api/datasets/${state.current.id}/pipeline`, json("PUT", { graph, against })));
  show(body.dataset);
  await refreshAll();
  said(body.run);
}

/** The graph with a step added to its steps node: made the first time, just before the edits and the output. */
function withStep(graph, step) {
  const g = structuredClone(graph);
  let steps = g.nodes.find((n) => n.kind === "steps");
  if (!steps) {
    const out = g.nodes.find((n) => n.kind === "output");
    const before = g.nodes.find((n) => n.kind === "edits" && n.id === out.input) ?? out;
    steps = { id: "steps", kind: "steps", input: before.input, steps: [] };
    before.input = "steps";
    g.nodes.splice(g.nodes.indexOf(before), 0, steps);
  }
  steps.steps.push(step);
  return { graph: g, node: steps.id };
}

async function removeStep(node, i) {
  const graph = structuredClone(state.pipeline.graph);
  const n = graph.nodes.find((x) => x.id === node);
  n.steps = n.steps.filter((_, j) => j !== i);
  await saveGraph(graph);
}

async function undo(id) {
  const { body } = await running(() => api(`/api/datasets/${state.current.id}/edits`, json("POST", { undo: id })));
  show(body.dataset);
  await refreshAll();
  said(body.run);
}

function newStep() {
  return JSON.parse($("#new-step").value);
}

const first = (pred) => state.current.columns.find(pred)?.name ?? "column";
const template = (s) => () => ($("#new-step").value = JSON.stringify(s(), null, 2));
$("#add-remap").addEventListener("click", template(() => ({ op: "remap_val", column: first((c) => c.type === "text"), values: [{ from: "old", to: "new" }] })));
// Every column but the text ones kept, as a spreadsheet's months are:
// a month a later file adds is unpivoted too.
$("#add-unpivot").addEventListener("click", template(() => ({
  op: "unpivot",
  keep: state.current.columns.filter((c) => c.type === "text").map((c) => c.name),
  name: "month",
  value: "amount",
})));
$("#add-sql").addEventListener("click", template(() => {
  const dim = first((c) => c.role === "dimension");
  const m = first((c) => c.role === "measure");
  return { op: "sql", query: `SELECT *, sum(${JSON.stringify(m)}) OVER (PARTITION BY ${JSON.stringify(dim)}) AS ${JSON.stringify(`${m} total`)} FROM input` };
}));
$("#add-fill-down").addEventListener("click", template(() => ({ op: "fill_down", columns: [first((c) => c.role === "dimension")] })));
$("#add-filter").addEventListener("click", template(() => ({ op: "filter", formula: `${first((c) => c.role === "measure")} > 0` })));
$("#add-formula").addEventListener("click", template(() => ({ op: "add_column", column: "label", formula: `upper(${first((c) => c.type === "text")})` })));
$("#add-update").addEventListener("click", template(() => ({ op: "update_column", column: first((c) => c.type === "text"), formula: `trim(${first((c) => c.type === "text")})` })));
$("#add-case").addEventListener("click", template(() => ({ op: "lower_case", columns: [first((c) => c.type === "text")] })));
$("#add-group").addEventListener("click", template(() => ({
  op: "group",
  by: [first((c) => c.role === "dimension")],
  aggregates: [{ column: first((c) => c.role === "measure"), fn: "sum", as: "total" }],
})));
$("#add-sort").addEventListener("click", template(() => ({ op: "sort", by: [{ column: first((c) => c.role === "measure"), desc: true }] })));

// The graph with the new step, run up to its node over a sample, kept nowhere.
$("#preview").addEventListener("click", guarded(async () => {
  const { graph, node } = withStep(state.pipeline.graph, newStep());
  const { body } = await api(`/api/datasets/${state.current.id}/preview`, json("POST", { graph, node, limit: 20 }));
  const head = el("tr", {}, body.columns.map((c) => el("th", {}, c.name, el("small", {}, `${c.type} · ${c.role}`))));
  const rows = body.rows.map((r) => el("tr", {}, r.map((v) => el("td", {}, shownValue(v)))));
  const problems = body.problems.map((p) => el("tr", {}, el("td", { class: "null" }, `${p.node}: ${p.message}`)));
  const did = body.nodes.find((n) => n.id === node);
  $("#preview-grid").replaceChildren(el("caption", {}, did?.summary ?? ""), el("thead", {}, head), el("tbody", {}, rows, problems));
}));

$("#add-step").addEventListener("click", guarded(async () => {
  await saveGraph(withStep(state.pipeline.graph, newStep()).graph);
  $("#new-step").value = "";
}));

$("#save-graph").addEventListener("click", guarded(async () => {
  await saveGraph(JSON.parse($("#graph").value));
}));

// A dry run: what a run of the graph as written would read and do.
$("#check").addEventListener("click", guarded(async () => {
  const { body } = await api(`/api/datasets/${state.current.id}/check`, json("POST", { graph: JSON.parse($("#graph").value) }));
  $("#checked").textContent = body.said;
}));

// Another input, joined as a look-up on the columns both have by name.
$("#join").addEventListener("click", guarded(async () => {
  const other = $("#join-with").value;
  if (!other) throw new Error("there is nothing else of yours to join");
  const graph = structuredClone(state.pipeline.graph);
  const out = graph.nodes.find((n) => n.kind === "output");
  const theirs = other.startsWith("src_") ? (await api("/api/sources")).body.sources.find((s) => s.id === other)?.columns : (await api(`/api/datasets/${other}`)).body.columns;
  const common = (theirs ?? []).filter((c) => state.current.columns.some((x) => x.name === c.name)).map((c) => c.name);
  if (!common.length) throw new Error("they have no column of the same name to join on; join them in the graph below, naming the keys");
  const k = graph.nodes.filter((n) => n.kind === "join").length + 1;
  const input = { id: `with${k}`, ...(other.startsWith("src_") ? { kind: "source", source: other } : { kind: "dataset", dataset: other }) };
  const join = { id: `join${k}`, kind: "join", left: out.input, right: input.id, on: common.map((c) => ({ left: c, right: c })), type: "left" };
  out.input = join.id;
  graph.nodes.splice(graph.nodes.indexOf(out), 0, input, join);
  await saveGraph(graph);
}));

// ------------------------------------------------------------------ lineage

async function showLineage() {
  const { body } = await api(`/api/datasets/${state.current.id}/lineage`);
  $("#lineage").textContent = JSON.stringify(body.lineage, null, 2);
}

// ------------------------------------------------------------------ blend

let modelFields;

// A request that runs as it is: each dimension of the dataset that the
// model has by the same name is a join key, and is projected, as every
// model key a join names must be; a measure of the model's main fact
// beside the dataset's measures. A dataset with no such dimension is
// set beside every row: a join on no keys, a cross join.
async function showBlend() {
  if ($("#blend").dataset.for === state.current.id) return;
  modelFields ??= (await api("/api/fields")).body.fields;
  const dims = modelFields.filter((f) => f.kind === "dimension");
  const measures = modelFields.filter((f) => f.kind === "measure");
  const perTable = {};
  for (const m of measures) for (const t of m.tables) perTable[t] = (perTable[t] ?? 0) + 1;
  const fact = Object.entries(perTable).sort((a, b) => b[1] - a[1])[0]?.[0];
  const measure = measures.find((m) => m.tables.includes(fact)) ?? measures[0];
  const same = (a, b) => a.toLowerCase().replace(/[\s_-]+/g, " ") === b.toLowerCase().replace(/[\s_-]+/g, " ");
  const on = [];
  for (const c of state.current.columns.filter((c) => c.role === "dimension")) {
    const f = dims.find((d) => same(d.name, c.name) || same(d.uid, c.name));
    if (f) on.push({ left: f.name, right: `ds.${c.name}` });
  }
  const dsMeasures = state.current.columns.filter((c) => c.role === "measure").map((c) => ({ field: `ds.${c.name}` }));
  const dsDims = on.length ? [] : state.current.columns.filter((c) => c.role === "dimension").map((c) => ({ field: `ds.${c.name}` }));
  const keys = on.length ? on.map((o) => ({ field: o.left })) : [{ field: dims[0]?.name ?? "<a model dimension>" }];
  $("#blend").value = JSON.stringify({
    spec: { projections: [...keys, ...(measure ? [{ field: measure.name }] : []), ...dsDims, ...dsMeasures] },
    datasets: [{ id: state.current.id, as: "ds" }],
    blend: [{ join: "ds", to: "model", on, kind: "enrich" }],
  }, null, 2);
  $("#blend-hint").textContent = on.length
    ? `Joined on ${on.map((o) => o.left).join(", ")}, the model's dimension${on.length > 1 ? "s" : ""} named as this dataset's. A model key a join names must be projected too.`
    : "The model has no dimension named as this dataset's, so this joins on no keys: each of its rows beside every row of the model (a cross join). Name keys in \"on\" to join on them, and project them.";
  $("#blend").dataset.for = state.current.id;
}

$("#run-blend").addEventListener("click", guarded(async () => {
  const { status, body } = await api("/api/blend", json("POST", JSON.parse($("#blend").value)));
  if (status === 202) return shout(new Error(body.message));
  const head = el("tr", {}, body.columns.map((c) => el("th", {}, c.name, el("small", {}, c.type))));
  const rows = body.rows.map((r) => el("tr", {}, r.map((v) => el("td", {}, shownValue(v)))));
  $("#blend-grid").replaceChildren(el("thead", {}, head), el("tbody", {}, rows));
}));

load().catch(shout);
