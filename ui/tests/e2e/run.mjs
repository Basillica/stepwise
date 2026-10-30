// End-to-end test of the management UI. The page is the real one (served by the real proxy),
// running in jsdom; the backend is a real stepwise server + builder. See run.sh.
import { JSDOM, VirtualConsole } from "jsdom";

const env = (k) => process.env[k] || (() => { throw new Error(`${k} is required`); })();
const UI = env("UI_URL"), UI_TOKEN = env("UI_TOKEN"), DATA = env("DATA_URL"), CONTROL = env("CONTROL_URL"), CONTROL_TOKEN = env("CONTROL_TOKEN");

const TS_RULES = `
import { log } from "stepwise:hook/host@0.1.0";
export const handler = {
  handle(ctx: any) {
    const body = ctx.requestBody ? JSON.parse(ctx.requestBody) : {};
    if (body.name === "forbidden") return { tag: "fail", val: { status: 422, code: "name_blocked", message: "nope" } };
    const name = String(body.name ?? "").trim().replace(/\\b\\w/g, (c: string) => c.toUpperCase());
    log("info", "normalized to " + name);
    return { tag: "proceed", val: { requestBody: JSON.stringify({ ...body, name }) } };
  },
};
`;

const jsErrors = [];
const vc = new VirtualConsole();
vc.on("jsdomError", (e) => jsErrors.push(e.stack || String(e)));
vc.on("error", (...a) => jsErrors.push(a.join(" ")));

const dom = await JSDOM.fromURL(UI + "/", {
  runScripts: "dangerously", resources: "usable", pretendToBeVisual: true, virtualConsole: vc,
  beforeParse(window) {
    window.fetch = async (u, o) => {
      const r = await fetch(new URL(u, UI), o);
      if (process.env.DEBUG_FETCH) console.log(`      [fetch] ${(o && o.method) || "GET"} ${u} -> ${r.status}`);
      return r;
    };
  },
});
const { window } = dom, doc = window.document;
const $ = (s) => doc.querySelector(s), $$ = (s) => [...doc.querySelectorAll(s)];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let passed = 0;
async function step(name, fn) {
  try { await fn(); passed++; console.log("  ok   " + name); }
  catch (e) { console.log("  FAIL " + name + "\n       " + (e.stack || e).toString().split("\n").slice(0, 6).join("\n       ")); console.log("\n--- view text ---\n" + (doc.querySelector("main")?.textContent || "").slice(0, 800));
    console.log("--- notices ---\n" + [...doc.querySelectorAll(".notice")].map((n) => n.className + ": " + n.textContent).join("\n"));
    if (jsErrors.length) console.log("--- page errors ---\n" + jsErrors.join("\n").slice(0, 1500));
    process.exitCode = 1; throw e; }
}
async function waitFor(fn, what, timeout = 90000) {
  const t0 = Date.now();
  for (;;) {
    let v; try { v = fn(); } catch { v = null; }
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error("timed out waiting for: " + what);
    await sleep(40);
  }
}
const click = (el) => el.dispatchEvent(new window.MouseEvent("click", { bubbles: true, cancelable: true }));
const setValue = (el, v) => { el.value = v; el.dispatchEvent(new window.Event("input", { bubbles: true })); el.dispatchEvent(new window.Event("change", { bubbles: true })); };
const tid = (id) => $(`[data-testid="${id}"]`);
const text = (el) => (el ? el.textContent : "");
const go = (view) => { window.location.hash = "#/" + view; };
const clearNotices = () => $$(".notice").forEach((n) => n.remove());
const notice = (kind, re) => $$(`.notice.${kind}`).some((n) => re.test(n.textContent));
const control = async (method, path, body, extra = {}) => {
  const r = await fetch(CONTROL + "/api/v1" + path, { method, headers: { authorization: "Bearer " + CONTROL_TOKEN, "content-type": "application/json", ...extra }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null), etag: r.headers.get("etag") };
};
const data = (method, path, body) => fetch(DATA + path, { method, headers: { "content-type": "application/json" }, body: body ? JSON.stringify(body) : undefined });

console.log("UI end-to-end");
await waitFor(() => doc.readyState === "complete" && $("#login-form"), "page load");

await step("sign-in rejects a wrong token, accepts the right one", async () => {
  setValue($("#token"), "wrong-token-0000000000");
  $("#login-form").dispatchEvent(new window.Event("submit", { bubbles: true, cancelable: true }));
  await waitFor(() => /not accepted/.test(text(tid("login-error"))), "login error");
  if ($("#login").hidden) throw new Error("login overlay must stay up");
  setValue($("#token"), UI_TOKEN);
  $("#login-form").dispatchEvent(new window.Event("submit", { bubbles: true, cancelable: true }));
  await waitFor(() => $("#login").hidden && !$("#top").hidden, "app shell");
});

