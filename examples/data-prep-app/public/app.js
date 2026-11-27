// The browser side: it talks only to this app's /api, never to zsqld.
// Kept to plain DOM so the backend, which is the reference, stays the
// thing to read.

const $ = (s) => document.querySelector(s);
const PAGE = 50;
const state = { me: null, current: null, offset: 0, sort: null, desc: false, filter: "", page: null, editing: null };

async function api(path, init = {}) {
  const res = await fetch(path, init);
  const body = res.status === 204 ? null : await res.json().catch(() => null);
  if (!res.ok) throw new Error(body?.error?.message ?? `${res.status} ${res.statusText}`);
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
        d.name, el("small", {}, `${d.rows} rows · v${d.live ?? "-"}${d.held ? " · held" : ""}${d.access === "reader" ? ` · shared by ${d.owner}` : ""}`)),
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
  $("#d-meta").textContent = `${d.rows} rows · version ${d.live} · recipe ${d.recipe} · ${d.edits} edit entr${d.edits === 1 ? "y" : "ies"}`;
  $("#notes").replaceChildren(...(d.notes ?? []).map((n) => el("li", {}, n)));
  const held = $("#held");
  held.hidden = !d.held;
  if (d.held) {
    held.replaceChildren(
      el("strong", {}, "The latest file is held. "),
      "Columns the recipe reads are gone, so version ", String(d.live), " is still the one shown: ",
      d.held.unresolved.map(describe).join("; "), ". ",
      owner() ? el("span", {}, "Fix the recipe, or ", el("button", { onclick: guarded(confirmHeld) }, "drop what reads them")) : "",
    );
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

function describe(u) {
  if (u.op === "reader") return `dataset ${u.dataset} reads its column '${u.column}'`;
  if (u.edit) return `edit ${u.edit} reads '${u.column}'`;
  if (u.step === 0) return `the ${u.op} on '${u.column}'`;
  return `step ${u.step} (${u.op}) reads '${u.column}'`;
}

async function confirmHeld() {
  const { body } = await api(`/api/datasets/${state.current.id}/confirm`, json("POST", { against: state.current.recipe }));
  show(body);
  await refreshAll();
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
  show(body);
  await refreshList();
  await refreshAll();
}));

$("#delete").addEventListener("click", guarded(async () => {
  const id = state.current.id;
  try {
    await api(`/api/datasets/${id}`, { method: "DELETE" });
  } catch (e) {
    // Other datasets read it: zsqld names them and asks to be sure.
    if (!confirm(`${e.message}\n\nDelete it anyway?`)) return;
    await api(`/api/datasets/${id}?confirm=true`, { method: "DELETE" });
  }
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
  // The other dimensions, each with this row's value, to match on.
  const others = columns.map((c, j) => ({ c, v: row[j] })).filter(({ c }) => c.role === "dimension" && c.name !== column.name);
  $("#ce-dims").replaceChildren(...others.map(({ c, v }) =>
    el("label", {}, el("input", { type: "checkbox", value: c.name, onchange: () => (scope("where").checked = true) }), ` ${c.name} = ${shownValue(v)}`)));
  // With no other dimension there is nothing to match on but the row itself.
  scope("where").closest("label").hidden = others.length === 0;
  $("#ce-dims").hidden = others.length === 0;
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
  const rowObject = Object.fromEntries(columns.map((c, i) => [c.name, row[i]]));
  const chosen = document.querySelector("#cell-edit input[name=scope]:checked").value;
  const dims = columns.filter((c) => c.role === "dimension" && c.name !== column.name).map((c) => c.name);
  let edit;
  if (chosen === "every") {
    edit = { op: "cell", column: column.name, row: rowObject, to, match: [] };
  } else if (chosen === "where") {
    const match = [...document.querySelectorAll("#ce-dims input:checked")].map((x) => x.value);
    if (!match.length) throw new Error("tick the columns the rows must also match");
    edit = { op: "cell", column: column.name, row: rowObject, to, match, all: true };
  } else {
    // This row: its dimensions, which are what name it.
    edit = { op: "cell", column: column.name, row: rowObject, to, match: dims };
  }
  const { body } = await api(`/api/datasets/${state.current.id}/edits`, json("POST", { against: state.current.recipe, edits: [edit] }));
  $("#cell-edit").hidden = true;
  show(body.dataset);
  $("#outcomes").replaceChildren(...body.outcomes.filter((o) => o.state !== "applied").map((o) =>
    el("div", { class: "banner warn" }, `edit ${o.id}: ${o.state}${o.detail ? ` (${o.detail})` : ""}`)));
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
  const { body: recipe } = await api(`/api/datasets/${state.current.id}/recipe`);
  const roles = { ...(recipe.roles ?? {}), [column.name]: column.role === "dimension" ? "measure" : "dimension" };
  await saveRecipe({ ...recipe, roles });
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
  ({ recipe: showRecipe, lineage: showLineage, blend: showBlend })[name]?.().catch(shout);
}
document.querySelectorAll(".tabs button").forEach((b) => b.addEventListener("click", () => tab(b.dataset.tab)));

// ------------------------------------------------------------------ the recipe, edits in place

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
    case "append": return `append dataset ${s.dataset}`;
    case "join": return `join dataset ${s.dataset} on ${s.on.map((k) => `${k.left} = ${k.right}`).join(", ")}`;
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
    case "set": return `${x.column}: ${shownValue(x.key[x.column] ?? x.was)} → ${shownValue(x.to)}${where(x.key, x.column)}${x.all ? " (every such row)" : ""}`;
    case "delete": return `delete the row${where(x.key)}`;
    case "add": return `add the row ${JSON.stringify({ ...x.key, ...x.values })}`;
    case "undo": return `undo ${x.undoes}`;
  }
  return JSON.stringify(x);
}

