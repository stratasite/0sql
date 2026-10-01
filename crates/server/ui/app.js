// 0sql app. One shell, views rendered from the JSON API. The path says
// which view; the session cookie says who.
(function () {
  const root = document.getElementById("app");
  const state = { me: null };

  // ------------------------------------------------------------- helpers
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const isRaw = (v) => v !== null && typeof v === "object" && "__raw" in v;
  const h = (strings, ...vals) => strings.reduce((out, str, i) => out + str + (i < vals.length ? (isRaw(vals[i]) ? vals[i].__raw : esc(vals[i])) : ""), "");
  const raw = (s) => ({ __raw: s });
  const pill = (text) => raw(h`<span class="pill ${text}">${text}</span>`);
  const when = (iso) => iso ? iso.replace("T", " ").replace("Z", "") : "";

  async function api(method, path, body) {
    const r = await fetch(path, {
      method,
      headers: { "x-requested-with": "zsql", ...(body !== undefined ? { "content-type": "application/json" } : {}) },
      body: body !== undefined ? JSON.stringify(body) : undefined,
      credentials: "same-origin",
    });
    let data = null;
    try { data = await r.json(); } catch (_) { data = {}; }
    if (!r.ok) {
      const err = new Error((data && data.error && data.error.message) || r.statusText);
      err.status = r.status;
      throw err;
    }
    return data;
  }

  function toast(msg) {
    let t = document.querySelector(".toast");
    if (!t) { t = document.createElement("div"); t.className = "toast"; document.body.appendChild(t); }
    t.textContent = msg; t.classList.add("show");
    clearTimeout(t._timer); t._timer = setTimeout(() => t.classList.remove("show"), 1800);
  }

  function copy(text) { navigator.clipboard && navigator.clipboard.writeText(text).then(() => toast("copied")); }

  function go(path) { history.pushState(null, "", path); route(); }
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-link]");
    if (a) { e.preventDefault(); go(a.getAttribute("href")); }
    const c = e.target.closest("[data-copy]");
    if (c) { copy(c.getAttribute("data-copy")); }
  });
  window.addEventListener("popstate", route);

  function theme(next) {
    const cur = document.documentElement.getAttribute("data-theme") || "auto";
    const order = ["auto", "light", "dark"];
    const t = next || order[(order.indexOf(cur) + 1) % order.length];
    if (t === "auto") document.documentElement.removeAttribute("data-theme"); else document.documentElement.setAttribute("data-theme", t);
    try { localStorage.setItem("zsql-theme", t); } catch (_) {}
  }
  try { const t = localStorage.getItem("zsql-theme"); if (t && t !== "auto") document.documentElement.setAttribute("data-theme", t); } catch (_) {}

  const secretBlock = (secret, note) => h`
    <div class="secret">
      <div class="label">Shown once. Copy it now.</div>
      <code>${secret}</code>
      <div class="actions" style="margin-top:8px"><button class="btn sm" data-copy="${secret}">Copy</button></div>
      <div class="note">${note}</div>
    </div>`;

  const alertBox = (msg, kind) => msg ? h`<div class="alert ${kind || "error"}">${msg}</div>` : "";

  // ---------------------------------------------------------------- frame
  function frame(active, body) {
    const me = state.me;
    const isAdmin = me && me.role === "admin";
    const tabs = [["/app/", "Projects"], ["/app/query-keys", "Query keys"], ["/app/keys", "My keys"], ["/app/members", "Members"]];
    if (isAdmin) tabs.push(["/app/audit", "Audit"]);
    root.innerHTML = h`
      <div class="topbar">
        <a class="brand" href="/app/" data-link><span class="mark"></span>0sql</a>
        <nav class="nav">${raw(tabs.map(([p, t]) => h`<a href="${p}" data-link class="${p === active ? "active" : ""}">${t}</a>`).join(""))}</nav>
        <div class="spacer"></div>
        <div class="who"><span><b>${me.account.name}</b> · ${me.user.email} ${pill(me.role)}</span>
          <button class="theme" id="theme">theme</button>
          <button class="btn sm quiet" id="logout">Sign out</button></div>
      </div>
      <div class="main">${raw(body)}</div>`;
    document.getElementById("logout").onclick = async () => { await api("POST", "/logout"); state.me = null; go("/app/login"); };
    document.getElementById("theme").onclick = () => theme();
  }

  function authFrame(body) {
    root.innerHTML = h`<div class="auth"><div><div class="brand"><span class="mark"></span>0sql</div><div class="card"><div class="card-body">${raw(body)}</div></div></div></div>`;
  }

  function form(el, onSubmit) {
    const f = el.querySelector("form");
    f.onsubmit = async (e) => {
      e.preventDefault();
      const data = Object.fromEntries(new FormData(f).entries());
      const btn = f.querySelector("button[type=submit]");
      if (btn) btn.disabled = true;
      try { await onSubmit(data, f); } catch (err) { showError(el, err.message); }
      if (btn) btn.disabled = false;
    };
  }
  function showError(el, msg) {
    let a = el.querySelector(".alert.error");
    if (!a) { a = document.createElement("div"); a.className = "alert error"; el.querySelector("form").prepend(a); }
    a.textContent = msg;
  }

  // ----------------------------------------------------------------- auth
  function viewLogin(msg) {
    authFrame(h`
      ${raw(alertBox(msg, "ok"))}
      <form>
        <div class="field"><label>Email</label><input name="email" type="email" required autofocus></div>
        <div class="field"><label>Password</label><input name="password" type="password" required></div>
        <button class="btn primary" type="submit" style="width:100%;justify-content:center">Sign in</button>
      </form>
      <div class="switch">New here? <a href="/app/signup" data-link>Create an account</a></div>`);
    form(root, async (d) => { await api("POST", "/login", d); await loadMe(); go("/app/"); });
  }

  function viewSignup() {
    authFrame(h`
      <h1 style="margin-bottom:4px">Create your account</h1>
      <p class="muted" style="margin-bottom:16px">An account holds your projects, your team and the keys your apps use.</p>
      <form>
        <div class="field"><label>Account name</label><input name="account" required placeholder="Acme" autofocus></div>
        <div class="field"><label>Your name</label><input name="name" placeholder="Ann"></div>
        <div class="field"><label>Email</label><input name="email" type="email" required></div>
        <div class="field"><label>Password</label><input name="password" type="password" required minlength="8"></div>
        <button class="btn primary" type="submit" style="width:100%;justify-content:center">Create account</button>
      </form>
      <div class="switch">Have one? <a href="/app/login" data-link>Sign in</a></div>`);
    form(root, async (d) => {
      const r = await api("POST", "/signup", d);
      await loadMe();
      authFrame(h`
        <h1>Welcome, ${r.user.name || r.user.email}</h1>
        <p class="muted">Your account <b>${r.account.name}</b> is ready. This is your personal key for the CLI:</p>
        ${raw(secretBlock(r.secret, "Run `zsql auth --api-key <key>` inside a project to store it. You can make more keys under My keys."))}
        <a class="btn primary" href="/app/" data-link>Go to projects</a>`);
    });
  }

  function viewInvite() {
    const token = new URLSearchParams(location.search).get("token") || "";
    authFrame(h`
      <h1 style="margin-bottom:4px">Join the account</h1>
      <p class="muted" style="margin-bottom:16px">Set your name and a password. You get a personal key for the CLI as well.</p>
      <form>
        <div class="field"><label>Invite token</label><input name="token" class="mono" required value="${token}" placeholder="zin_..."></div>
        <div class="field"><label>Your name</label><input name="name" autofocus></div>
        <div class="field"><label>Password</label><input name="password" type="password" required minlength="8"></div>
        <button class="btn primary" type="submit" style="width:100%;justify-content:center">Join</button>
      </form>`);
    form(root, async (d) => {
      const r = await api("POST", "/invites/accept", d);
      await loadMe();
      authFrame(h`
        <h1>You are in</h1>
        <p class="muted">You joined <b>${r.account.name}</b> as ${r.user.email}. Your personal key:</p>
        ${raw(secretBlock(r.secret, "Run `zsql auth --api-key <key>` inside a project to store it."))}
        <a class="btn primary" href="/app/" data-link>Go to projects</a>`);
    });
  }

  // ------------------------------------------------------------- projects
  async function viewProjects() {
    const d = await api("GET", "/projects");
    const byProject = {};
    for (const dep of d.deployments) (byProject[dep.project] = byProject[dep.project] || []).push(dep);
    const rows = (d.projects || []).map((p) => {
      const deps = byProject[p.uid] || [];
      return h`<a class="row" href="/app/projects/${p.uid}" data-link style="color:inherit;text-decoration:none">
        <div class="grow"><div class="title">${p.name || p.uid} <span class="faint mono">${p.uid}</span></div>
          <div class="meta">production <code>${p.production_branch}</code>${p.protected_production ? " · protected" : ""}${p.visibility === "restricted" ? " · restricted" : ""} · ${deps.length} deployed branch${deps.length === 1 ? "" : "es"}</div></div>
        ${pill(p.level)}</a>`;
    }).join("");
    frame("/app/", h`
      <div class="page-head"><div><h1>Projects</h1><div class="sub">Deploy from a project directory with <code>zsql deploy</code>; the first deploy creates the project.</div></div></div>
      <div class="card"><div class="card-head"><h2>${state.me.account.name}</h2><span class="muted small">${(d.projects || []).length} projects</span></div>
        <div class="card-body flush rows">${raw(rows || h`<div class="empty">No projects yet. Run <code>zsql init</code>, then <code>zsql auth --api-key …</code> and <code>zsql deploy</code>.</div>`)}</div></div>`);
  }

  async function viewProject(uid) {
    let access, list;
    try { [access, list] = await Promise.all([api("GET", `/projects/${encodeURIComponent(uid)}/access`), api("GET", "/projects")]); }
    catch (e) { return frame("/app/", h`<div class="alert error">${e.message}</div>`); }
    const p = access.project;
    const mine = (list.projects || []).find((x) => x.uid === uid) || {};
    const owner = mine.level === "owner";
    const deps = list.deployments.filter((x) => x.project === uid);
    const depRows = deps.map((x) => h`<tr><td class="mono">${x.branch}</td><td>${x.tables} tables · ${x.fields} fields · ${x.joins} joins · ${x.policies} policies</td><td class="muted small">${x.deployed_at}</td></tr>`).join("");
    const accessRows = access.access.map((a) => h`<tr><td>${a.user.email}${a.user.name ? raw(` <span class="faint">${esc(a.user.name)}</span>`) : ""}</td><td>${pill(a.level)}</td>
      <td class="actions">${owner ? raw(h`<select class="sm" data-level="${a.user.email}" style="width:auto"><option ${a.level === "read" ? "selected" : ""}>read</option><option ${a.level === "write" ? "selected" : ""}>write</option><option ${a.level === "owner" ? "selected" : ""}>owner</option></select><button class="btn sm quiet danger" data-remove-access="${a.user.email}">Remove</button>`) : ""}</td></tr>`).join("");
    const keyRows = access.query_keys.map((k) => h`<tr><td>${k.key.name} <span class="faint mono">${k.key.prefix}…</span></td><td class="mono">${k.grant.branch === null ? p.production_branch + " (production)" : k.grant.branch === "*" ? "every branch" : k.grant.branch}</td>
      <td class="actions">${owner ? raw(h`<button class="btn sm quiet danger" data-revoke-grant="${k.key.id}">Revoke</button>`) : ""}</td></tr>`).join("");
    frame("/app/", h`
      <div class="page-head"><div><div class="small"><a href="/app/" data-link>Projects</a> / ${p.uid}</div><h1>${p.name || p.uid}</h1></div>${pill(mine.level || "read")}</div>
      <div class="grid two">
        <div class="card"><div class="card-head"><h2>Deployed branches</h2><span class="muted small">zsql deploy</span></div>
          <div class="card-body flush">${raw(deps.length ? h`<table><thead><tr><th>branch</th><th>model</th><th>deployed</th></tr></thead><tbody>${raw(depRows)}</tbody></table>` : h`<div class="empty">Nothing deployed yet.</div>`)}</div></div>
        <div class="card"><div class="card-head"><h2>Settings</h2>${owner ? "" : raw('<span class="muted small">owners edit</span>')}</div>
          <div class="card-body"><form id="settings">
            <div class="field"><label>Name</label><input name="name" value="${p.name}" ${owner ? "" : "disabled"}></div>
            <div class="field"><label>Production branch</label><input name="production_branch" class="mono" value="${p.production_branch}" ${owner ? "" : "disabled"}></div>
            <div class="field"><label>Visibility</label><select name="visibility" ${owner ? "" : "disabled"}><option value="account" ${p.visibility === "account" ? "selected" : ""}>account: every developer reads it</option><option value="restricted" ${p.visibility === "restricted" ? "selected" : ""}>restricted: granted users only</option></select></div>
            <div class="field"><label class="switch"><input type="checkbox" name="protected_production" ${p.protected_production ? "checked" : ""} ${owner ? "" : "disabled"}> Deploys to the production branch need an owner</label></div>
            ${owner ? raw('<div class="actions"><button class="btn primary" type="submit">Save</button><button class="btn danger quiet" type="button" id="delete">Delete project</button></div>') : ""}
          </form></div></div>
        <div class="card"><div class="card-head"><h2>People</h2><span class="muted small">developers read by default</span></div>
          <div class="card-body flush"><table><thead><tr><th>user</th><th>level</th><th></th></tr></thead><tbody>${raw(accessRows)}</tbody></table>
          ${owner ? raw(h`<form id="grant-user" class="inline" style="padding:12px 16px;border-top:1px solid var(--line)"><div class="field"><label>Member email</label><input name="email" type="email" required placeholder="bob@acme.com"></div><div class="field narrow"><label>Level</label><select name="level"><option>read</option><option>write</option><option>owner</option></select></div><button class="btn" type="submit">Grant</button></form>`) : ""}</div></div>
        <div class="card"><div class="card-head"><h2>Query keys on this project</h2><a href="/app/query-keys" data-link class="small">manage keys</a></div>
          <div class="card-body flush">${raw(access.query_keys.length ? h`<table><thead><tr><th>key</th><th>branch</th><th></th></tr></thead><tbody>${raw(keyRows)}</tbody></table>` : h`<div class="empty">No query key reads this project.</div>`)}</div></div>
      </div>`);
    if (owner) {
      const settings = document.getElementById("settings");
      settings.onsubmit = async (e) => {
        e.preventDefault();
        const d = Object.fromEntries(new FormData(settings).entries());
        d.protected_production = settings.protected_production.checked;
        try { await api("PATCH", `/projects/${encodeURIComponent(uid)}`, d); toast("saved"); viewProject(uid); } catch (err) { showError(settings.parentElement, err.message); }
      };
      document.getElementById("delete").onclick = async () => {
        if (!confirm(`Delete ${uid} and every deployment of it?`)) return;
        try { await api("DELETE", `/projects/${encodeURIComponent(uid)}`); go("/app/"); } catch (err) { toast(err.message); }
      };
      const gu = document.getElementById("grant-user");
      gu.onsubmit = async (e) => { e.preventDefault(); const d = Object.fromEntries(new FormData(gu).entries()); try { await api("PUT", `/projects/${encodeURIComponent(uid)}/access`, d); viewProject(uid); } catch (err) { toast(err.message); } };
      root.querySelectorAll("[data-level]").forEach((sel) => sel.onchange = async () => { try { await api("PUT", `/projects/${encodeURIComponent(uid)}/access`, { email: sel.dataset.level, level: sel.value }); toast("updated"); } catch (err) { toast(err.message); viewProject(uid); } });
      root.querySelectorAll("[data-remove-access]").forEach((b) => b.onclick = async () => { try { await api("DELETE", `/projects/${encodeURIComponent(uid)}/access/${encodeURIComponent(b.dataset.removeAccess)}`); viewProject(uid); } catch (err) { toast(err.message); } });
      root.querySelectorAll("[data-revoke-grant]").forEach((b) => b.onclick = async () => { try { await api("DELETE", `/account/query-keys/${b.dataset.revokeGrant}/grants/${encodeURIComponent(uid)}`); viewProject(uid); } catch (err) { toast(err.message); } });
    }
  }

  // ----------------------------------------------------------- query keys
  async function viewQueryKeys(reveal) {
    const [d, list] = await Promise.all([api("GET", "/account/query-keys"), api("GET", "/projects")]);
    const projects = (list.projects || []).filter((p) => p.level === "owner");
    const options = projects.map((p) => h`<option value="${p.uid}">${p.name || p.uid}</option>`).join("");
    const cards = d.query_keys.map(({ key, grants }) => h`
      <div class="card"><div class="card-head"><h2>${key.name} <span class="faint mono" style="font-weight:400">${key.prefix}…</span></h2>
        <div class="actions"><span class="muted small">${key.last_used_at ? "used " + when(key.last_used_at) : "never used"}</span><button class="btn sm" data-rotate="${key.id}">Rotate</button><button class="btn sm quiet danger" data-revoke-key="${key.id}">Revoke</button></div></div>
        <div class="card-body flush">
          ${raw(grants.length ? h`<table><thead><tr><th>project</th><th>branch</th><th></th></tr></thead><tbody>${raw(grants.map((g) => h`<tr><td><a href="/app/projects/${g.project_uid}" data-link>${g.project_uid}</a></td><td class="mono">${g.branch === null ? "production" : g.branch === "*" ? "every branch" : g.branch}</td><td class="actions"><button class="btn sm quiet danger" data-revoke-grant="${key.id}" data-project="${g.project_uid}">Revoke</button></td></tr>`).join(""))}</tbody></table>` : h`<div class="empty">Reads nothing yet. Grant it a project below.</div>`)}
          ${projects.length ? raw(h`<form class="inline" data-grant="${key.id}" style="padding:12px 16px;border-top:1px solid var(--line)"><div class="field"><label>Project you own</label><select name="project">${raw(options)}</select></div><div class="field narrow"><label>Branch</label><input name="branch" class="mono" placeholder="production"></div><button class="btn" type="submit">Grant read</button></form>`) : ""}
        </div></div>`).join("");
    frame("/app/query-keys", h`
      <div class="page-head"><div><h1>Query keys</h1><div class="sub">What your applications send. Read only, on the projects and branches they are granted. <code>*</code> means every branch.</div></div></div>
      ${raw(reveal ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>${reveal.name}</h2>${raw(secretBlock(reveal.secret, "Put it in your app's configuration as the Bearer token for /sql and /explore."))}</div></div>` : "")}
      <div class="card" style="margin-bottom:20px"><div class="card-body"><form id="new-key" class="inline"><div class="field"><label>New query key</label><input name="name" required placeholder="web app, prod"></div><button class="btn primary" type="submit">Create</button></form></div></div>
      <div class="grid">${raw(cards || h`<div class="empty">No query keys yet.</div>`)}</div>`);
    const nk = document.getElementById("new-key");
    nk.onsubmit = async (e) => { e.preventDefault(); try { const r = await api("POST", "/account/query-keys", Object.fromEntries(new FormData(nk).entries())); viewQueryKeys({ name: r.key.name, secret: r.secret }); } catch (err) { toast(err.message); } };
    root.querySelectorAll("[data-grant]").forEach((f) => f.onsubmit = async (e) => { e.preventDefault(); const d = Object.fromEntries(new FormData(f).entries()); if (!d.branch) delete d.branch; try { await api("POST", `/account/query-keys/${f.dataset.grant}/grants`, d); viewQueryKeys(); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-revoke-grant]").forEach((b) => b.onclick = async () => { try { await api("DELETE", `/account/query-keys/${b.dataset.revokeGrant}/grants/${encodeURIComponent(b.dataset.project)}`); viewQueryKeys(); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-rotate]").forEach((b) => b.onclick = async () => { if (!confirm("Rotate this key? The current secret stops working at once.")) return; try { const r = await api("POST", `/account/query-keys/${b.dataset.rotate}/rotate`); viewQueryKeys({ name: r.key.name + " (rotated)", secret: r.secret }); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-revoke-key]").forEach((b) => b.onclick = async () => { if (!confirm("Revoke this key? Apps using it stop working.")) return; try { await api("DELETE", `/account/query-keys/${b.dataset.revokeKey}`); viewQueryKeys(); } catch (err) { toast(err.message); } });
  }

  // ------------------------------------------------------------- my keys
  async function viewKeys(reveal) {
    const d = await api("GET", "/account/keys");
    const rows = d.keys.map((k) => h`<tr><td>${k.name}</td><td class="mono">${k.prefix}…</td><td class="muted small">${when(k.created_at)}</td><td class="muted small">${k.last_used_at ? when(k.last_used_at) : "never"}</td><td class="actions"><button class="btn sm quiet danger" data-revoke="${k.id}">Revoke</button></td></tr>`).join("");
    frame("/app/keys", h`
      <div class="page-head"><div><h1>My keys</h1><div class="sub">Personal keys do exactly what you can do. One per machine; store it with <code>zsql auth --api-key …</code>.</div></div></div>
      ${raw(reveal ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>${reveal.name}</h2>${raw(secretBlock(reveal.secret, "Run `zsql auth --api-key <key>` inside a project to store it."))}</div></div>` : "")}
      <div class="card" style="margin-bottom:20px"><div class="card-body"><form id="new-key" class="inline"><div class="field"><label>New key</label><input name="name" required placeholder="laptop"></div><button class="btn primary" type="submit">Create</button></form></div></div>
      <div class="card"><div class="card-body flush"><table><thead><tr><th>name</th><th>prefix</th><th>created</th><th>last used</th><th></th></tr></thead><tbody>${raw(rows)}</tbody></table></div></div>
      <div class="card" style="margin-top:20px"><div class="card-head"><h2>Password</h2></div><div class="card-body"><form id="password" class="inline"><div class="field"><label>New password</label><input name="password" type="password" minlength="8" required></div><button class="btn" type="submit">Change</button></form></div></div>`);
    const nk = document.getElementById("new-key");
    nk.onsubmit = async (e) => { e.preventDefault(); try { const r = await api("POST", "/account/keys", Object.fromEntries(new FormData(nk).entries())); viewKeys({ name: r.key.name, secret: r.secret }); } catch (err) { toast(err.message); } };
    root.querySelectorAll("[data-revoke]").forEach((b) => b.onclick = async () => { if (!confirm("Revoke this key?")) return; try { await api("DELETE", `/account/keys/${b.dataset.revoke}`); viewKeys(); } catch (err) { toast(err.message); } });
    const pw = document.getElementById("password");
    pw.onsubmit = async (e) => { e.preventDefault(); try { await api("POST", "/account/password", Object.fromEntries(new FormData(pw).entries())); pw.reset(); toast("password changed"); } catch (err) { toast(err.message); } };
  }

  // -------------------------------------------------------------- members
  async function viewMembers(invite) {
    const d = await api("GET", "/account/members");
    const admin = state.me.role === "admin";
    const rows = d.members.map((m) => h`<tr><td>${m.user.email}${m.user.name ? raw(` <span class="faint">${esc(m.user.name)}</span>`) : ""}</td><td>${pill(m.role)}</td>
      <td class="actions">${admin ? raw(h`<select data-role="${m.user.email}" style="width:auto"><option ${m.role === "developer" ? "selected" : ""}>developer</option><option ${m.role === "admin" ? "selected" : ""}>admin</option></select><button class="btn sm" data-invite="${m.user.email}">New invite</button><button class="btn sm quiet danger" data-remove="${m.user.email}">Remove</button>`) : ""}</td></tr>`).join("");
    frame("/app/members", h`
      <div class="page-head"><div><h1>Members</h1><div class="sub">Developers read every project and own the ones they create. Admins own everything and manage the account.</div></div></div>
      ${raw(invite ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>Invite for ${invite.email}</h2>${raw(secretBlock(invite.token, "Send them the link " + location.origin + "/app/invite?token=… or the token itself; it works once."))}</div></div>` : "")}
      ${admin ? raw(h`<div class="card" style="margin-bottom:20px"><div class="card-body"><form id="add" class="inline"><div class="field"><label>Email</label><input name="email" type="email" required placeholder="bob@acme.com"></div><div class="field"><label>Name</label><input name="name" placeholder="Bob"></div><div class="field narrow"><label>Role</label><select name="role"><option>developer</option><option>admin</option></select></div><button class="btn primary" type="submit">Add member</button></form></div></div>`) : ""}
      <div class="card"><div class="card-body flush"><table><thead><tr><th>user</th><th>role</th><th></th></tr></thead><tbody>${raw(rows)}</tbody></table></div></div>`);
    if (!admin) return;
    const add = document.getElementById("add");
    add.onsubmit = async (e) => { e.preventDefault(); const d = Object.fromEntries(new FormData(add).entries()); try { const r = await api("POST", "/account/members", d); viewMembers(r.invite ? { email: r.user.email, token: r.invite } : null); if (!r.invite) toast("role updated"); } catch (err) { toast(err.message); } };
    root.querySelectorAll("[data-role]").forEach((sel) => sel.onchange = async () => { try { await api("POST", "/account/members", { email: sel.dataset.role, role: sel.value }); toast("updated"); } catch (err) { toast(err.message); viewMembers(); } });
    root.querySelectorAll("[data-invite]").forEach((b) => b.onclick = async () => { try { const r = await api("POST", `/account/members/${encodeURIComponent(b.dataset.invite)}/invite`); viewMembers({ email: b.dataset.invite, token: r.invite }); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-remove]").forEach((b) => b.onclick = async () => { if (!confirm(`Remove ${b.dataset.remove}? Their keys stop working.`)) return; try { await api("DELETE", `/account/members/${encodeURIComponent(b.dataset.remove)}`); viewMembers(); } catch (err) { toast(err.message); } });
  }

  // ---------------------------------------------------------------- audit
  async function viewAudit() {
    const d = await api("GET", "/account/audit?limit=200");
    const rows = d.events.map((e) => h`<tr><td class="muted small mono">${when(e.at)}</td><td>${e.actor}</td><td><code>${e.action}</code></td><td class="muted">${e.subject}</td></tr>`).join("");
    frame("/app/audit", h`
      <div class="page-head"><div><h1>Audit</h1><div class="sub">Who did what: sign-ups, members, keys, grants, deploys.</div></div></div>
      <div class="card"><div class="card-body flush"><table><thead><tr><th>when</th><th>actor</th><th>action</th><th>subject</th></tr></thead><tbody>${raw(rows)}</tbody></table></div></div>`);
  }

  // ---------------------------------------------------------------- route
  async function loadMe() {
    try { const me = await api("GET", "/me"); state.me = me.kind === "user" ? me : null; } catch (_) { state.me = null; }
  }

  async function route() {
    const path = location.pathname.replace(/\/+$/, "") || "/app";
    if (path === "/app/signup") return viewSignup();
    if (path === "/app/invite") return viewInvite();
    if (path === "/app/login") return viewLogin();
    if (!state.me) await loadMe();
    if (!state.me) return viewLogin();
    try {
      if (path === "/app") return await viewProjects();
      if (path === "/app/query-keys") return await viewQueryKeys();
      if (path === "/app/keys") return await viewKeys();
      if (path === "/app/members") return await viewMembers();
      if (path === "/app/audit") return await viewAudit();
      const m = path.match(/^\/app\/projects\/([^/]+)$/);
      if (m) return await viewProject(decodeURIComponent(m[1]));
      go("/app/");
    } catch (e) {
      if (e.status === 401) { state.me = null; return viewLogin(); }
      frame(path, h`<div class="alert error">${e.message}</div>`);
    }
  }
  route();
})();
