// Stepwise management UI. No build step, no dependencies, no external requests.
// Every piece of server-provided text (documents, logs, errors) is inserted with textContent via
// h(): there is no innerHTML anywhere, so hostile data cannot become markup.
(() => {
  "use strict";

  const STEPS = ["rate_limit", "authenticate", "authorize", "validate_request", "pre_process", "execute", "post_process", "validate_response"];
  const SLOTS = ["before", "replace", "after"];
  const VIEWS = { overview: "Overview", routes: "Routes", scripts: "Scripts", data: "Data", config: "Config" };

  const $ = (s, r = document) => r.querySelector(s);
  const state = { token: sessionStorage.getItem("stepwise-ui-token") || "" };

  // ---------------------------------------------------------------- DOM helper
  function h(tag, props = {}, ...kids) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(props || {})) {
      if (v == null || v === false) continue;
      if (k === "class") el.className = v;
      else if (k === "text") el.textContent = v;
      else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
      else if (k === "value") el.value = v;
      else el.setAttribute(k, v === true ? "" : v);
    }
    for (const kid of kids.flat(Infinity)) {
      if (kid == null || kid === false) continue;
      el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
    }
    return el;
  }
  const pre = (v, testid) => h("pre", { "data-testid": testid }, typeof v === "string" ? v : JSON.stringify(v, null, 2));
  const badge = (text, kind = "") => h("span", { class: "badge " + kind }, text);
  const short = (d) => (d ? String(d).replace("sha256:", "").slice(0, 10) : "");
  const when = (s) => (s ? new Date(s * 1000).toISOString().replace("T", " ").slice(0, 19) : "");

  // ---------------------------------------------------------------- API
  class ApiError extends Error {}
  async function api(method, path, body, opts = {}) {
    const headers = { authorization: "Bearer " + state.token };
    let payload;
    if (opts.raw !== undefined) { payload = opts.raw; headers["content-type"] = opts.contentType; }
    else if (body !== undefined) { payload = JSON.stringify(body); headers["content-type"] = "application/json"; }
    if (opts.ifMatch) headers["if-match"] = opts.ifMatch;
    const res = await fetch("/api/v1" + path, { method, headers, body: payload });
    const text = await res.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch { data = text; }
    if (!res.ok) {
      const e = new ApiError((data && data.error && data.error.message) || res.statusText || "request failed");
      Object.assign(e, { status: res.status, code: data && data.error && data.error.code, details: data && data.error && data.error.details });
      throw e;
    }
    return { data, headers: res.headers, status: res.status };
  }

  // ---------------------------------------------------------------- notices
  function notify(kind, message) {
    const box = $("#notices");
    const n = h("div", { class: "notice " + kind, "data-kind": kind, role: kind === "error" ? "alert" : "status" },
      h("span", { text: message }), h("button", { class: "link", type: "button", onclick: () => n.remove() }, "dismiss"));
    box.append(n);
    if (kind !== "error") setTimeout(() => n.remove(), 6000);
  }
  const fail = (e) => { if (e && e.status === 401) { signOut(); return; } notify("error", e && e.message ? e.message : String(e)); };

  /** Disable a button while an async action runs; surface errors as notices. */
  function act(button, fn) {
    return async () => {
      button.disabled = true;
      try { await fn(); } catch (e) { fail(e); } finally { button.disabled = false; }
    };
  }
  const button = (label, onclick, cls = "", testid) => {
    const b = h("button", { type: "button", class: cls, "data-testid": testid }, label);
    b.addEventListener("click", act(b, onclick));
    return b;
  };

  // ---------------------------------------------------------------- shell / auth / routing
  function showApp(on) {
    $("#login").hidden = on;
    $("#top").hidden = !on;
  }
  function signOut() {
    sessionStorage.removeItem("stepwise-ui-token");
    state.token = "";
    $("#view").replaceChildren();
    showApp(false);
  }
  async function signIn(token) {
    state.token = token;
    try {
      await api("GET", "/status");
    } catch (e) {
      state.token = "";
      throw e;
    }
    sessionStorage.setItem("stepwise-ui-token", token);
    showApp(true);
    route();
  }

  function buildNav() {
    $("#nav").replaceChildren(...Object.entries(VIEWS).map(([id, label]) => h("a", { href: "#/" + id, "data-view": id }, label)));
  }

  const views = {};
  async function route() {
    if (!state.token) return;
    const id = (location.hash.match(/^#\/(\w+)/) || [])[1];
    const view = views[id] ? id : "overview";
    for (const a of document.querySelectorAll("#nav a")) a.classList.toggle("active", a.dataset.view === view);
    const root = $("#view");
    root.dataset.view = view;
    root.replaceChildren(h("p", { class: "muted" }, "Loading…"));
    try {
      const content = await views[view]();
      if (root.dataset.view === view) root.replaceChildren(content);
    } catch (e) {
      if (e.status === 401) return signOut();
      root.replaceChildren(h("p", { class: "error", "data-testid": "view-error" }, "Could not load this page: " + e.message));
    }
  }

  // ---------------------------------------------------------------- overview
  views.overview = async () => {
    const [{ data: st }, ready, builder] = await Promise.all([
      api("GET", "/status"),
      api("GET", "/ready").then((r) => r.data).catch((e) => ({ status: "error: " + e.message })),
      api("GET", "/builder").then((r) => r.data).catch(() => ({ enabled: false })),
    ]);
    const l = (x) => (x ? `${x.addr}${x.tls ? " · TLS" : ""}${x.mtls ? " · mTLS" : ""}` : "not listening");
    const kv = (pairs) => h("dl", {}, pairs.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
    return h("div", {},
      h("h2", {}, st.app_name || "Stepwise"),
      h("div", { class: "grid" },
        h("div", { class: "panel" }, h("h3", {}, "Server"), kv([
          ["Status", badge(ready.status, ready.status === "ready" ? "ok" : "bad")],
          ["Config version", String(st.config_version)], ["Uptime", st.uptime_seconds + " s"],
          ["API spec", `${st.spec.title} ${st.spec.version}`], ["Routes", String(st.routes)],
        ])),
        h("div", { class: "panel" }, h("h3", {}, "Listeners"), kv([["Data plane", l(st.listeners.data_plane)], ["Control plane", l(st.listeners.control_plane)]])),
        h("div", { class: "panel" }, h("h3", {}, "Features"), kv([
          ["Database", st.database.enabled ? badge(`${st.database.type} · ${ready.database}`, ready.database === "ok" ? "ok" : "bad") : badge("off")],
          ["WASM hooks", st.wasm.enabled ? badge("on", "ok") : badge("off")],
          ["Builder", !builder.enabled ? badge("not configured") : builder.reachable ? badge((builder.builder.languages || []).join(", ") || "reachable", "ok") : badge("unreachable", "bad")],
        ]))),
      st.pending_process_restart.length
        ? h("div", { class: "panel", "data-testid": "pending-restart" }, h("h3", {}, "Needs a process restart"), h("ul", {}, st.pending_process_restart.map((p) => h("li", {}, h("code", {}, p)))))
        : null);
  };

  // ---------------------------------------------------------------- routes
  views.routes = async () => {
    const [{ data: rs }, cfgRes, { data: sc }, { data: hk }] = await Promise.all([
      api("GET", "/routes"), api("GET", "/config"),
      api("GET", "/scripts").catch(() => ({ data: { scripts: [] } })), api("GET", "/hooks"),
    ]);
    const cfg = cfgRes.data.config;
    const etag = cfgRes.headers.get("etag");
    let selected = null;
    const detail = h("div", { "data-testid": "route-detail" });
    const refs = [...hk.hooks, ...sc.scripts.flatMap((s) => [`wasm:${s.name}@live`, ...Array.from({ length: s.versions }, (_, i) => `wasm:${s.name}@${i + 1}`)])];

    const table = h("table", { "data-testid": "routes-table" },
      h("thead", {}, h("tr", {}, ["Route", "Auth", "Database", "Rate limit", "Status", "Hooks"].map((c) => h("th", {}, c)))),
      h("tbody", {}, rs.routes.map((r) => {
        const n = Object.values(r.steps).reduce((a, s) => a + s.before.length + s.after.length + (s.replace ? 1 : 0), 0);
        return h("tr", { class: "clickable", "data-route": r.id, onclick: () => select(r) },
          h("td", {}, h("code", {}, r.id)), h("td", {}, r.protected ? badge(r.required_roles.length ? "roles" : "yes", "warn") : ""),
          h("td", {}, r.crud ? badge(`${r.crud.operation} ${r.crud.collection}`) : h("span", { class: "muted" }, "mock"), (r.data_rules || []).length ? h("span", { class: "muted", title: "no-code data rules" }, " + " + r.data_rules.join(", ")) : null),
          h("td", {}, r.rate_limit ? `${r.rate_limit.requests}/${r.rate_limit.per_seconds}s` : ""), h("td", {}, String(r.response_status)), h("td", {}, n ? String(n) : ""));
      })));

    function select(r) {
      selected = r;
      for (const tr of table.querySelectorAll("tr.clickable")) tr.classList.toggle("selected", tr.dataset.route === r.id);
      const own = (cfg.routes[r.id] && cfg.routes[r.id].steps) || {};
      const draft = JSON.parse(JSON.stringify(own));
      renderDetail(r, draft);
    }

    function renderDetail(r, draft) {
      const report = h("div", { "data-testid": "route-report" });
      const hookName = (x) => h("span", {}, h("code", {}, x));
      const pinnedFor = (step, slot, ref) => {
        const bound = r.steps[step] && (slot === "replace" ? [r.steps[step].replace] : r.steps[step][slot]).filter(Boolean).find((b) => b.ref === ref);
        return bound && bound.pinned ? h("span", { class: "muted mono" }, ` ${short(bound.pinned)}`) : null;
      };
      const stepRows = STEPS.map((step) => {
        const d = draft[step] || {};
        const cells = SLOTS.map((slot) => {
          const list = slot === "replace" ? (d.replace ? [d.replace] : []) : d[slot] || [];
          return h("td", {}, list.map((ref) => h("div", {}, hookName(ref), pinnedFor(step, slot, ref),
            h("button", { class: "link", type: "button", "data-testid": `remove-${step}-${slot}`, onclick: () => {
              if (slot === "replace") delete d.replace; else d[slot] = d[slot].filter((x) => x !== ref);
              draft[step] = d; renderDetail(r, draft);
            } }, "remove"))));
        });
        return h("tr", {}, h("td", {}, h("code", {}, step)), cells);
      });

      const step = h("select", { "data-testid": "add-step" }, STEPS.map((s) => h("option", { value: s, selected: s === "pre_process" }, s)));
      const slot = h("select", { "data-testid": "add-slot" }, SLOTS.map((s) => h("option", { value: s }, s)));
      const ref = h("input", { list: "hook-refs", placeholder: "wasm:name@3  or  builtin:stamp", size: 34, "data-testid": "add-ref" });
      const dl = h("datalist", { id: "hook-refs" }, refs.map((x) => h("option", { value: x })));
      const fullDoc = () => {
        const next = JSON.parse(JSON.stringify(cfg));
        next.routes[r.id] = Object.assign({}, next.routes[r.id] || {}, { steps: draft });
        return next;
      };
      const add = button("Add hook", async () => {
        const v = ref.value.trim();
        if (!v) throw new Error("enter a hook reference");
        const d = draft[step.value] || {};
        if (slot.value === "replace") d.replace = v; else (d[slot.value] = d[slot.value] || []).push(v);
        draft[step.value] = d; renderDetail(r, draft);
      }, "", "add-hook");
      const showReport = (rep, applied) => report.replaceChildren(
        h("p", {}, applied ? badge("applied", "ok") : badge("preview"), ` ${rep.changes.length} change(s)`, rep.restarts.length ? ` · restarts: ${rep.restarts.join(", ")}` : ""),
        rep.changes.length ? h("table", {}, rep.changes.map((c) => h("tr", {}, h("td", {}, h("code", {}, c.path)), h("td", {}, c.action)))) : h("p", { class: "muted" }, "No changes."),
        rep.warnings.map((w) => h("p", { class: "muted" }, w)));
      const preview = button("Preview", async () => { showReport((await api("PUT", "/config?dry_run=true", fullDoc())).data, false); }, "", "preview-routes");
      const apply = button("Apply", async () => {
        const res = await api("PUT", "/config", fullDoc(), { ifMatch: etag });
        notify("ok", `Applied (config version ${res.data.version})`);
        await route();
      }, "primary", "apply-routes");

      detail.replaceChildren(h("div", { class: "panel" },
        h("h2", {}, h("code", {}, r.id)),
        h("p", { class: "muted" }, `${r.crud ? `database: ${r.crud.operation} on "${r.crud.collection}"` : "mock response"} · responds ${r.response_status}${r.operation_id ? " · " + r.operation_id : ""}`),
        h("table", {}, h("thead", {}, h("tr", {}, ["Step", "before", "replace", "after"].map((c) => h("th", {}, c)))), h("tbody", {}, stepRows)),
        h("h3", {}, "Add a hook"),
        h("div", { class: "row" }, step, slot, ref, dl, add),
        h("div", { class: "row" }, preview, apply), report));
    }
    return h("div", {}, h("h2", {}, "Routes"), h("div", { class: "panel" }, table), detail);
  };

  // ---------------------------------------------------------------- scripts
  views.scripts = async () => {
    const root = h("div", {});
    let current = (location.hash.match(/script=([\w-]+)/) || [])[1] || null;
    let lastBuild = null; // survives the redraw that follows a save, so the result stays visible

    async function draw() {
      const { data: list } = await api("GET", "/scripts");
      const names = list.scripts.map((s) => s.name);
      if (current && !names.includes(current)) current = null;
      const detail = current ? (await api("GET", `/scripts/${current}`)).data : null;
      let languages = ["typescript", "javascript", "python"];
      try { const b = (await api("GET", "/builder")).data; if (b.enabled && b.reachable) languages = b.builder.languages; else if (!b.enabled) languages = []; } catch { /* keep defaults */ }

      const sidebar = h("div", { class: "panel" }, h("h3", {}, "Scripts"),
        h("div", { class: "list", "data-testid": "script-list" }, list.scripts.length
          ? list.scripts.map((s) => h("button", { type: "button", class: s.name === current ? "active" : "", "data-script": s.name, onclick: () => { current = s.name; draw().catch(fail); } },
              `${s.name}  ·  v${s.latest_version}`))
          : h("span", { class: "muted" }, "None yet. Create one with the editor.")));

      const nameInput = h("input", { id: "script-name", "data-testid": "script-name", placeholder: "script-name", value: current || "", size: 22 });
      const lang = h("select", { "data-testid": "language" }, (languages.length ? languages : ["typescript", "javascript", "python"]).map((l) => h("option", { value: l }, l)));
      const backend = h("select", { "data-testid": "backend", title: "JavaScript/TypeScript only" }, [["", "qjs (default)"], ["spidermonkey", "spidermonkey"]].map(([v, t]) => h("option", { value: v }, t)));
      const note = h("input", { placeholder: "note (optional)", size: 24, "data-testid": "note" });
      const source = h("textarea", { class: "code", "data-testid": "source", spellcheck: "false", placeholder: "Write a hook, or load a template…" });
      const result = h("div", { "data-testid": "build-result" });

      const showBuild = (d, title) => result.replaceChildren(h("div", { class: "panel" },
        h("p", {}, badge(title, "ok"), " ", d.reference ? h("code", {}, d.reference) : "", d.deduplicated ? " (identical source already built)" : ""),
        h("dl", {}, [["Size", d.size ? `${(d.size / 1048576).toFixed(2)} MB` : ""], ["Digest", d.digest || ""], ["Toolchain", d.build ? d.build.toolchain : ""], ["Build time", d.build ? `${d.build.duration_ms} ms` : ""]]
          .filter(([, v]) => v).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
        d.build && d.build.log ? h("details", {}, h("summary", {}, "Compiler output"), pre(d.build.log)) : null));
      const showBuildError = (e) => result.replaceChildren(h("div", { class: "panel" },
        h("p", { class: "error", "data-testid": "build-error" }, `${e.code || "error"}: ${e.message}`),
        e.details && e.details.log ? pre(e.details.log, "build-log") : null));

      const body = () => {
        const b = { language: lang.value, source: source.value, note: note.value };
        if (backend.value && lang.value !== "python") b.backend = backend.value;
        return b;
      };
      const needName = () => { const n = nameInput.value.trim(); if (!n) throw new Error("give the script a name"); return n; };
      const build = (dry) => async () => {
        const n = needName();
        try {
          const r = await api("POST", `/scripts/${n}/source${dry ? "?dry_run=true" : ""}`, body());
          if (dry) showBuild(r.data, "builds OK (not saved)");
          else { current = n; lastBuild = [r.data, r.data.deduplicated ? "unchanged" : "saved"]; await draw(); }
        } catch (e) { if (e.status === 401) throw e; showBuildError(e); }
      };
      const template = button("Load template", async () => { source.value = (await api("GET", `/builder/templates/${lang.value}`)).data.source; }, "", "load-template");
      const fileInput = h("input", { type: "file", accept: ".wasm,application/wasm", "data-testid": "wasm-file" });
      const upload = button("Upload .wasm", async () => {
        const n = needName(); const f = fileInput.files[0];
        if (!f) throw new Error("choose a compiled .wasm component first");
        const r = await api("POST", `/scripts/${n}/versions?language=${encodeURIComponent(lang.value)}`, undefined, { raw: await f.arrayBuffer(), contentType: "application/wasm" });
        current = n; lastBuild = [r.data, r.data.created ? "uploaded" : "already uploaded"]; await draw();
      }, "", "upload-wasm");

      if (lastBuild) { showBuild(...lastBuild); lastBuild = null; }
      const editor = h("div", { class: "panel" }, h("h3", {}, "Editor"),
        h("div", { class: "row" }, nameInput, lang, backend, note),
        source,
        h("div", { class: "row" }, template, button("Check (dry run)", build(true), "", "check-source"), button("Build & save as new version", build(false), "primary", "build-source")),
        h("div", { class: "row" }, h("span", { class: "muted" }, "or a prebuilt component:"), fileInput, upload),
        result);

      // ---- versions + aliases
      let versionsPanel = null;
      let testPanel = null;
      if (detail) {
        const s = detail.script;
        const aliasName = h("input", { value: "live", size: 8, "data-testid": "alias-name" });
        const rows = s.versions.slice().reverse().map((v) => {
          const aliases = Object.entries(s.aliases).filter(([, n]) => n === v.version).map(([a]) => badge(a, "ok"));
          const promote = button("promote", async () => {
            const r = await api("PUT", `/scripts/${current}/aliases/${aliasName.value.trim()}`, { version: v.version });
            notify("ok", `${aliasName.value.trim()} → v${v.version}${r.data.applied ? " (routes updated)" : ""}`); await draw();
          }, "link", `promote-${v.version}`);
          const viewSrc = v.source ? button("view source", async () => {
            const r = (await api("GET", `/scripts/${current}/versions/${v.version}/source`)).data;
            source.value = r.source; lang.value = r.language; nameInput.value = current;
            if (r.backend) backend.value = r.backend;
          }, "link", `source-${v.version}`) : h("span", { class: "muted" }, "no source");
          const del = button("delete", async () => {
            await api("DELETE", `/scripts/${current}/versions/${v.version}`); notify("ok", `deleted v${v.version}`); await draw();
          }, "link", `delete-${v.version}`);
          return h("tr", { "data-version": v.version }, h("td", {}, `v${v.version}`), h("td", {}, aliases), h("td", {}, v.language), h("td", { class: "mono" }, short(v.digest)),
            h("td", {}, `${(v.size / 1048576).toFixed(1)} MB`), h("td", {}, when(v.created_at)), h("td", {}, v.note || ""), h("td", {}, viewSrc, promote, del));
        });
        const used = detail.used_by.length
          ? h("p", { class: "muted" }, "Used by: ", detail.used_by.map((u) => h("code", {}, `${u.route} ${u.step}/${u.slot} (${u.ref})  `)))
          : h("p", { class: "muted" }, "Not bound to any route.");
        versionsPanel = h("div", { class: "panel" }, h("h3", {}, `${current}  ·  ${s.id}`),
          h("table", { "data-testid": "versions" }, h("thead", {}, h("tr", {}, ["Version", "Aliases", "Language", "Digest", "Size", "Created", "Note", ""].map((c) => h("th", {}, c)))), h("tbody", {}, rows)),
          h("div", { class: "row" }, h("span", { class: "muted" }, "alias"), aliasName), used);

        // ---- test panel
        const tv = h("select", { "data-testid": "test-version" }, s.versions.slice().reverse().map((v) => h("option", { value: v.version }, `v${v.version}`)));
        const tstep = h("select", { "data-testid": "test-step" }, STEPS.map((x) => h("option", { value: x, selected: x === "pre_process" }, x)));
        const tdb = h("select", { "data-testid": "test-db" }, [["readonly", "database: read-only"], ["none", "database: none"], ["live", "database: LIVE (writes!)"]].map(([v, t]) => h("option", { value: v }, t)));
        const treq = h("textarea", { "data-testid": "test-request", spellcheck: "false" }, JSON.stringify({ method: "POST", path: "/pets", body: { name: "  ada lovelace " } }, null, 2));
        const out = h("div", { "data-testid": "test-result" });
        const run = button("Run test", async () => {
          let rq; try { rq = JSON.parse(treq.value); } catch (e) { throw new Error("request is not valid JSON: " + e.message); }
          let r;
          try { r = (await api("POST", `/scripts/${current}/versions/${tv.value}/test`, { step: tstep.value, request: rq, db: tdb.value })).data; }
          catch (e) { if (e.status === 401) throw e; out.replaceChildren(h("p", { class: "error", "data-testid": "test-error" }, `${e.code || "error"}: ${e.message}`)); return; }
          out.replaceChildren(h("div", { class: "panel" },
            h("p", {}, badge(r.result, r.ok ? "ok" : "bad"), ` ${r.elapsed_ms.toFixed(1)} ms · route `, h("code", {}, r.route), ` · outcome ${r.outcome.kind}`,
              r.contract && !r.contract.response_valid ? [" ", badge("response violates schema", "bad")] : null),
            r.detail ? pre(r.detail, "test-detail") : null,
            h("h3", {}, "Resulting state"), pre(r.resulting, "test-resulting"),
            (() => {
              const q = r.effects ? [...r.effects.in_transaction.map((e) => ["in the route's transaction", e]), ...r.effects.after_commit.map((e) => ["after commit", e])] : [];
              return q.length ? h("div", { "data-testid": "test-effects" }, h("h3", {}, "Queued writes (planned only: a test never applies them)"),
                q.map(([when, e]) => h("div", {}, badge(when), " ", h("code", {}, `${e.op} ${e.collection}${e.id ? "/" + e.id : ""}`), pre(e)))) : null;
            })(),
            r.logs.length ? [h("h3", {}, "Logs"), h("div", { "data-testid": "test-logs" }, r.logs.map((l) => h("div", {}, badge(l.level), " ", l.message)))] : null,
            r.contract && r.contract.response_problems.length ? [h("h3", {}, "Schema problems"), pre(r.contract.response_problems.join("\n"))] : null));
        }, "primary", "run-test");
        testPanel = h("div", { class: "panel" }, h("h3", {}, "Test before binding"),
          h("div", { class: "row" }, tv, tstep, tdb), treq, h("div", { class: "row" }, run), out);
      }
      root.replaceChildren(h("h2", {}, "Scripts"), h("div", { class: "cols" }, sidebar, h("div", {}, versionsPanel, editor, testPanel)));
    }
    await draw();
    return root;
  };

  // ---------------------------------------------------------------- data
  views.data = async () => {
    const root = h("div", {});
    let current = null;
    async function draw() {
      let cols;
      try { cols = (await api("GET", "/db/collections")).data.collections; }
      catch (e) { if (e.code === "database_disabled") return root.replaceChildren(h("h2", {}, "Data"), h("p", { class: "muted" }, "The database is not enabled (database.enabled).")); throw e; }
      const side = h("div", { class: "panel" }, h("h3", {}, "Collections"), h("div", { class: "list", "data-testid": "collections" }, cols.length
        ? cols.map((c) => h("button", { type: "button", class: c.name === current ? "active" : "", "data-collection": c.name, onclick: () => { current = c.name; draw().catch(fail); } }, `${c.name}  ·  ${c.count}`))
        : h("span", { class: "muted" }, "No data yet.")),
        h("h3", {}, "Seed"), h("div", { class: "row" }, (() => {
          const nm = h("input", { placeholder: "collection", size: 12, "data-testid": "seed-collection", value: current || "" });
          const ta = h("textarea", { "data-testid": "seed-json", placeholder: '[{"name":"Rex"}]', style: "min-height:70px" });
          const go = button("Seed", async () => {
            const name = nm.value.trim(); if (!name) throw new Error("collection name required");
            let docs; try { docs = JSON.parse(ta.value); } catch (e) { throw new Error("not valid JSON: " + e.message); }
            const r = (await api("POST", `/db/collections/${encodeURIComponent(name)}/docs`, docs)).data;
            notify("ok", `inserted ${r.inserted}, skipped ${r.skipped_existing}`); current = name; await draw();
          }, "", "seed");
          return h("div", { style: "width:100%" }, nm, ta, go);
        })()));
      let main = h("div", { class: "panel" }, h("p", { class: "muted" }, "Pick a collection."));
      if (current) {
        const filterField = h("input", { placeholder: "field", size: 10, "data-testid": "filter-field" });
        const filterValue = h("input", { placeholder: "value", size: 12, "data-testid": "filter-value" });
        const sort = h("input", { placeholder: "sort (-name)", size: 12, "data-testid": "sort" });
        const limit = h("input", { value: "25", size: 4, "data-testid": "limit" });
        const offset = h("input", { value: "0", size: 4, "data-testid": "offset" });
        const tableBox = h("div", { "data-testid": "docs" });
        const total = h("span", { class: "muted", "data-testid": "total" });
        const load = async () => {
          const p = new URLSearchParams({ limit: limit.value, offset: offset.value });
          if (sort.value.trim()) p.set("sort", sort.value.trim());
          if (filterField.value.trim()) p.set(filterField.value.trim(), filterValue.value);
          const r = (await api("GET", `/db/collections/${encodeURIComponent(current)}/docs?${p}`)).data;
          total.textContent = `${r.total} matching`;
          const keys = [...new Set(r.docs.flatMap((d) => Object.keys(d)))].slice(0, 8);
          tableBox.replaceChildren(r.docs.length ? h("table", {}, h("thead", {}, h("tr", {}, keys.map((k) => h("th", {}, k)), h("th", {}, ""))),
            h("tbody", {}, r.docs.map((d) => h("tr", {}, keys.map((k) => h("td", {}, d[k] === undefined ? "" : typeof d[k] === "object" ? JSON.stringify(d[k]) : String(d[k]))),
              h("td", {}, h("details", {}, h("summary", {}, "json"), pre(d))))))) : h("p", { class: "muted" }, "No documents."));
        };
        let armed = false;
        const clear = h("button", { type: "button", class: "danger", "data-testid": "clear" }, "Clear collection");
        clear.addEventListener("click", act(clear, async () => {
          if (!armed) { armed = true; clear.textContent = "Click again to confirm"; setTimeout(() => { armed = false; clear.textContent = "Clear collection"; }, 3000); return; }
          const r = (await api("DELETE", `/db/collections/${encodeURIComponent(current)}`)).data;
          notify("ok", `deleted ${r.deleted} documents`); await draw();
        }));
        main = h("div", { class: "panel" }, h("h3", {}, current),
          h("div", { class: "row" }, filterField, "=", filterValue, sort, "limit", limit, "offset", offset, button("Load", load, "primary", "load-docs"), total, h("span", { class: "spacer" }), clear), tableBox);
        queueMicrotask(() => load().catch(fail));
      }
      // ---- query box: the GraphQL-shaped read language (relations, projection, ordering)
      const qta = h("textarea", { "data-testid": "query-json", spellcheck: "false", style: "min-height:150px" },
        JSON.stringify({ from: current || (cols[0] && cols[0].name) || "orders", where: {}, select: ["*"], orderBy: [], limit: 10, total: true }, null, 2));
      const qout = h("div", { "data-testid": "query-result" });
      const runQuery = button("Run query", async () => {
        let body; try { body = JSON.parse(qta.value); } catch (e) { throw new Error("query is not valid JSON: " + e.message); }
        try {
          const r = (await api("POST", "/db/query", body)).data;
          qout.replaceChildren(h("p", { class: "muted" }, `${Array.isArray(r.data) ? r.data.length + " row(s)" : r.data ? "1 document" : "no document"}${r.total != null ? " · " + r.total + " match" : ""}`), pre(r.data, "query-data"));
        } catch (e) { if (e.status === 401) throw e; qout.replaceChildren(h("p", { class: "error", "data-testid": "query-error" }, `${e.code || "error"}: ${e.message}`)); }
      }, "primary", "run-query");
      const queryPanel = h("div", { class: "panel" }, h("h3", {}, "Query"),
        h("p", { class: "muted" }, 'Follow relations with {"items": {"from": "orderItems", "by": "orderId"}} (children) or {"customer": {"from": "customers", "id": "customerId"}} (parent); name them once under relations: in the config to reuse.'),
        qta, h("div", { class: "row" }, runQuery), qout);
      root.replaceChildren(h("h2", {}, "Data"), h("div", { class: "cols" }, side, main), queryPanel);
    }
    await draw();
    return root;
  };

  // ---------------------------------------------------------------- config
  views.config = async () => {
    const res = await api("GET", "/config");
    let version = res.data.version, etag = res.headers.get("etag");
    const ta = h("textarea", { class: "code", "data-testid": "config-json", spellcheck: "false" }, JSON.stringify(res.data.config, null, 2));
    const report = h("div", { "data-testid": "config-report" });
    const parse = () => { try { return JSON.parse(ta.value); } catch (e) { throw new Error("config is not valid JSON: " + e.message); } };
    const show = (rep, applied) => report.replaceChildren(h("div", { class: "panel" },
      h("p", {}, applied ? badge("applied", "ok") : badge("preview"), ` version ${rep.version}`, rep.persisted ? " · saved to file" : ""),
      rep.changes.length ? h("table", {}, rep.changes.map((c) => h("tr", {}, h("td", {}, h("code", {}, c.path)), h("td", {}, c.action)))) : h("p", { class: "muted" }, "No changes."),
      rep.restarts.length ? h("p", {}, "Listener restarts: ", rep.restarts.join(", ")) : null,
      rep.pending_process_restart.length ? h("p", { class: "error" }, "Needs a process restart: " + rep.pending_process_restart.join(", ")) : null,
      rep.warnings.map((w) => h("p", { class: "muted" }, w))));
    const failReport = (e) => { if (e.status === 401) throw e; report.replaceChildren(h("div", { class: "panel" }, h("p", { class: "error", "data-testid": "config-error" }, `${e.code || "error"}: ${e.message}`))); };
    const dry = button("Dry run", async () => { try { show((await api("PUT", "/config?dry_run=true", parse())).data, false); } catch (e) { failReport(e); } }, "", "config-dry");
    const apply = button("Apply", async () => {
      try { const r = await api("PUT", "/config", parse(), { ifMatch: etag }); show(r.data, true); notify("ok", `Applied (version ${r.data.version})`); const g = await api("GET", "/config"); ta.value = JSON.stringify(g.data.config, null, 2); etag = g.headers.get("etag"); version = g.data.version; }
      catch (e) { failReport(e); }
    }, "primary", "config-apply");
    const reload = button("Reload from disk", async () => { try { show((await api("POST", "/reload")).data, true); } catch (e) { failReport(e); } }, "", "config-reload");
    const refresh = button("Discard edits", () => route(), "", "config-refresh");
    return h("div", {}, h("h2", {}, "Config"), h("p", { class: "muted" }, "Secrets show as *** and are kept when you leave them. Updates are checked against the version you loaded (If-Match)."),
      h("div", { class: "panel" }, ta, h("div", { class: "row" }, dry, apply, reload, refresh, h("span", { class: "muted" }, `loaded version ${version}`)), report));
  };

  // ---------------------------------------------------------------- boot
  $("#login-form").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    $("#login-error").textContent = "";
    try { await signIn($("#token").value.trim()); }
    catch (e) { $("#login-error").textContent = e.status === 401 ? "That token was not accepted." : e.status === 429 ? "Too many attempts; wait a minute." : e.message; }
  });
  $("#logout").addEventListener("click", signOut);
  window.addEventListener("hashchange", route);
  buildNav();
  if (state.token) signIn(state.token).catch(() => signOut()); else showApp(false);
})();
