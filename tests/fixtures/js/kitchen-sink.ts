// Test guest: behaviour is selected with `?mode=...` so one component covers every host path.
import { log } from "stepwise:hook/host@0.1.0";
import * as db from "stepwise:hook/db@0.1.0";
import * as fx from "stepwise:hook/effects@0.1.0";
import * as qry from "stepwise:hook/query@0.1.0";

type KV = { name: string; value: string };
const q = (ctx: any, k: string): string | undefined => ctx.query.find((x: KV) => x.name === k)?.value;
// `result<T, string>` imports surface differently across JS runtimes (throw vs {tag,val}); normalise.
function attempt(f: () => any): { ok: boolean; val?: any; err?: string } {
  try {
    const r = f();
    if (r && typeof r === "object" && (r.tag === "ok" || r.tag === "err")) {
      return r.tag === "ok" ? { ok: true, val: r.val } : { ok: false, err: String(r.val) };
    }
    return { ok: true, val: r };
  } catch (e: any) {
    return { ok: false, err: String(e?.payload ?? e?.message ?? e) };
  }
}
const proceed = (patch: Record<string, unknown> = {}) => ({ tag: "proceed", val: patch });

export const handler = {
  handle(ctx: any) {
    const mode = q(ctx, "mode") ?? "noop";
    const body = ctx.requestBody ? JSON.parse(ctx.requestBody) : undefined;
    const state = JSON.parse(ctx.state);

    switch (mode) {
      case "noop":
        return proceed();

      case "normalize": {
        // business logic: trim + Title Case the pet's name, remember that we did
        const name = String(body?.name ?? "").trim().replace(/\b\w/g, (c: string) => c.toUpperCase());
        return proceed({
          requestBody: JSON.stringify({ ...body, name }),
          state: JSON.stringify({ ...state, normalized: true }),
        });
      }

      case "reject":
        if (body?.name === "forbidden") {
          return { tag: "fail", val: { status: 422, code: "name_blocked", message: "that name is not allowed", details: JSON.stringify({ field: "name" }) } };
        }
        return proceed();

      case "respond":
        return { tag: "respond", val: { status: 200, headers: [{ name: "x-served-by", value: "hook" }], body: JSON.stringify({ cached: true, step: ctx.step }) } };

      case "post": {
        // post_process: decorate what `execute` produced
        const resp = ctx.response.body ? JSON.parse(ctx.response.body) : {};
        const decorate = (o: any) => ({ ...o, decoratedBy: "post-hook", seenNormalized: state.normalized === true });
        return proceed({
          responseHeaders: [...ctx.response.headers, { name: "x-hook", value: "ok" }],
          responseBody: JSON.stringify(Array.isArray(resp) ? resp.map(decorate) : decorate(resp)),
        });
      }

      case "db-count":
      case "db-get":
      case "db-find":
      case "db-insert":
      case "db-merge":
      case "db-delete":
      case "db-denied": {
        const id = q(ctx, "id") ?? "1";
        const r =
          mode === "db-count" ? attempt(() => db.count("pets", [])) :
          mode === "db-get" ? attempt(() => db.get("pets", id)) :
          mode === "db-find" ? attempt(() => db.find("pets", [["tag", "dog"]], 10)) :
          mode === "db-insert" ? attempt(() => db.insert("audit", id, JSON.stringify({ by: ctx.routeId, step: ctx.step }))) :
          mode === "db-merge" ? attempt(() => db.merge("pets", id, JSON.stringify({ seen: true }))) :
          mode === "db-delete" ? attempt(() => db.delete("pets", id)) :
          attempt(() => db.get("secrets", "1"));
        return proceed({ state: JSON.stringify({ ...state, db: r }) });
      }

      // ---- declarative reads: the query comes in as the `q` URL parameter (JSON)
      case "q-run": {
        const r = attempt(() => qry.run(q(ctx, "q") ?? "{}"));
        return proceed({ state: JSON.stringify({ ...state, query: r.ok ? { ok: true, val: JSON.parse(r.val) } : r }) });
      }

      // ---- declarative writes (effects): the host applies them atomically; the hook never writes
      case "fx-order": {
        const sku = q(ctx, "sku") ?? "sku-1";
        const qty = Number(q(ctx, "qty") ?? "1");
        fx.emit(JSON.stringify({ op: "update", collection: "products", id: sku, inc: { stock: -qty },
          require: { stock: { gte: qty } }, onFail: { status: 409, code: "out_of_stock", message: "not enough stock" } }));
        fx.emit(JSON.stringify({ op: "insert", collection: "audit", doc: { event: "order", sku, qty } }));
        return proceed();
      }
      case "fx-then-fail":
        fx.emit(JSON.stringify({ op: "insert", collection: "audit", doc: { event: "should-not-exist" } }));
        return { tag: "fail", val: { status: 422, code: "name_blocked", message: "no" } };
      case "fx-bad": {
        const r1 = attempt(() => fx.emit("{not json"));
        const r2 = attempt(() => fx.emit(JSON.stringify({ op: "update", collection: "audit", id: "x" })));
        return proceed({ state: JSON.stringify({ ...state, emit: [r1, r2] }) });
      }
      case "fx-denied": {
        const r = attempt(() => fx.emit(JSON.stringify({ op: "insert", collection: "secrets", doc: {} })));
        return proceed({ state: JSON.stringify({ ...state, emit: [r] }) });
      }
      case "fx-after":
        fx.emit(JSON.stringify({ op: "insert", collection: "audit", doc: { event: "responded", step: ctx.step } }));
        return proceed();
      case "fx-after-bad":
        fx.emit(JSON.stringify({ op: "update", collection: "audit", id: "does-not-exist", inc: { n: 1 } }));
        return proceed();
      case "fx-respond":
        fx.emit(JSON.stringify({ op: "insert", collection: "audit", doc: { event: "cached" } }));
        return { tag: "respond", val: { status: 200, headers: [], body: JSON.stringify({ cached: true }) } };

      case "echo":
        return proceed({ responseStatus: 200, responseBody: JSON.stringify({ ctx: { ...ctx, state } }) });

      case "log":
        log("info", `hello from ${ctx.routeId} at ${ctx.step}`);
        log("error", "something to look at");
        return proceed();

      case "loop":
        for (;;) {} // must be stopped by the host's deadline

      case "throw":
        throw new Error("boom from guest");

      case "badjson":
        return proceed({ requestBody: "{this is not json" });

      case "badschema":
        return proceed({ requestBody: JSON.stringify({ tag: "no name here" }) });

      case "memhog": {
        const chunks: string[] = [];
        for (let i = 0; i < 64; i++) chunks.push("x".repeat(16_000_000)); // far beyond the memory cap
        return proceed({ state: JSON.stringify({ n: chunks.length }) });
      }

      default:
        return { tag: "fail", val: { status: 400, code: "unknown_mode", message: mode } };
    }
  },
};