await step("overview shows live server facts", async () => {
  await waitFor(() => $("#view dl"), "overview");
  const t = text($("#view"));
  for (const needle of ["ready", "sqlite", "Petstore", "typescript"]) if (!t.includes(needle)) throw new Error(`overview missing "${needle}": ${t.slice(0, 300)}`);
});

await step("scripts: template loads, a compiler error is shown with its log, good source builds and saves", async () => {
  go("scripts");
  await waitFor(() => tid("source"), "scripts view");
  setValue(tid("language"), "typescript");
  click(tid("load-template"));
  await waitFor(() => /Stepwise hook/.test(tid("source").value), "template");
  setValue(tid("script-name"), "e2e-rules");

  setValue(tid("source"), "export const handler = {\n  handle(ctx) {\n    return {\n");
  click(tid("build-source"));
  await waitFor(() => /build_failed/.test(text(tid("build-error"))), "build error", 120000);
  if (!text(tid("build-log")).trim()) throw new Error("the compiler log must be shown");
  if ($$('[data-testid="versions"] tbody tr').length) throw new Error("a failed build must not create a version");

  setValue(tid("source"), TS_RULES);
  click(tid("check-source"));
  await waitFor(() => /builds OK/.test(text(tid("build-result"))), "dry run", 120000);
  if ($$('[data-testid="versions"] tbody tr').length) throw new Error("dry run must not save");
  click(tid("build-source"));
  await waitFor(() => $$('[data-testid="versions"] tbody tr').length === 1, "version 1 listed", 120000);
  await waitFor(() => /saved/.test(text(tid("build-result"))), "saved message survives the redraw");
  if (!/wasm:e2e-rules@1/.test(text(tid("build-result")))) throw new Error("pinned reference not shown");
});

await step("scripts: the source of a version can be reopened", async () => {
  setValue(tid("source"), "// scratch");
  click(tid("source-1"));
  await waitFor(() => /normalized to/.test(tid("source").value), "source reloaded");
});

await step("scripts: test panel runs the hook in the sandbox and shows state, logs and contract", async () => {
  click(tid("run-test"));
  await waitFor(() => /Ada Lovelace/.test(text(tid("test-resulting"))), "test result", 60000);
  const t = text(tid("test-result"));
  if (!/ok/.test(t) || !/normalized to Ada Lovelace/.test(text(tid("test-logs")))) throw new Error("test panel incomplete: " + t.slice(0, 300));
  setValue(tid("test-request"), "{ not json");
  click(tid("run-test"));
  await waitFor(() => $$(".notice.error").some((n) => /not valid JSON/.test(n.textContent)), "bad-JSON notice");
});

await step("scripts: promoting an alias works", async () => {
  click(tid("promote-1"));
  await waitFor(() => notice("ok", /live → v1/), "promote notice");
  await waitFor(() => /live/.test(text($('tr[data-version="1"]'))), "alias badge on v1");
});

await step("routes: bind the script to POST /pets, preview, apply; behaviour changes on the data plane", async () => {
  go("routes");
  await waitFor(() => $$('[data-testid="routes-table"] tbody tr').length >= 8, "routes table");
  click($('tr[data-route="POST /pets"]'));
  await waitFor(() => tid("add-ref"), "route detail");
  setValue(tid("add-step"), "pre_process"); setValue(tid("add-slot"), "before"); setValue(tid("add-ref"), "wasm:e2e-rules@live");
  click(tid("add-hook"));
  await waitFor(() => /e2e-rules@live/.test(text(tid("route-detail"))), "staged hook shown");
  click(tid("preview-routes"));
  await waitFor(() => /preview/.test(text(tid("route-report"))) && /routes\.POST \/pets/.test(text(tid("route-report"))) && /hot/.test(text(tid("route-report"))), "preview report");
  const before = await data("POST", "/pets", { name: "  untouched  " });
  if ((await before.json()).name !== "  untouched  ") throw new Error("preview must not change behaviour");
  clearNotices();
  click(tid("apply-routes"));
  await waitFor(() => notice("ok", /Applied/), "applied notice");
  await waitFor(() => /pinned|[0-9a-f]{10}/.test(text(tid("route-detail"))) || tid("route-detail"), "reload");
  const r = await data("POST", "/pets", { name: "  grace hopper " });
  const j = await r.json();
  if (r.status !== 201 || j.name !== "Grace Hopper") throw new Error("hook not applied: " + JSON.stringify(j));
  const rej = await data("POST", "/pets", { name: "forbidden" });
  if (rej.status !== 422) throw new Error("hook rejection expected 422, got " + rej.status);
});

