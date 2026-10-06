// 0sql app. One shell, views rendered from the JSON API. The path says
// which view; the session cookie says who.
//
// Icons are Lucide (https://lucide.dev, ISC licence; see LICENSE-lucide),
// inlined below as their path data and drawn in currentColor, so they stay
// monochrome and take the rust only where the text around them does.
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

  const ICONS = {
    "layers": '<path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z"/><path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12"/><path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17"/>',
    "key-round": '<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5" fill="currentColor"/>',
    "key": '<path d="m15.5 7.5 2.3 2.3a1 1 0 0 0 1.4 0l2.1-2.1a1 1 0 0 0 0-1.4L19 4"/><path d="m21 2-9.6 9.6"/><circle cx="7.5" cy="15.5" r="5.5"/>',
    "users": '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><path d="M16 3.128a4 4 0 0 1 0 7.744"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><circle cx="9" cy="7" r="4"/>',
    "scroll-text": '<path d="M15 12h-5"/><path d="M15 8h-5"/><path d="M19 17V5a2 2 0 0 0-2-2H4"/><path d="M8 21h12a2 2 0 0 0 2-2v-1a1 1 0 0 0-1-1H11a1 1 0 0 0-1 1v1a2 2 0 1 1-4 0V5a2 2 0 1 0-4 0v2a1 1 0 0 0 1 1h3"/>',
    "sun": '<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>',
    "moon": '<path d="M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"/>',
    "monitor": '<rect width="20" height="14" x="2" y="3" rx="2"/><line x1="8" x2="16" y1="21" y2="21"/><line x1="12" x2="12" y1="17" y2="21"/>',
    "log-out": '<path d="m16 17 5-5-5-5"/><path d="M21 12H9"/><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/>',
    "copy": '<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
    "check": '<path d="M20 6 9 17l-5-5"/>',
    "refresh-cw": '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>',
    "trash-2": '<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
    "plus": '<path d="M5 12h14"/><path d="M12 5v14"/>',
    "play": '<path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z"/>',
    "git-branch": '<line x1="6" x2="6" y1="3" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>',
    "search": '<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>',
    "compass": '<path d="m16.24 7.76-1.804 5.411a2 2 0 0 1-1.265 1.265L7.76 16.24l1.804-5.411a2 2 0 0 1 1.265-1.265z"/><circle cx="12" cy="12" r="10"/>',
    "book-open": '<path d="M12 7v14"/><path d="M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"/>',
    "shield": '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/>',
    "settings": '<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/>',
    "lock": '<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
    "shield-check": '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
    "inbox": '<polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/>',
    "circle-alert": '<circle cx="12" cy="12" r="10"/><line x1="12" x2="12" y1="8" y2="12"/><line x1="12" x2="12.01" y1="16" y2="16"/>',
    "circle-check": '<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>',
    "triangle-alert": '<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
    "chevron-right": '<path d="m9 18 6-6-6-6"/>',
    "chevron-down": '<path d="m6 9 6 6 6-6"/>',
    "circle-user": '<circle cx="12" cy="12" r="10"/><circle cx="12" cy="10" r="3"/><path d="M7 20.662V19a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v1.662"/>',
    "arrow-right": '<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>',
    "hash": '<line x1="4" x2="20" y1="9" y2="9"/><line x1="4" x2="20" y1="15" y2="15"/><line x1="10" x2="8" y1="3" y2="21"/><line x1="16" x2="14" y1="3" y2="21"/>',
    "sigma": '<path d="M18 7V5a1 1 0 0 0-1-1H6.5a.5.5 0 0 0-.4.8l4.5 6a2 2 0 0 1 0 2.4l-4.5 6a.5.5 0 0 0 .4.8H17a1 1 0 0 0 1-1v-2"/>',
    "rocket": '<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0"/><path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>',
    "history": '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
    "database": '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5V19A9 3 0 0 0 21 19V5"/><path d="M3 12A9 3 0 0 0 21 12"/>',
    "list-tree": '<path d="M8 5h13"/><path d="M13 12h8"/><path d="M13 19h8"/><path d="M3 10a2 2 0 0 0 2 2h3"/><path d="M3 5v12a2 2 0 0 0 2 2h3"/>',
    "x": '<path d="M18 6 6 18"/><path d="m6 6 12 12"/>',
    "user-plus": '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><line x1="19" x2="19" y1="8" y2="14"/><line x1="22" x2="16" y1="11" y2="11"/>',
    "clock": '<path d="M12 6v6l4 2"/><circle cx="12" cy="12" r="10"/>',
    "terminal": '<path d="M12 19h8"/><path d="m4 17 6-6-6-6"/>',
    "braces": '<path d="M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5c0 1.1.9 2 2 2h1"/><path d="M16 21h1a2 2 0 0 0 2-2v-5c0-1.1.9-2 2-2a2 2 0 0 1-2-2V5a2 2 0 0 0-2-2h-1"/>',
    "mail": '<path d="m22 7-8.991 5.727a2 2 0 0 1-2.009 0L2 7"/><rect x="2" y="4" width="20" height="16" rx="2"/>',
    "send": '<path d="M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11z"/><path d="m21.854 2.147-10.94 10.939"/>',
  };
  const icon = (name, cls) => raw(`<svg class="i${cls ? " " + cls : ""}" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] || ""}</svg>`);
  // A dimension or a measure, as a small badge: hash for a dimension, sigma for a measure.
  const kindBadge = (kind) => raw(h`<span class="kind ${kind}" title="${kind === "dim" ? "dimension" : "measure"}">${icon(kind === "dim" ? "hash" : "sigma")}</span>`);
  const logo = () => raw(h`<a class="logo" href="/app/" data-link aria-label="0sql"><img class="on-light" src="/app/brand/wordmark-light.svg" alt="0sql"><img class="on-dark" src="/app/brand/wordmark-dark.svg" alt="0sql"></a>`);

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
      // 429, and 503 with a delay, mean the server refused this before
      // running any of it, and said when to come back.
      err.retryAfter = Number(r.headers.get("retry-after")) || 0;
      throw err;
    }
    return data;
  }

  function toast(msg, ok) {
    let t = document.querySelector(".toast");
    if (!t) { t = document.createElement("div"); t.className = "toast"; document.body.appendChild(t); }
    t.innerHTML = h`${ok ? icon("check", "sm") : ""}${msg}`; t.classList.add("show");
    clearTimeout(t._timer); t._timer = setTimeout(() => t.classList.remove("show"), 1800);
  }

  function copy(text) { navigator.clipboard && navigator.clipboard.writeText(text).then(() => toast("copied", true)); }

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
    const b = document.getElementById("theme");
    if (b) { b.innerHTML = themeIcon(); b.title = themeTitle(); }
  }
  const themeIcon = () => { const t = document.documentElement.getAttribute("data-theme"); return icon(t === "dark" ? "moon" : t === "light" ? "sun" : "monitor").__raw; };
  const themeTitle = () => { const t = document.documentElement.getAttribute("data-theme"); return "theme: " + (t || "system") + " (click to change)"; };
  try { const t = localStorage.getItem("zsql-theme"); if (t && t !== "auto") document.documentElement.setAttribute("data-theme", t); } catch (_) {}

  // Scrollbars are revealed by a class rather than by `:hover`, because
  // Blink and WebKit do not restyle a custom scrollbar when the element
  // it belongs to is hovered: the class is a style change they do act
  // on. Only the colour ever changes, so nothing reflows. Firefox needs
  // none of this and does it in the stylesheet.
  let showing = [];
  const scrolls = (el) => {
    if (!(el instanceof Element) || el === document.body) return false;
    // The page's own bar is always inked, so it is not marked here.
    if (el === document.documentElement) return false;
    const o = getComputedStyle(el).overflowY;
    return (o === "auto" || o === "scroll") && el.scrollHeight > el.clientHeight + 1;
  };
  function reveal(from) {
    const want = [];
    for (let el = from instanceof Element ? from : null; el; el = el.parentElement) if (scrolls(el)) want.push(el);
    for (const el of showing) if (!want.includes(el)) el.classList.remove("scrolling");
    for (const el of want) el.classList.add("scrolling");
    showing = want;
  }
  document.addEventListener("mouseover", (e) => reveal(e.target));
  document.addEventListener("focusin", (e) => reveal(e.target));
  // The pointer leaving the window fires one mouseout with nothing on
  // the other side of it.
  document.addEventListener("mouseout", (e) => { if (!e.relatedTarget) reveal(null); });

  const secretBlock = (secret, note, label) => h`
    <div class="secret">
      <div class="label">${icon("triangle-alert", "sm")}Shown once. Copy it now.</div>
      <code>${secret}</code>
      <div class="actions" style="margin-top:8px"><button class="btn sm" data-copy="${secret}">${icon("copy")}${label || "Copy"}</button></div>
      <div class="note">${note}</div>
    </div>`;

  // An invite is only ever redeemed in the browser, and the form there
  // fills itself from the link, so the link is the whole thing. Nothing
  // takes the token on its own.
  const inviteLink = (token) => `${location.origin}/app/invite?token=${encodeURIComponent(token)}`;

  const alertBox = (msg, kind) => msg ? h`<div class="alert ${kind || "error"}">${icon(kind === "ok" ? "circle-check" : "circle-alert")}<div>${msg}</div></div>` : "";

  // ---------------------------------------------------------------- frame
  // `tall` lays the view out against the window rather than letting it
  // grow: the playground wants its panels to stay on screen and scroll
  // their own content.
  function frame(active, body, tall) {
    const me = state.me;
    const isAdmin = me && me.role === "admin";
    const tabs = [["/app/", "Projects", "layers"], ["/app/query-keys", "Query keys", "key-round"], ["/app/keys", "My keys", "key"], ["/app/members", "Members", "users"]];
    if (isAdmin) tabs.push(["/app/audit", "Audit", "scroll-text"]);
    root.innerHTML = h`
      <div class="topbar">
        ${logo()}
        <nav class="nav">${raw(tabs.map(([p, t, i]) => h`<a href="${p}" data-link class="${p === active ? "active" : ""}">${icon(i)}<span>${t}</span></a>`).join(""))}</nav>
        <div class="spacer"></div>
        <div class="who">
          <button class="theme" id="theme" title="${themeTitle()}">${raw(themeIcon())}</button>
          <div class="menu-wrap">
            <button class="who-btn" id="who-btn" aria-haspopup="menu" aria-expanded="false">${icon("circle-user")}<span class="who-name">${me.account.name}</span>${icon("chevron-down", "sm")}</button>
            <div class="menu" id="who-menu" role="menu" hidden>
              <div class="menu-head">
                <div class="menu-name">${me.user.name || me.user.email}</div>
                <div class="menu-sub">${me.user.email}</div>
                <div class="menu-sub"><b>${me.account.name}</b> ${pill(me.role)}</div>
              </div>
              <a class="menu-item" href="/app/keys" data-link role="menuitem">${icon("key")}My keys</a>
              <button class="menu-item danger" id="logout" role="menuitem">${icon("log-out")}Sign out</button>
            </div>
          </div>
        </div>
      </div>
      <div class="main">${raw(unverifiedBanner())}${raw(body)}</div>`;
    root.classList.toggle("tall", !!tall);
    document.getElementById("logout").onclick = async () => { await api("POST", "/logout"); state.me = null; go("/app/login"); };
    document.getElementById("theme").onclick = () => theme();
    const resend = document.getElementById("resend-verify");
    if (resend) resend.onclick = async () => {
      resend.disabled = true;
      try {
        const r = await api("POST", "/account/verify-email");
        toast(r.sent ? "link sent" : r.waiting ? "a link was sent moments ago; check your inbox" : "mail is not configured on this server; ask your operator", r.sent);
      } catch (err) { toast(err.message); }
      resend.disabled = false;
    };
    whoMenu();
  }

  // Nothing is withheld for an unconfirmed address; it is the address
  // support and billing would use, so it is worth saying once per page
  // and not worth blocking anyone over.
  function unverifiedBanner() {
    const me = state.me;
    if (!me || !me.user || me.user.email_verified) return "";
    return h`<div class="alert">${icon("mail")}<div>Confirm <b>${me.user.email}</b> so it can be used for support and recovery.</div>
      <button class="btn sm" id="resend-verify" style="margin-left:auto;flex:none">Send the link</button></div>`;
  }

  // The account menu: who you are, and the one thing you do to that.
  // The theme button stays beside it rather than inside it, since it is
  // reached for often and belongs to the browser, not the account.
  function whoMenu() {
    const trigger = document.getElementById("who-btn");
    const menu = document.getElementById("who-menu");
    const onOutside = (e) => {
      if (!menu.contains(e.target) && !trigger.contains(e.target)) close();
    };
    const onKey = (e) => {
      if (e.key === "Escape") { close(); trigger.focus(); }
    };
    function close() {
      menu.hidden = true;
      trigger.setAttribute("aria-expanded", "false");
      document.removeEventListener("click", onOutside);
      document.removeEventListener("keydown", onKey);
    }
    trigger.onclick = (e) => {
      e.stopPropagation();
      if (!menu.hidden) return close();
      menu.hidden = false;
      trigger.setAttribute("aria-expanded", "true");
      document.addEventListener("click", onOutside);
      document.addEventListener("keydown", onKey);
    };
    menu.onclick = (e) => { if (e.target.closest("[role=menuitem]")) close(); };
  }

  function authFrame(body) {
    root.innerHTML = h`<div class="auth"><div>${logo()}<div class="card"><div class="card-body">${raw(body)}</div></div></div></div>`;
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
    a.innerHTML = h`${icon("circle-alert")}<div>${msg}</div>`;
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
      <div class="alt">New here? <a href="/app/signup" data-link>Create an account</a></div>
      <div class="alt"><a href="/app/forgot" data-link>Forgot your password?</a></div>`);
    form(root, async (d) => { await api("POST", "/login", d); await loadMe(); go("/app/"); });
  }

  // Asking for a reset link. The answer is the same whether or not the
  // address has an account, so the view says what was done rather than
  // what was found: anything else would make this a way to test which
  // addresses are registered.
  function viewForgot() {
    authFrame(h`
      <h1 style="margin-bottom:4px">Reset your password</h1>
      <p class="muted" style="margin-bottom:16px">We will send a link to your address. It works once and expires in an hour.</p>
      <form>
        <div class="field"><label>Email</label><input name="email" type="email" required autofocus></div>
        <button class="btn primary" type="submit" style="width:100%;justify-content:center">Send the link</button>
      </form>
      <div class="alt"><a href="/app/login" data-link>Back to sign in</a></div>`);
    form(root, async (d) => {
      await api("POST", "/password-reset", d);
      authFrame(h`
        <h1 style="margin-bottom:4px">Check your email</h1>
        <p class="muted">If <b>${d.email}</b> has an account, a link is on its way. It works once and expires in an hour.</p>
        <p class="muted" style="margin-bottom:16px">Nothing has changed yet, and your current password still works until you use the link.</p>
        <a class="btn primary" href="/app/login" data-link>Back to sign in${icon("arrow-right")}</a>`);
    });
  }

  // Setting a password from a link. Succeeding signs the user in, since
  // the one thing they came to do should not be followed by a login form.
  function viewReset() {
    const token = new URLSearchParams(location.search).get("token") || "";
    // The token is a credential; once read it leaves the address bar,
    // so it is not kept in history or sent along as a referrer.
    if (token) history.replaceState(null, "", location.pathname);
    authFrame(h`
      <h1 style="margin-bottom:4px">Set a new password</h1>
      <p class="muted" style="margin-bottom:16px">This signs you in, and signs out anything else that was.</p>
      <form>
        ${raw(token ? "" : h`<div class="field"><label>Reset token</label><input name="token" class="mono" required placeholder="zrs_..."></div>`)}
        <div class="field"><label>New password</label><input name="password" type="password" required minlength="8" autofocus></div>
        <button class="btn primary" type="submit" style="width:100%;justify-content:center">Set password and sign in</button>
      </form>
      <div class="alt"><a href="/app/forgot" data-link>Ask for a new link</a></div>`);
    form(root, async (d) => {
      await api("POST", "/password-reset/accept", { token: d.token || token, password: d.password });
      await loadMe();
      go("/app/");
    });
  }

  // Confirming an address. The link is usually opened in a different
  // browser than the one that signed up, so this works signed out and
  // only offers the way on when there is a session to use it with.
  async function viewVerify() {
    const token = new URLSearchParams(location.search).get("token") || "";
    authFrame(h`<h1>Confirming your address…</h1>`);
    try {
      const r = await api("POST", "/verify-email", { token });
      await loadMe();
      authFrame(h`
        <h1 style="display:flex;align-items:center;gap:8px;margin-bottom:4px">${icon("circle-check")}Address confirmed</h1>
        <p class="muted" style="margin-bottom:16px"><b>${r.user.email}</b> is confirmed for <b>${r.account.name}</b>.</p>
        ${raw(state.me ? h`<a class="btn primary" href="/app/" data-link>Go to projects${icon("arrow-right")}</a>` : h`<a class="btn primary" href="/app/login" data-link>Sign in${icon("arrow-right")}</a>`)}`);
    } catch (e) {
      authFrame(h`
        <h1 style="margin-bottom:12px">That link did not work</h1>
        ${raw(alertBox(e.message))}
        <p class="muted" style="margin-bottom:16px">Links work once and expire. Sign in and ask for a new one from the banner at the top of any page.</p>
        <a class="btn" href="/app/login" data-link>Sign in${icon("arrow-right")}</a>`);
    }
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
      <div class="alt">Have one? <a href="/app/login" data-link>Sign in</a></div>`);
    form(root, async (d) => {
      const r = await api("POST", "/signup", d);
      await loadMe();
      authFrame(h`
        <h1>Welcome, ${r.user.name || r.user.email}</h1>
        <p class="muted">Your account <b>${r.account.name}</b> is ready. This is your personal key for the CLI:</p>
        ${raw(secretBlock(r.secret, "Run `zsql auth --api-key <key>` inside a project to store it. You can make more keys under My keys."))}
        <a class="btn primary" href="/app/" data-link>Go to projects${icon("arrow-right")}</a>`);
    });
  }

  function viewInvite() {
    const token = new URLSearchParams(location.search).get("token") || "";
    // The token is a credential; once read it leaves the address bar,
    // so it is not kept in history or sent along as a referrer.
    if (token) history.replaceState(null, "", location.pathname);
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
        <a class="btn primary" href="/app/" data-link>Go to projects${icon("arrow-right")}</a>`);
    });
  }

  // ------------------------------------------------------------- projects
  async function viewProjects() {
    const d = await api("GET", "/projects");
    const byProject = {};
    for (const dep of d.deployments) (byProject[dep.project] = byProject[dep.project] || []).push(dep);
    const rows = (d.projects || []).map((p) => {
      const deps = byProject[p.uid] || [];
      return h`<a class="row" href="/app/projects/${p.uid}" data-link>
        <div class="grow"><div class="title">${p.name || p.uid} <span class="faint mono">${p.uid}</span></div>
          <div class="meta"><span>${icon("git-branch")}<code>${p.production_branch}</code></span>${p.protected_production ? raw(h`<span>${icon("shield-check")}protected</span>`) : ""}${p.visibility === "restricted" ? raw(h`<span>${icon("lock")}restricted</span>`) : ""}<span>${icon("rocket")}${deps.length} deployed branch${deps.length === 1 ? "" : "es"}</span></div></div>
        ${pill(p.level)}${icon("chevron-right", "go")}</a>`;
    }).join("");
    frame("/app/", h`
      <div class="page-head"><div><h1>Projects</h1><div class="sub">Deploy from a project directory with <code>zsql deploy</code>; the first deploy creates the project.</div></div></div>
      <div class="card"><div class="card-head"><h2>${icon("layers")}${state.me.account.name}</h2><span class="muted small">${(d.projects || []).length} project${(d.projects || []).length === 1 ? "" : "s"}</span></div>
        <div class="card-body flush rows">${raw(rows || h`<div class="empty">${icon("inbox", "xl")}<div>No projects yet. Run <code>zsql init</code>, then <code>zsql auth --api-key …</code> and <code>zsql deploy</code>.</div></div>`)}</div></div>`);
  }

  async function viewProject(uid) {
    let access, list;
    try { [access, list] = await Promise.all([api("GET", `/projects/${encodeURIComponent(uid)}/access`), api("GET", "/projects")]); }
    catch (e) { return frame("/app/", alertBox(e.message)); }
    const p = access.project;
    const mine = (list.projects || []).find((x) => x.uid === uid) || {};
    const owner = mine.level === "owner";
    const deps = list.deployments.filter((x) => x.project === uid);
    const depRows = deps.map((x) => h`<tr><td class="mono">${x.branch}</td><td>${x.tables} tables · ${x.fields} fields · ${x.joins} joins · ${x.policies} policies</td><td class="muted small">${x.deployed_at}</td><td class="actions"><a class="btn sm" href="/app/projects/${uid}/play?branch=${encodeURIComponent(x.branch)}" data-link>${icon("play")}Playground</a></td></tr>`).join("");
    const accessRows = access.access.map((a) => h`<tr><td>${a.user.email}${a.user.name ? raw(` <span class="faint">${esc(a.user.name)}</span>`) : ""}</td><td>${pill(a.level)}</td>
      <td class="actions">${owner ? raw(h`<select class="sm" data-level="${a.user.email}" style="width:auto"><option ${a.level === "read" ? "selected" : ""}>read</option><option ${a.level === "write" ? "selected" : ""}>write</option><option ${a.level === "owner" ? "selected" : ""}>owner</option></select><button class="btn sm quiet danger" data-remove-access="${a.user.email}">${icon("x")}Remove</button>`) : ""}</td></tr>`).join("");
    const keyRows = access.query_keys.map((k) => h`<tr><td>${k.key.name} <span class="faint mono">${k.key.prefix}…</span></td><td class="mono">${k.grant.branch === null ? p.production_branch + " (production)" : k.grant.branch === "*" ? "every branch" : k.grant.branch}</td>
      <td class="actions">${owner ? raw(h`<button class="btn sm quiet danger" data-revoke-grant="${k.key.id}">${icon("x")}Revoke</button>`) : ""}</td></tr>`).join("");
    frame("/app/", h`
      <div class="page-head"><div><div class="crumbs"><a href="/app/" data-link>Projects</a>${icon("chevron-right")}<span>${p.uid}</span></div><h1>${p.name || p.uid}</h1></div>
        <div class="actions">${pill(mine.level || "read")}${deps.length ? raw(h`<a class="btn primary" href="/app/projects/${uid}/play?branch=${encodeURIComponent(deps.some((x) => x.branch === p.production_branch) ? p.production_branch : deps[0].branch)}" data-link>${icon("play")}Playground</a>`) : ""}</div></div>
      <div class="grid two">
        <div class="card"><div class="card-head"><h2>${icon("git-branch")}Deployed branches</h2><span class="muted small mono">zsql deploy</span></div>
          <div class="card-body flush">${raw(deps.length ? h`<table><thead><tr><th>branch</th><th>model</th><th>deployed</th><th></th></tr></thead><tbody>${raw(depRows)}</tbody></table>` : h`<div class="empty">${icon("rocket", "xl")}<div>Nothing deployed yet.</div></div>`)}</div></div>
        <div class="card"><div class="card-head"><h2>${icon("settings")}Settings</h2>${owner ? "" : raw('<span class="muted small">owners edit</span>')}</div>
          <div class="card-body"><form id="settings">
            <div class="field"><label>Name</label><input name="name" value="${p.name}" ${owner ? "" : "disabled"}></div>
            <div class="field"><label>Production branch</label><input name="production_branch" class="mono" value="${p.production_branch}" ${owner ? "" : "disabled"}></div>
            <div class="field"><label>Visibility</label><select name="visibility" ${owner ? "" : "disabled"}><option value="account" ${p.visibility === "account" ? "selected" : ""}>account: every developer reads it</option><option value="restricted" ${p.visibility === "restricted" ? "selected" : ""}>restricted: granted users only</option></select></div>
            <div class="field"><label class="switch"><input type="checkbox" name="protected_production" ${p.protected_production ? "checked" : ""} ${owner ? "" : "disabled"}> Deploys to the production branch need an owner</label></div>
            ${owner ? raw(h`<div class="actions"><button class="btn primary" type="submit">${icon("check")}Save</button><span class="spacer"></span><button class="btn danger quiet" type="button" id="delete">${icon("trash-2")}Delete project</button></div>`) : ""}
          </form></div></div>
        <div class="card"><div class="card-head"><h2>${icon("users")}People</h2><span class="muted small">developers read by default</span></div>
          <div class="card-body flush"><table><thead><tr><th>user</th><th>level</th><th></th></tr></thead><tbody>${raw(accessRows)}</tbody></table>
          ${owner ? raw(h`<form id="grant-user" class="inline" style="padding:12px 16px;border-top:1px solid var(--line)"><div class="field"><label>Member email</label><input name="email" type="email" required placeholder="bob@acme.com"></div><div class="field narrow"><label>Level</label><select name="level"><option>read</option><option>write</option><option>owner</option></select></div><button class="btn" type="submit">${icon("plus")}Grant</button></form>`) : ""}</div></div>
        <div class="card"><div class="card-head"><h2>${icon("key-round")}Query keys on this project</h2><a href="/app/query-keys" data-link class="small">manage keys ${icon("arrow-right", "sm")}</a></div>
          <div class="card-body flush">${raw(access.query_keys.length ? h`<table><thead><tr><th>key</th><th>branch</th><th></th></tr></thead><tbody>${raw(keyRows)}</tbody></table>` : h`<div class="empty">${icon("key-round", "xl")}<div>No query key reads this project.</div></div>`)}</div></div>
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
    const options = projects.map((p) => h`<option value="${p.uid}">${p.name && p.name !== p.uid ? p.name + " · " + p.uid : p.uid}</option>`).join("");
    // The branch is one the server serves: the project's production branch
    // (the default), every branch, or a deployed one by name.
    const branchOptions = (uid) => {
      const p = projects.find((x) => x.uid === uid) || {};
      const deployed = list.deployments.filter((d) => d.project === uid).map((d) => d.branch).sort();
      return h`<option value="">production (${p.production_branch || "main"})</option><option value="*">every branch</option>` +
        deployed.filter((b) => b !== p.production_branch).map((b) => h`<option value="${b}">${b}</option>`).join("");
    };
    const cards = d.query_keys.map(({ key, grants }) => h`
      <div class="card"><div class="card-head"><h2>${icon("key-round")}${key.name} <span class="faint mono" style="font-weight:400">${key.prefix}…</span></h2>
        <div class="actions"><span class="muted small stat">${icon("clock", "sm")}${key.last_used_at ? "used " + when(key.last_used_at) : "never used"}</span><button class="btn sm" data-rotate="${key.id}">${icon("refresh-cw")}Rotate</button><button class="btn sm quiet danger" data-revoke-key="${key.id}">${icon("trash-2")}Revoke</button></div></div>
        <div class="card-body flush">
          ${raw(grants.length ? h`<table><thead><tr><th>project</th><th>branch</th><th></th></tr></thead><tbody>${raw(grants.map((g) => h`<tr><td><a href="/app/projects/${g.project_uid}" data-link>${g.project_uid}</a></td><td class="mono">${g.branch === null ? "production" : g.branch === "*" ? "every branch" : g.branch}</td><td class="actions"><button class="btn sm quiet danger" data-revoke-grant="${key.id}" data-project="${g.project_uid}">${icon("x")}Revoke</button></td></tr>`).join(""))}</tbody></table>` : h`<div class="empty small">Reads nothing yet. Grant it a project below.</div>`)}
          ${projects.length ? raw(h`<form class="inline" data-grant="${key.id}" style="padding:12px 16px;border-top:1px solid var(--line)"><div class="field"><label>Project you own</label><select name="project">${raw(options)}</select></div><div class="field narrow"><label>Branch</label><select name="branch" class="mono">${raw(branchOptions(projects[0].uid))}</select></div><button class="btn" type="submit">${icon("plus")}Grant read</button></form>`) : ""}
        </div></div>`).join("");
    frame("/app/query-keys", h`
      <div class="page-head"><div><h1>Query keys</h1><div class="sub">What your applications send. Read only, on the projects and branches they are granted. <code>*</code> means every branch.</div></div></div>
      ${raw(reveal ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>${icon("key-round")}${reveal.name}</h2>${raw(secretBlock(reveal.secret, "Put it in your app's configuration as the Bearer token for /sql and /explore."))}</div></div>` : "")}
      <div class="card" style="margin-bottom:20px"><div class="card-body"><form id="new-key" class="inline"><div class="field"><label>New query key</label><input name="name" required placeholder="web app, prod"></div><button class="btn primary" type="submit">${icon("plus")}Create</button></form></div></div>
      <div class="grid">${raw(cards || h`<div class="card"><div class="empty">${icon("key-round", "xl")}<div>No query keys yet. Your applications send one as the Bearer token.</div></div></div>`)}</div>`);
    const nk = document.getElementById("new-key");
    nk.onsubmit = async (e) => { e.preventDefault(); try { const r = await api("POST", "/account/query-keys", Object.fromEntries(new FormData(nk).entries())); viewQueryKeys({ name: r.key.name, secret: r.secret }); } catch (err) { toast(err.message); } };
    root.querySelectorAll("[data-grant]").forEach((f) => {
      f.project.onchange = () => { f.branch.innerHTML = branchOptions(f.project.value); };
      f.onsubmit = async (e) => { e.preventDefault(); const d = Object.fromEntries(new FormData(f).entries()); if (!d.branch) delete d.branch; try { await api("POST", `/account/query-keys/${f.dataset.grant}/grants`, d); viewQueryKeys(); } catch (err) { toast(err.message); } };
    });
    root.querySelectorAll("[data-revoke-grant]").forEach((b) => b.onclick = async () => { try { await api("DELETE", `/account/query-keys/${b.dataset.revokeGrant}/grants/${encodeURIComponent(b.dataset.project)}`); viewQueryKeys(); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-rotate]").forEach((b) => b.onclick = async () => { if (!confirm("Rotate this key? The current secret stops working at once.")) return; try { const r = await api("POST", `/account/query-keys/${b.dataset.rotate}/rotate`); viewQueryKeys({ name: r.key.name + " (rotated)", secret: r.secret }); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-revoke-key]").forEach((b) => b.onclick = async () => { if (!confirm("Revoke this key? Apps using it stop working.")) return; try { await api("DELETE", `/account/query-keys/${b.dataset.revokeKey}`); viewQueryKeys(); } catch (err) { toast(err.message); } });
  }

  // ------------------------------------------------------------- my keys
  async function viewKeys(reveal) {
    const d = await api("GET", "/account/keys");
    const rows = d.keys.map((k) => h`<tr><td>${k.name}</td><td class="mono">${k.prefix}…</td><td class="muted small">${when(k.created_at)}</td><td class="muted small">${k.last_used_at ? when(k.last_used_at) : "never"}</td><td class="actions"><button class="btn sm quiet danger" data-revoke="${k.id}">${icon("trash-2")}Revoke</button></td></tr>`).join("");
    frame("/app/keys", h`
      <div class="page-head"><div><h1>My keys</h1><div class="sub">Personal keys do exactly what you can do. One per machine; store it with <code>zsql auth --api-key …</code>.</div></div></div>
      ${raw(reveal ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>${icon("key")}${reveal.name}</h2>${raw(secretBlock(reveal.secret, "Run `zsql auth --api-key <key>` inside a project to store it."))}</div></div>` : "")}
      <div class="card" style="margin-bottom:20px"><div class="card-body"><form id="new-key" class="inline"><div class="field"><label>New key</label><input name="name" required placeholder="laptop"></div><button class="btn primary" type="submit">${icon("plus")}Create</button></form></div></div>
      <div class="card"><div class="card-body flush"><table><thead><tr><th>name</th><th>prefix</th><th>created</th><th>last used</th><th></th></tr></thead><tbody>${raw(rows)}</tbody></table></div></div>
      <div class="card" style="margin-top:20px"><div class="card-head"><h2>${icon("lock")}Password</h2></div><div class="card-body"><form id="password" class="inline"><div class="field"><label>New password</label><input name="password" type="password" minlength="8" required></div><button class="btn" type="submit">Change</button></form></div></div>`);
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
      <td class="actions">${admin ? raw(h`<select data-role="${m.user.email}" style="width:auto"><option ${m.role === "developer" ? "selected" : ""}>developer</option><option ${m.role === "admin" ? "selected" : ""}>admin</option></select><button class="btn sm" data-invite="${m.user.email}">${icon("mail")}New invite</button><button class="btn sm quiet danger" data-remove="${m.user.email}">${icon("trash-2")}Remove</button>`) : ""}</td></tr>`).join("");
    frame("/app/members", h`
      <div class="page-head"><div><h1>Members</h1><div class="sub">Developers read every project and own the ones they create. Admins own everything and manage the account.</div></div></div>
      ${raw(invite ? h`<div class="card" style="margin-bottom:20px"><div class="card-body"><h2>${icon("send")}Invite for ${invite.email}</h2>${raw(secretBlock(inviteLink(invite.token), "Send this to " + invite.email + ". It works once, and sets them up with a password and a personal key for the CLI.", "Copy link"))}</div></div>` : "")}
      ${admin ? raw(h`<div class="card" style="margin-bottom:20px"><div class="card-body"><form id="add" class="inline"><div class="field"><label>Email</label><input name="email" type="email" required placeholder="bob@acme.com"></div><div class="field"><label>Name</label><input name="name" placeholder="Bob"></div><div class="field narrow"><label>Role</label><select name="role"><option>developer</option><option>admin</option></select></div><button class="btn primary" type="submit">${icon("user-plus")}Add member</button></form></div></div>`) : ""}
      <div class="card"><div class="card-body flush"><table><thead><tr><th>user</th><th>role</th><th></th></tr></thead><tbody>${raw(rows)}</tbody></table></div></div>`);
    if (!admin) return;
    const add = document.getElementById("add");
    add.onsubmit = async (e) => { e.preventDefault(); const d = Object.fromEntries(new FormData(add).entries()); try { const r = await api("POST", "/account/members", d); viewMembers(r.invite ? { email: r.user.email, token: r.invite } : null); if (!r.invite) toast("role updated"); } catch (err) { toast(err.message); } };
    root.querySelectorAll("[data-role]").forEach((sel) => sel.onchange = async () => { try { await api("POST", "/account/members", { email: sel.dataset.role, role: sel.value }); toast("updated"); } catch (err) { toast(err.message); viewMembers(); } });
    root.querySelectorAll("[data-invite]").forEach((b) => b.onclick = async () => { try { const r = await api("POST", `/account/members/${encodeURIComponent(b.dataset.invite)}/invite`); viewMembers({ email: b.dataset.invite, token: r.invite }); } catch (err) { toast(err.message); } });
    root.querySelectorAll("[data-remove]").forEach((b) => b.onclick = async () => { if (!confirm(`Remove ${b.dataset.remove}? Their keys stop working.`)) return; try { await api("DELETE", `/account/members/${encodeURIComponent(b.dataset.remove)}`); viewMembers(); } catch (err) { toast(err.message); } });
  }

  // ----------------------------------------------------------- playground
  const CHEATSHEET = [
    ["projection", "item category · month(date) · store net paid desc · yoy(revenue) · pct(revenue) · running_sum(revenue)"],
    ["filter", "category = Books · net paid > 100 · date between 2024-01-01 and 2024-12-31 · category in (Books, Music) · name contains foo · category top 10 by revenue"],
    ["calculation", "Share = sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m) · max([gross sales]@m) as Peak"],
    ["fields", "names or uids, case ignored; [Name] or \"Name\" is always a field; @d / @m pick a dimension or measure"],
  ];

  async function viewPlayground(uid) {
    const params = new URLSearchParams(location.search);
    let list;
    try { list = await api("GET", "/projects"); } catch (e) { return frame("/app/", alertBox(e.message)); }
    const project = (list.projects || []).find((x) => x.uid === uid) || { uid, name: uid, production_branch: "main" };
    const branches = list.deployments.filter((x) => x.project === uid).map((x) => x.branch).sort();
    if (!branches.length) return frame("/app/", alertBox(`${uid} has no deployed branch to play on.`));
    let branch = params.get("branch");
    if (!branches.includes(branch)) branch = branches.includes(project.production_branch) ? project.production_branch : branches[0];
    const histKey = `zsql-play-${uid}`;
    let recent = [];
    try { recent = JSON.parse(localStorage.getItem(histKey) || "[]"); } catch (_) {}
    let mode = "expr";
    let lastQuery = recent[0] || "";

    frame("/app/", h`
      <div class="page-head"><div><div class="crumbs"><a href="/app/" data-link>Projects</a>${icon("chevron-right")}<a href="/app/projects/${uid}" data-link>${uid}</a>${icon("chevron-right")}<span>playground</span></div><h1>${project.name || uid}</h1></div>
        <div class="actions"><label style="margin:0;display:inline-flex;align-items:center;gap:6px" for="branch">${icon("git-branch")}branch</label><select id="branch" class="mono" style="width:auto">${raw(branches.map((b) => h`<option ${b === branch ? "selected" : ""}>${b}</option>`).join(""))}</select></div></div>
      <div class="play">
        <div class="play-main">
          <div class="card">
            <div class="card-head"><div class="tabs"><button class="tab active" data-mode="expr">${icon("terminal")}Expression</button><button class="tab" data-mode="spec">${icon("braces")}Spec JSON</button></div>
              <button class="btn sm quiet" id="help-toggle">${icon("book-open")}cheat sheet</button></div>
            <div class="card-body">
              <div id="help" class="cheat" hidden>${raw(CHEATSHEET.map(([k, v]) => h`<div><b>${k}</b><span>${v}</span></div>`).join(""))}</div>
              <textarea id="query" class="mono" rows="3" spellcheck="false" placeholder="department, sales value, department = PRODUCE">${lastQuery}</textarea>
              <details id="ctx-details" style="margin-top:8px"><summary class="small muted">${icon("shield", "sm")} Security context (JSON, for branches with policies)</summary>
                <textarea id="context" class="mono" rows="3" spellcheck="false" placeholder='{"email": "ann@acme.com", "groups": [{"name": "EMEA", "tags": ["region:emea"]}]}'></textarea></details>
              <div class="actions" style="margin-top:10px">
                <button class="btn primary" id="run-sql">${icon("play")}SQL <span class="kbd">⌘↵</span></button>
                <button class="btn" id="run-explain">${icon("list-tree")}Explain</button>
                <span class="inline-explore"><button class="btn" id="run-explore">${icon("compass")}Explore</button><span class="with-icon">${icon("search", "sm")}<input id="term" placeholder="filter term" style="width:150px"></span></span>
                <span class="spacer"></span><span id="status" class="muted small"></span>
              </div>
              ${recent.length ? raw(h`<div class="chips" id="recent">${icon("history", "sm")}${raw(recent.slice(0, 8).map((q) => h`<button class="chip" data-q="${q}" title="${q}">${q.length > 48 ? q.slice(0, 48) + "…" : q}</button>`).join(""))}</div>`) : ""}
            </div>
          </div>
          <div id="out"></div>
        </div>
        <div class="play-side">
          <div class="card"><div class="card-head"><h2>${icon("hash")}Fields</h2><span class="muted small">click to add</span></div>
            <div class="card-body"><div class="with-icon">${icon("search", "sm")}<input id="field-q" placeholder="search fields"></div><div id="field-list" class="field-list"></div></div></div>
        </div>
      </div>`, true);

    const $ = (id) => document.getElementById(id);
    const out = $("out"), query = $("query"), status = $("status");
    const branchOf = () => $("branch").value;
    $("branch").onchange = () => { history.replaceState(null, "", `/app/projects/${uid}/play?branch=${encodeURIComponent(branchOf())}`); loadFields(); };
    $("help-toggle").onclick = () => { $("help").hidden = !$("help").hidden; };
    root.querySelectorAll(".tab").forEach((t) => t.onclick = () => {
      mode = t.dataset.mode;
      root.querySelectorAll(".tab").forEach((x) => x.classList.toggle("active", x === t));
      query.rows = mode === "spec" ? 10 : 3;
      query.placeholder = mode === "spec" ? '{"projections": [{"field": "department"}, {"field": "sales value"}], "filters": [{"field": "department", "predicate": "equals", "value": "PRODUCE"}]}' : "department, sales value, department = PRODUCE";
    });
    root.querySelectorAll(".chip").forEach((c) => c.onclick = () => { query.value = c.dataset.q; mode === "spec" && c.dataset.q.trim().startsWith("{") || (mode = "expr"); run("sql"); });

    function remember(q) {
      recent = [q, ...recent.filter((x) => x !== q)].slice(0, 20);
      try { localStorage.setItem(histKey, JSON.stringify(recent)); } catch (_) {}
    }

    function body() {
      const q = query.value.trim();
      if (!q) throw new Error("type an expression or a spec first");
      const b = {};
      if (mode === "spec" || q.startsWith("{")) { try { b.spec = JSON.parse(q); } catch (e) { throw new Error("spec is not valid JSON: " + e.message); } }
      else b.expr = q;
      const ctx = $("context").value.trim();
      if (ctx) { try { b.context = JSON.parse(ctx); } catch (e) { throw new Error("context is not valid JSON: " + e.message); } }
      return b;
    }

    const sqlBlock = (r) => h`
      <div class="card"><div class="card-head"><h2>${icon("terminal")}SQL</h2><div class="actions"><span class="muted small stat">${icon("database", "sm")}${r.datasource} · ${r.adapter}</span><button class="btn sm" data-copy="${r.sql}">${icon("copy")}Copy</button></div></div>
        <div class="card-body">
          ${raw((r.corrections || []).map((c) => h`<div class="note">${c.term} → <b>${c.field_name}</b> <span class="faint">~${c.score}</span></div>`).join(""))}
          ${r.items && r.items.length ? raw(h`<div class="items">${raw(r.items.map((i) => h`<code>${i}</code>`).join(""))}</div>`) : ""}
          <pre class="sql">${r.sql}</pre>
          ${r.spec ? raw(h`<details class="small"><summary class="muted">spec this made</summary><pre class="sql">${JSON.stringify(r.spec, null, 2)}</pre></details>`) : ""}
        </div></div>`;

    function explainBlock(r) {
      const byId = {}; for (const n of r.nodes) byId[n.id] = n;
      const lines = [];
      const walk = (n, depth, via) => {
        const head = [via + n.kind, n.alias, n.table ? "on " + n.table + (n.datasource ? " (" + n.datasource + ")" : "") : "", n.join_type ? n.join_type + " join" : "", n.group_by ? "group by" : "", n.segment ? "[segment]" : "", n.transform ? "[" + n.transform + "]" : "", n.purpose ? "[" + n.purpose + "]" : ""].filter(Boolean).join(" ");
        lines.push(h`<div class="tree-line" style="padding-left:${depth * 18}px"><b>${head}</b></div>`);
        const sub = (label, arr) => { if (arr && arr.length) lines.push(h`<div class="tree-sub" style="padding-left:${depth * 18 + 14}px">${label}: ${arr.join(", ")}</div>`); };
        sub("paths", n.paths); sub("projections", n.projections); sub("filters", n.filters); sub("security", n.security_filters); sub("masks", n.security_masks);
        for (const st of n.strategies || []) if (byId[st.node]) walk(byId[st.node], depth + 1, st.kind + " ← ");
        for (const id of n.inputs || []) if (byId[id]) walk(byId[id], depth + 1, "");
      };
      const rootNode = r.nodes.find((n) => n.root);
      if (rootNode) walk(rootNode, 0, "");
      return sqlBlock(r) + h`
        <div class="card"><div class="card-head"><h2>${icon("list-tree")}Plan</h2><span class="muted small stat">${icon("clock", "sm")}${r.nodes.length} nodes · server ${r.timings.total_us} µs (parser ${r.timings.parser_us}, plan ${r.timings.plan_us})</span></div>
          <div class="card-body"><div class="tree">${raw(lines.join(""))}</div>
            <div class="phases">${raw(r.phases.map((p) => h`<span><b>${p.name}</b> ${p.us}</span>`).join(""))}</div></div></div>`;
    }

    const exploreBlock = (r) => {
      const rows = (arr, kind) => arr.map((f) => h`<button class="row row-btn" data-add="${f.name}"><span class="grow"><span class="title">${f.name}</span> <span class="faint mono">${f.uid}</span>${f.score !== undefined && f.score < 1 ? h` <span class="faint">~${f.score}</span>` : ""}</span><span class="meta">${(f.tables || []).join(", ")}</span>${kindBadge(kind)}</button>`).join("");
      return h`<div class="card"><div class="card-head"><h2>${icon("compass")}Can add</h2><span class="muted small">${r.dimensions.length} dimensions · ${r.measures.length} measures · server ${r.total_us} µs</span></div>
        <div class="card-body flush rows">${raw(rows(r.dimensions, "dim") + rows(r.measures, "msr") || h`<div class="empty">${icon("inbox", "xl")}<div>Nothing can be added to this query.</div></div>`)}</div></div>`;
    };

    async function run(kind) {
      let b;
      try { b = body(); } catch (e) { out.innerHTML = alertBox(e.message); return; }
      status.textContent = "…";
      const path = `/projects/${encodeURIComponent(uid)}/branches/${encodeURIComponent(branchOf())}/${kind}`;
      if (kind === "explore" && $("term").value.trim()) b.q = $("term").value.trim();
      const t0 = performance.now();
      try {
        const r = await api("POST", path, b);
        const ms = (performance.now() - t0).toFixed(1);
        status.textContent = `${ms} ms round trip`;
        remember(query.value.trim());
        out.innerHTML = kind === "sql" ? sqlBlock(r) : kind === "explain" ? explainBlock(r) : exploreBlock(r);
        out.querySelectorAll("[data-add]").forEach((btn) => btn.onclick = () => { if (mode === "expr") { query.value = query.value.trim().replace(/,\s*$/, "") + (query.value.trim() ? ", " : "") + btn.dataset.add; run("sql"); } });
      } catch (e) {
        status.textContent = "";
        out.innerHTML = alertBox(e.message);
        if (e.retryAfter) holdOff(e.retryAfter);
      }
    }
    // Over a limit: hold the run buttons until it is worth asking
    // again, rather than letting a repeated click spend the next
    // allowance the moment it arrives.
    let holdTimer;
    function holdOff(secs) {
      const buttons = ["run-sql", "run-explain", "run-explore"].map($);
      let left = secs;
      clearInterval(holdTimer);
      const tick = () => {
        buttons.forEach((b) => (b.disabled = left > 0));
        status.textContent = left > 0 ? `at the rate limit · ${left}s` : "";
        if (left-- <= 0) clearInterval(holdTimer);
      };
      tick();
      holdTimer = setInterval(tick, 1000);
    }

    $("run-sql").onclick = () => run("sql");
    $("run-explain").onclick = () => run("explain");
    $("run-explore").onclick = () => run("explore");
    query.addEventListener("keydown", (e) => { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); run("sql"); } });

    let fieldTimer;
    async function loadFields() {
      const q = $("field-q").value.trim();
      try {
        const r = await api("GET", `/projects/${encodeURIComponent(uid)}/branches/${encodeURIComponent(branchOf())}/fields?q=${encodeURIComponent(q)}`);
        const fields = r.fields.slice(0, 60);
        $("field-list").innerHTML = fields.map((f) => h`<button class="field-item" data-add="${f.name}">${kindBadge(f.kind === "dimension" ? "dim" : "msr")}<span class="grow">${f.name}</span>${f.score !== undefined && f.score < 1 ? h`<span class="faint">~${f.score}</span>` : ""}</button>`).join("") || h`<div class="empty small">no fields match</div>`;
        $("field-list").querySelectorAll("[data-add]").forEach((btn) => btn.onclick = () => {
          if (mode !== "expr") return;
          query.value = query.value.trim().replace(/,\s*$/, "") + (query.value.trim() ? ", " : "") + btn.dataset.add;
          query.focus();
        });
      } catch (e) { $("field-list").innerHTML = alertBox(e.message); }
    }
    $("field-q").oninput = () => { clearTimeout(fieldTimer); fieldTimer = setTimeout(loadFields, 250); };
    loadFields();
    if (lastQuery) run("sql");
  }

  // ---------------------------------------------------------------- audit
  async function viewAudit() {
    const d = await api("GET", "/account/audit?limit=200");
    const rows = d.events.map((e) => h`<tr><td class="muted small mono">${when(e.at)}</td><td>${e.actor}</td><td class="mono">${e.action}</td><td class="muted">${e.subject}</td></tr>`).join("");
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
    if (path === "/app/forgot") return viewForgot();
    if (path === "/app/reset") return viewReset();
    if (path === "/app/verify") return await viewVerify();
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
      const pm = path.match(/^\/app\/projects\/([^/]+)\/play$/);
      if (pm) return await viewPlayground(decodeURIComponent(pm[1]));
      go("/app/");
    } catch (e) {
      if (e.status === 401) { state.me = null; return viewLogin(); }
      frame(path, alertBox(e.message));
    }
  }
  route();
})();
