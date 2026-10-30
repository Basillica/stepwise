"""Test guest in Python. Same contract as the JS one: behaviour picked with ?mode=..."""
import json

from wit_world import exports
from wit_world.imports import host, types


def _q(ctx: types.Context, key: str):
    return next((kv.value for kv in ctx.query if kv.name == key), None)


class Handler(exports.Handler):
    def handle(self, ctx: types.Context) -> types.Outcome:
        mode = _q(ctx, "mode") or "noop"
        body = json.loads(ctx.request_body) if ctx.request_body else None
        state = json.loads(ctx.state)

        def proceed(**kw):
            fields = dict(state=None, request_body=None, response_status=None, response_headers=None, response_body=None)
            fields.update(kw)
            return types.Outcome_Proceed(types.Patch(**fields))

        if mode == "normalize":
            name = str((body or {}).get("name", "")).strip().title()
            return proceed(
                request_body=json.dumps({**(body or {}), "name": name}),
                state=json.dumps({**state, "normalized": True, "lang": "python"}),
            )
        if mode == "reject" and (body or {}).get("name") == "forbidden":
            return types.Outcome_Fail(types.Failure(422, "name_blocked", "that name is not allowed", None))
        if mode == "post":
            resp = json.loads(ctx.response.body) if ctx.response.body else {}
            deco = lambda o: {**o, "decoratedBy": "python-post-hook"}
            out = [deco(x) for x in resp] if isinstance(resp, list) else deco(resp)
            return proceed(response_body=json.dumps(out))
        if mode == "log":
            host.log(host.Level.INFO, f"hello from python at {ctx.step}")
        return proceed()