async function showRecipe() {
  const id = state.current.id;
  const [{ body: recipe }, { body: log }] = await Promise.all([api(`/api/datasets/${id}/recipe`), api(`/api/datasets/${id}/edits`)]);
  state.recipe = recipe;
  $("#recipe").value = JSON.stringify({ roles: recipe.roles ?? {}, key: recipe.key ?? [], steps: recipe.steps ?? [] }, null, 2);
  const steps = recipe.steps ?? [];
  const stateOf = Object.fromEntries(log.outcomes.map((o) => [o.id, o]));
  const undone = new Set(log.entries.flatMap((e) => e.edits).filter((x) => x.kind === "undo").map((x) => x.undoes));

  // Each entry of the edit log applies after the steps the recipe had when it was made.
  const editsAt = (p) => log.entries.filter((e) => Math.min(e.after, steps.length) === p && e.edits.some((x) => x.kind !== "undo"));
  const editBlock = (entry) => el("li", {},
    el("span", { class: "kind" }, "edits"), `made ${new Date(entry.at).toLocaleString()}`,
    el("ul", { class: "edits" }, entry.edits.filter((x) => x.kind !== "undo").map((x) => {
      const o = stateOf[x.id];
      return el("li", {},
        el("code", {}, x.id), " ", editSummary(x), " ",
        undone.has(x.id) ? el("span", { class: "state" }, "undone") : o ? el("span", { class: `state ${o.state}`, title: o.detail ?? "" }, o.state) : "",
        owner() && !undone.has(x.id) ? el("button", { class: "link", onclick: guarded(() => undo(x.id)) }, " undo") : "",
      );
    })),
  );

  const items = [el("li", { class: "source" }, el("span", { class: "kind" }, "source"),
    `the imported file, version ${state.current.sources[0]?.snapshot ?? 1} of it`)];
  items.push(...editsAt(0).map(editBlock));
  steps.forEach((s, i) => {
    items.push(el("li", {}, el("span", { class: "kind" }, `step ${i + 1}`), stepSummary(s),
      owner() ? el("button", { class: "link", title: "remove this step", onclick: guarded(() => removeStep(i)) }, " remove") : ""));
    items.push(...editsAt(i + 1).map(editBlock));
  });
  const roles = Object.entries(recipe.roles ?? {});
  if (roles.length) items.push(el("li", {}, el("span", { class: "kind" }, "roles"), roles.map(([c, r]) => `${c} is a ${r}`).join(", ")));
  if (recipe.key?.length) items.push(el("li", {}, el("span", { class: "kind" }, "key"), `rows are named by ${recipe.key.join(", ")}`));
  $("#pipeline").replaceChildren(...items);
  $("#preview-grid").replaceChildren();
}

async function saveRecipe(r) {
  const { body } = await api(`/api/datasets/${state.current.id}/recipe`, json("POST", { against: r.version ?? state.current.recipe, roles: r.roles ?? {}, steps: r.steps ?? [], key: r.key ?? [] }));
  show(body);
  await refreshAll();
}

async function removeStep(i) {
  const r = state.recipe;
  await saveRecipe({ ...r, steps: r.steps.filter((_, j) => j !== i) });
}

async function undo(id) {
  const { body } = await api(`/api/datasets/${state.current.id}/edits`, json("POST", { against: state.current.recipe, edits: [{ op: "undo", edit: id }] }));
  show(body.dataset);
  await refreshAll();
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

$("#preview").addEventListener("click", guarded(async () => {
  const steps = [...(state.recipe.steps ?? []), newStep()];
  const { body } = await api(`/api/datasets/${state.current.id}/preview`, json("POST", { steps, limit: 20 }));
  const head = el("tr", {}, body.columns.map((c) => el("th", {}, c.name, el("small", {}, `${c.type} · ${c.role}`))));
  const rows = body.rows.map((r) => el("tr", {}, r.map((v) => el("td", {}, shownValue(v)))));
  const unresolved = body.unresolved.map((u) => el("tr", {}, el("td", { class: "null" }, u)));
  $("#preview-grid").replaceChildren(el("thead", {}, head), el("tbody", {}, rows, unresolved));
}));

$("#add-step").addEventListener("click", guarded(async () => {
  const r = state.recipe;
  await saveRecipe({ ...r, steps: [...(r.steps ?? []), newStep()] });
  $("#new-step").value = "";
}));

$("#save-recipe").addEventListener("click", guarded(async () => {
  await saveRecipe({ version: state.recipe.version, ...JSON.parse($("#recipe").value) });
}));

// ------------------------------------------------------------------ lineage

async function showLineage() {
  const { body } = await api(`/api/datasets/${state.current.id}/lineage`);
  $("#lineage").textContent = JSON.stringify(body, null, 2);
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