await step("data: browse, filter, seed, hostile content is rendered as text, clear needs a second click", async () => {
  go("data");
  await waitFor(() => $('[data-collection="pets"]'), "pets collection");
  click($('[data-collection="pets"]'));
  await waitFor(() => /Grace Hopper/.test(text(tid("docs"))), "documents listed");

  setValue(tid("seed-collection"), "pets");
  setValue(tid("seed-json"), JSON.stringify([{ name: "<img src=x onerror=\"window.__pwned=1\">", tag: "evil" }, { name: "Seeded", tag: "x" }]));
  click(tid("seed"));
  await waitFor(() => notice("ok", /inserted 2/), "seed notice");
  await waitFor(() => /Seeded/.test(text(tid("docs"))), "seeded docs listed");
  if ($("#view img")) throw new Error("document content became markup");
  if (window.__pwned) throw new Error("script from document content executed");
  if (!text(tid("docs")).includes('<img src=x onerror="window.__pwned=1">')) throw new Error("hostile text should appear literally");

  setValue(tid("filter-field"), "tag"); setValue(tid("filter-value"), "x");
  click(tid("load-docs"));
  await waitFor(() => /1 matching/.test(text(tid("total"))), "filter applied");

  click(tid("clear"));
  await waitFor(() => /confirm/i.test(text(tid("clear"))), "confirm state");
  const still = await control("GET", "/db/collections");
  if (!still.json.collections.length) throw new Error("first click must not delete");
  click(tid("clear"));
  await waitFor(() => notice("ok", /deleted \d+ documents/), "cleared");
  const after = await control("GET", "/db/collections");
  if (after.json.collections.length) throw new Error("collection should be gone");
});

await step("config: dry run previews, apply persists, a stale version is refused", async () => {
  go("config");
  await waitFor(() => tid("config-json"), "config view");
  const cfg = JSON.parse(tid("config-json").value);
  if (JSON.stringify(cfg).includes(CONTROL_TOKEN)) throw new Error("secrets must be redacted in the UI");
  cfg.logging.level = "debug";
  setValue(tid("config-json"), JSON.stringify(cfg, null, 2));
  click(tid("config-dry"));
  await waitFor(() => /logging\.level/.test(text(tid("config-report"))) && /hot/.test(text(tid("config-report"))), "dry-run report");
  if ((await control("GET", "/config")).json.config.logging.level === "debug") throw new Error("dry run must not apply");

  // someone else changes the config after we loaded it
  const cur = await control("GET", "/config");
  const other = cur.json.config; other.app_name = "changed elsewhere";
  if ((await control("PUT", "/config", other)).status !== 200) throw new Error("setup change failed");
  click(tid("config-apply"));
  await waitFor(() => /version_conflict/.test(text(tid("config-error"))), "stale-version refusal");

  click(tid("config-refresh"));
  await waitFor(() => tid("config-json") && /changed elsewhere/.test(tid("config-json").value), "refresh shows the other change");
  const fresh = JSON.parse(tid("config-json").value); fresh.logging.level = "debug";
  setValue(tid("config-json"), JSON.stringify(fresh));
  clearNotices(); // an "Applied" notice from an earlier step must not satisfy this wait
  click(tid("config-apply"));
  await waitFor(() => notice("ok", /Applied/), "applied");
  if ((await control("GET", "/config")).json.config.logging.level !== "debug") throw new Error("not applied");

  setValue(tid("config-json"), "{ nope");
  click(tid("config-dry"));
  await waitFor(() => /not valid JSON/.test(text(tid("config-error"))), "invalid JSON message");
});

await step("sign out returns to the login screen and forgets the token", async () => {
  click($("#logout"));
  await waitFor(() => !$("#login").hidden && $("#top").hidden, "login screen");
  if (window.sessionStorage.getItem("stepwise-ui-token")) throw new Error("token must be cleared");
  if (text($("#view")).includes("e2e-rules")) throw new Error("data must be cleared from the page");
});

if (jsErrors.length) { console.log("\nuncaught page errors:\n" + jsErrors.join("\n")); process.exitCode = 1; }
console.log(`\n${passed} steps passed${process.exitCode ? ", with failures" : ""}`);
window.close();
process.exit(process.exitCode || 0);
