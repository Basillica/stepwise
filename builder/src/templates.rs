//! Starter sources. Each one is built by the test-suite, so they are known to compile.

pub const TYPESCRIPT: &str = r#"// A Stepwise hook. It runs in a sandbox, once per request, at the pipeline step you bind it to.
// Return proceed(patch), respond(response) or fail(error). Bodies are JSON *text*.
import { log } from "stepwise:hook/host@0.1.0";

export const handler = {
  handle(ctx: any) {
    // ctx: requestId, routeId, step, method, path, pathParams, query, headers,
    //      requestBody (JSON text), identity (JSON text), state (JSON text), response
    const body = ctx.requestBody ? JSON.parse(ctx.requestBody) : {};
    log("info", `handling ${ctx.method} ${ctx.path} at ${ctx.step}`);

    if (typeof body.name === "string" && body.name.trim() === "") {
      return { tag: "fail", val: { status: 422, code: "empty_name", message: "name must not be empty" } };
    }
    if (typeof body.name === "string") {
      // Normalize before the request is validated again and stored.
      return { tag: "proceed", val: { requestBody: JSON.stringify({ ...body, name: body.name.trim() }) } };
    }
    return { tag: "proceed", val: {} };
  },
};
"#;

pub const JAVASCRIPT: &str = r#"// A Stepwise hook. It runs in a sandbox, once per request, at the pipeline step you bind it to.
// Return proceed(patch), respond(response) or fail(error). Bodies are JSON *text*.
import { log } from "stepwise:hook/host@0.1.0";

export const handler = {
  handle(ctx) {
    const body = ctx.requestBody ? JSON.parse(ctx.requestBody) : {};
    log("info", `handling ${ctx.method} ${ctx.path} at ${ctx.step}`);

    if (typeof body.name === "string" && body.name.trim() === "") {
      return { tag: "fail", val: { status: 422, code: "empty_name", message: "name must not be empty" } };
    }
    if (typeof body.name === "string") {
      return { tag: "proceed", val: { requestBody: JSON.stringify({ ...body, name: body.name.trim() }) } };
    }
    return { tag: "proceed", val: {} };
  },
};
"#;

pub const PYTHON: &str = r#""""A Stepwise hook. It runs in a sandbox, once per request, at the pipeline step you bind it to.

Return Outcome_Proceed(Patch(...)), Outcome_Respond(Response(...)) or Outcome_Fail(Failure(...)).
Bodies are JSON *text*. Fields left as None are untouched.
"""
import json

from wit_world import exports
from wit_world.imports import host, types


class Handler(exports.Handler):
    def handle(self, ctx: types.Context) -> types.Outcome:
        body = json.loads(ctx.request_body) if ctx.request_body else {}
        host.log(host.Level.INFO, f"handling {ctx.method} {ctx.path} at {ctx.step}")

        name = body.get("name")
        if isinstance(name, str) and name.strip() == "":
            return types.Outcome_Fail(types.Failure(422, "empty_name", "name must not be empty", None))
        if isinstance(name, str):
            patch = types.Patch(
                state=None,
                request_body=json.dumps({**body, "name": name.strip()}),
                response_status=None,
                response_headers=None,
                response_body=None,
            )
            return types.Outcome_Proceed(patch)
        return types.Outcome_Proceed(
            types.Patch(state=None, request_body=None, response_status=None, response_headers=None, response_body=None)
        )
"#;
