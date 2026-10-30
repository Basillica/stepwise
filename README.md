# Stepwise

A spec-driven API server in Rust. Point it at an OpenAPI file and a YAML config and you get a
running API where **every endpoint is a state machine of steps**, backed by a database, with
**business logic written as WebAssembly hooks** (Rust, JS/TS, Python) and a separate, documented
**control plane** for a management UI.

```
export STEPWISE_CONTROL_TOKEN=$(openssl rand -hex 24)
cargo run -- check examples/config.yaml   # compile everything, print routes + inferred DB operations
cargo run -- run   examples/config.yaml   # data plane :8080, control plane :9090
```

Built with Rust 1.91 as a workspace: `stepwise` (server), `stepwise-store` (storage; no Wasmtime, so it
builds and tests in seconds), `stepwise-builder`, `stepwise-ui`.

**Test status, honestly.**
- *Run and green in the dev sandbox:* the store crate (24 unit + 4 backend-contract + 2 CRUD-executor + 2 read-engine + 2 data-rules tests) on
  **SQLite and on PostgreSQL 16**, including the concurrency properties (no overselling, no lost updates, one
  winner among duplicate inserts, no deadlocks between opposite-direction transfers); the UI proxy (7) and the
  UI end-to-end (10 steps in jsdom against a live server + builder).
- *Type-checked only* (the sandbox cannot build Wasmtime to run them): the server crate's suites, including the
  new `tests/effects.rs`, `tests/query.rs` and `tests/rules.rs`. The last full green run of the server and builder suites (81 + 8) predates the
  PostgreSQL and effects work. Run `cargo test --workspace` and `./scripts/test-postgres.sh` first.

## The state machine

```
rate_limit → authenticate → authorize → validate_request → pre_process
           → execute → post_process → validate_response → respond
```

Each step runs `before` hooks → (`replace` hook **or** the built-in) → `after` hooks. Any can
`Continue`, `ShortCircuit` with a response, or `Fail`. The built-in `execute` is **database CRUD**
when the route looks like a resource, otherwise a schema-generated mock.

## Database: every resource endpoint is CRUD

With `database.enabled: true`, operations are mapped from method + path shape + schemas:

| Operation | Does |
|---|---|
| `GET /pets` (array response) | list; `?limit&offset&sort=-field`, filters = query params declared in the spec, `x-total-count` header |
| `POST /pets` | create; id from a per-collection sequence (integer schema) or uuid; `Location` header; 409 on duplicate |
| `GET /pets/{id}` | read (404 if missing) |
| `PUT /pets/{id}` / `PATCH /pets/{id}` | replace / RFC 7396 merge patch (URL id wins, ids immutable) |
| `DELETE /pets/{id}` | delete |

**Backends:** `database.db_type: sqlite` (default) or `postgres` (`url: ${DATABASE_URL}`, `schema`, optional
`tls_ca_path`; TLS always verifies the server certificate). Both store JSON documents behind the same
`DocStore` trait and answer queries identically (filters are bound parameters; RFC 7396 merge patch;
same NULL ordering). `tests/store_contract.rs` is one behavioural suite run against every backend; set
`STEPWISE_TEST_POSTGRES_URL` (or run `scripts/test-postgres.sh`) to include PostgreSQL, and
`STEPWISE_TEST_POSTGRES_TLS_URL` + `STEPWISE_TEST_POSTGRES_CA` for the TLS check. `main` pings the
database at startup so a bad URL fails fast. Compose: `docker/compose.postgres.yaml`.

Anything that does not look like a resource (`GET /health`, `POST /pets/{id}/adopt`) keeps its mock
response. Override per route: `crud: {collection, operation, id_param, id_field, id_kind}`;
`operation: none` opts out. Storage is schema-less JSON documents in SQLite (WAL, pooled) behind a
`DocStore` trait; request input is never interpolated into SQL.

## Business logic: WebAssembly hooks

Hooks are components built against `wit/stepwise-hook.wit`. They receive the request context
(JSON text), and return `proceed(patch)`, `respond(response)` or `fail(error)`. Because hooks run
around the database step, a `pre_process` hook can normalize/validate/enrich a body before it is
stored, and a `post_process` hook can reshape what the database returned.

**Scripts and pinning.** Upload with `POST /api/v1/scripts/{name}/versions` (raw `.wasm`). Each
script has a unique id (`scr_…`) and immutable numbered versions stored by SHA-256. Routes pin in
config: `wasm:name@3`, `wasm:name@sha256:ab12…`, or a movable alias `wasm:name@live` (promote or
roll back with `PUT …/aliases/live`; the runtime rebuilds atomically and reverts if that fails).
References resolve to a digest when the runtime is built, so the request path never touches the store.

**Test before binding.** `POST /scripts/{name}/versions/{n}/test` runs a version in the real sandbox
against a fixture in the context of a route and reports the result, resulting state, whether the
response still matches the OpenAPI schema, logs, and timing. The database is **read-only** during
tests unless you pass `"db": "live"`.

**Typed, policed contract.** Everything a guest returns is checked by the host: valid JSON, legal
status/headers, and a patched request body must still satisfy the route's request schema. Violations
become a 500 `hook_contract_violation`; they never reach the client's data or the database.

**Database access from hooks** is capability-based: a route may touch its own CRUD collection plus
whatever `routes."<id>".db.grants` lists (`{audit: rw, inventory: r}`). Other collections are refused.

**Sandbox.** Fresh instance per call (pooled, pre-linked, precompiled `.cwasm`), memory cap,
cooperative epoch yielding so a spinning guest cannot starve the server, wall-clock timeout,
no filesystem/env/network, credentials headers hidden unless `wasm.expose_credentials`.

Building guests by hand: JS/TS `jco componentize app.ts --wit wit --world-name hook --backend qjs
--backend-qjs-disable-async --backend-qjs-stub-wasi` (1.7 MB, no WASI), or SpiderMonkey with
`--disable http --disable fetch-event`; Python `componentize-py -d wit -w hook componentize app`;
Rust `cargo component` / `wit-bindgen` (untested here). See `tests/fixtures/` for working examples.

## Writes from scripts: effects (scripts describe, the host executes)

Scripts do not write to the database; they **describe** writes with `effects.emit(json)` and the host applies
them **in one transaction together with the route's own operation**:

```json
{ "op": "update", "collection": "products", "id": "sku-1",
  "inc": { "stock": -2 }, "require": { "stock": { "gte": 2 } },
  "onFail": { "status": 409, "code": "out_of_stock", "message": "not enough stock" } }
```

* `op`: `insert | update | replace | upsert | delete`. Updates take `merge` (RFC 7396), `set`, `inc`, `push`,
  `unset` (applied in that order; dotted paths; `inc` of a missing field starts at 0). Atomic operators remove the
  read-modify-write race that `db.get` + `db.replace` has.
* `require` holds conditions on the *current* document, checked under a row lock: scalar (= `eq`) or
  `{eq, ne, gt, gte, lt, lte, in, nin, exists}`. `expectVersion` does optimistic concurrency, `ifMissing: "skip"`
  ignores absent documents, `onFail` chooses the error the client gets when an effect cannot be applied.
* **Atomicity.** Effects queued before `execute` (and by `execute.replace`) commit with the route's write. If a
  `require` fails, the route's write fails (409/412/…), or anything else goes wrong, **nothing** is written. A
  hook that returns `fail` discards what it queued; a hook that `respond`s early still gets its writes (in one
  transaction, without the route's own write).
* **After the write.** Effects queued by `execute.after`, `post_process` and later are applied once the response
  is decided, **best effort**: failures are logged and counted (`stepwise_effects_total`), never change the response.
* Effects are validated when emitted (malformed ones throw in the script, immediately), limited to 100 per
  request, and only for collections the route may write (its own CRUD collection, plus `routes."…".db.grants`).
* **Test before binding:** the test endpoint reports the queued plan (`effects.in_transaction` / `after_commit`)
  and **never applies it**, so the UI can show "this hook would write these documents".
* **Versions, ETag, If-Match.** Every document has a version (1 on insert, +1 per write). Reads and writes return
  `ETag: "<version>"`; `PUT`/`PATCH`/`DELETE` honour `If-Match` (`412` + the current ETag on mismatch).
* The imperative `db` interface still exists for odd cases but is *not* part of the route's transaction.
* **Compatibility:** components built against this WIT import `effects` and need a server of this version;
  components built earlier keep running on it.

Under the hood, `DocStore::apply(plan)` is the single write path (`store/src/effects.rs`: a pure `step()` decides
each effect; SQLite takes `BEGIN IMMEDIATE`, PostgreSQL locks the plan's rows in canonical order so plans cannot
deadlock). Reads are covered by the query language below and common patterns by the no-code `data:` rules after it. Not built
yet: a public `/graphql` endpoint.

## Reading data: one query, relations included

A small GraphQL-shaped JSON language: pick fields, follow relations, filter, order and page, in one request.

```json
{ "from": "orders",
  "where":   { "status": { "in": ["open", "paid"] }, "total": { "gte": 100 } },
  "select":  ["id", "total",
    { "customer": { "from": "customers",  "id": "customerId", "select": ["name"] } },
    { "items":    { "from": "orderItems", "by": "orderId", "orderBy": "-qty", "limit": 3,
                    "select": ["sku", "qty", { "product": { "from": "products", "id": "productId", "select": ["name"] } }] } }],
  "orderBy": [{ "createdAt": "desc" }], "limit": 20, "total": true }
```

* **Relations are always explicit.** `by` = has-many (children whose `by` field equals this document's id);
  `id` = belongs-to (this document's field holding the other document's id). Name them once in the config and
  reuse them (`{"items": "items"}`, or `{"items": {"rel": "items", "select": [...]}}` to override):
  ```yaml
  relations:
    orders:     { items: { from: orderItems, by: orderId }, customer: { from: customers, id: customerId } }
    orderItems: { product: { from: products, id: productId } }
  ```
* **No N+1.** Each relation is one batched query per level, however many parent rows there are (tested by
  counting backend calls: identical for 2 rows and 12). Per-relation `where`, `orderBy`, `limit` (per parent).
* **One filter language.** `where` uses the same operators and evaluator as effect conditions
  (`eq ne gt gte lt lte in nin exists`), so every backend agrees. Plain equality is evaluated by the database;
  anything else scans the collection, capped at 10,000 documents. Beyond the caps (`max_scan`, `max_docs` = 5,000
  per query, depth 4, `limit` ≤ 1,000) the query fails with `422 query_too_broad` instead of getting slow.
* **Where it is available:** `query.run(json)` for hooks (every collection touched, including relation
  targets, must be readable by the route; results are bounded by `wasm.max_payload_bytes`), and
  `POST /api/v1/db/query` on the control plane (operator access) plus a query box in the UI's Data view.
  Not yet: a public data-plane `/graphql` endpoint.
* Output: `{"data": [...], "total": n}`, or `{"data": {...} | null}` for a query by `id`. Missing and dangling
  references become `null`; fields not selected are not returned.

## No-code data rules: common patterns without a script

Per route, under `data:` (all optional). Every rule compiles down to the effects, conditions and filters above, so
it runs in the route's single transaction, under row locks, on both backends.

```yaml
routes:
  "POST /orders":
    data:
      owner: ownerId                 # scope to the signed-in user (subject of the token / JWT)
      timestamps: { created: createdAt, updated: updatedAt, deleted: deletedAt }
      defaults: { status: open }     # set on create when the client did not
      readonly: [status, total]      # clients cannot set them (create strips, replace keeps, merge strips)
      hide: [internalNotes]          # removed from every response (still stored)
      soft_delete: deleted           # DELETE sets this flag; reads, lists and writes skip flagged rows
      audit: auditLog                # one ledger row per successful write
      effects:                       # extra writes in the same transaction, with ${…} templates
        - op: update
          collection: products
          id: "${body.sku}"
          inc: { stock: "${-body.qty}" }
          require: { stock: { gte: "${body.qty}" } }
          onFail: { status: 409, code: out_of_stock, message: "not enough stock" }
```

* **owner:** the owner field is stamped from the *authenticated subject* (a forged value in the body is ignored).
  Other users get **404** (not 403, so existence does not leak) for read, replace, merge and delete, and their lists
  are filtered; the client cannot widen the scope via query parameters. Needs an authenticated route (checked at
  startup) and a signed-in caller (else 401).
* **timestamps / defaults / readonly:** protected fields (read-only, owner, timestamps, the soft-delete flag) cannot be
  forged: create strips them and applies defaults/stamps; `PUT` carries them over from the stored document (the new
  `keep` effect field); `PATCH` strips them from the patch. `updated` is re-stamped on every write.
* **soft_delete:** to the client it is an ordinary `DELETE` (204); the document stays stored with the flag and a
  `deleted` timestamp. A missing flag counts as "not deleted", so existing rows keep working when you turn it on.
* **effects templates:** `"${path}"` alone keeps the value's type (`"${body.qty}"` stays a number), `"${-body.qty}"`
  negates a number, inside text values are interpolated. Variables: `body.*`, `query.*`, `path.*`,
  `identity.subject|roles`, `id`, `route`, `request_id`, `now`. A value the request does not provide is a `400` that
  names the field. Rule effects run before script effects and the route's own write; a failing condition rolls
  everything back.
* **audit:** `{event: "<op> <route>", collection, docId, by, at, requestId}` per successful write; refused or
  rolled-back requests leave no row.
* Rules attach only to database-backed routes (checked at startup), are visible in `GET /api/v1/routes`
  (`data_rules`) and in the UI's routes table, and combine freely with hooks.

## Pasting source instead of building components: the builder

`stepwise-builder` (workspace crate `builder/`, with `docker/builder.Dockerfile`) is a separate
service that holds Node + `jco` and `componentize-py`, so the server needs no toolchains. Configure
`wasm.builder: {url, token}` and the control plane can take **source text**:

```
POST /api/v1/scripts/{name}/source      {"language":"typescript"|"javascript"|"python","source":"…","backend":"qjs"|"spidermonkey"}
GET  /api/v1/scripts/{name}/versions/{n}/source
GET  /api/v1/builder                    status + what it can build
GET  /api/v1/builder/templates/{lang}   starter source (known to compile)
GET  /api/v1/builder/typings            .d.ts for editor autocompletion
```

The result goes through the same validate → AOT-compile → store path as an uploaded component, and the
**source is stored with the version** (re-editable; identical source + settings returns the existing
version instead of rebuilding). Compiler errors come back as `422 build_failed` with the compiler
output; a JS/TS file that does not `export const handler` is rejected immediately with an explanation.
`?dry_run=true` builds and validates without storing.

Run it: `BUILDER_TOKEN=… BUILDER_JCO=jco BUILDER_COMPONENTIZE_PY=… BUILDER_WIT_DIR=./wit cargo run -p stepwise-builder`.
`docker/compose.yaml` wires builder + server on an internal network with no internet egress.
**Building executes your source** (inside Wasm, during pre-initialisation), so treat the builder as the
sandbox boundary: private network, no egress, memory/CPU limits, per-build timeouts (the compose file sets these).
JS/TS is transpiled, not type-checked; single-file sources only (no npm imports).

## The management UI

`ui/` (crate `stepwise-ui`, `docker/ui.Dockerfile`) is a no-build-step single-page app plus a small
backend that serves it and **proxies `/api/v1/*` to the control plane**. The browser only ever holds a
*UI* token; the control token stays in the proxy's environment, the proxy forwards a short allow-list of
headers, and sign-in guessing is throttled. Strict CSP (no inline script, no external resources), and
every piece of server text is inserted with `textContent` (no `innerHTML`), so hostile document or log
content renders as text.

```
STEPWISE_CONTROL_URL=http://127.0.0.1:9090 STEPWISE_CONTROL_TOKEN=… UI_TOKEN=$(openssl rand -hex 16) cargo run -p stepwise-ui
# open http://127.0.0.1:8081 and sign in with UI_TOKEN
```

Views: **Overview** (health, listeners, features, pending restarts) · **Routes** (inferred DB operation,
hook bindings per step with pinned digests; add/remove hooks, preview, apply) · **Scripts** (editor with
templates, check/build/save, versions, source, aliases, sandbox test panel, `.wasm` upload) · **Data**
(browse/filter/seed/clear) · **Config** (JSON editor, dry run, apply with `If-Match`, reload from disk).
Tests: `cargo test -p stepwise-ui` (proxy) and `ui/tests/e2e/run.sh` (the real page in jsdom against a
live server + builder; needs `npm install` there and the three binaries built).
The UI talks to the control plane over plain HTTP: keep them on a private network.

## Two listeners

| | Data plane | Control plane |
|---|---|---|
| Purpose | the API | config, scripts, data, metrics for your frontend |
| Default | `127.0.0.1:8080` | `127.0.0.1:9090` |
| Auth | per config (token / basic / JWT, roles) | bearer `control.token` (required), brute-force throttled |
| TLS / mTLS | `server.tls` | `control.tls` |
| Contract | your OpenAPI file | `openapi/control-plane.yaml` (served at `/api/v1/openapi.yaml`) |

A test fails if `control.rs` and `openapi/control-plane.yaml` drift apart.

## Changing config at runtime

`PUT /api/v1/config` (`?dry_run=true` to preview; `If-Match: <etag>`). The candidate is fully
compiled (spec, schemas, hook resolution) before anything changes; a bad config returns 422 and
nothing moves. Each change is classified `hot` / `restart_data_listener` /
`restart_control_listener` / `restart_process`. Failed listener re-binds restore the previous
listener. Accepted changes patch only the changed keys of your YAML, so `${ENV}` placeholders
survive and secrets are never written.

## Feature status

| Area | Status |
|---|---|
| OpenAPI 3.x, `$ref`s, request validation, schema-generated mocks | done |
| Step state machine, per-route hooks | done |
| SQLite CRUD inferred from the spec; hook DB access with grants; control-plane seed/browse/reset | done |
| Transactional effects from scripts, preconditions, atomic operators, versions/ETag/If-Match | done (store: tested on SQLite + PostgreSQL; server glue: type-checked, tests provided) |
| Declarative read engine: relations, projection, filters, ordering, paging; `query.run` for hooks, control-plane endpoint, UI query box | done (engine: tested on SQLite + PostgreSQL; surfaces: type-checked, tests provided) |
| No-code `data:` rules per route: owner scoping, soft delete, timestamps, defaults, read-only/hidden fields, templated effects, audit | done (executor: tested on SQLite + PostgreSQL; server wiring: type-checked, tests provided) |
| Public `/graphql` endpoint on the same engine | next |
| WASM hooks: JS/TS (QuickJS, SpiderMonkey) and Python verified; versions, aliases, pinning, test endpoint | done |
| Auth (API keys + roles, basic, JWT), `require_roles`, rate limiting, HTTPS/mTLS, security headers | done |
| Logging, access log, request IDs, Prometheus metrics (incl. per-hook), live config + persistence | done |
| PostgreSQL backend (jsonb, pooled, TLS, schema namespace) | written + type-checked; **tests provided, not yet run against a live server** |
| Management UI (overview, routes, scripts/editor, data, config) | done (proxy + jsdom end-to-end tested) |
| MySQL / MongoDB backends | later (implement `DocStore`) |
| Typed tables generated from schemas (instead of JSON documents) | later |
| Builder service: paste TS/JS/Python source, get a version (source kept, deduplicated, dry-run, templates, typings) | done |
| Rust guest toolchain verification; Rust source in the builder | next |
| WebSocket / webhooks, OAuth2/JWKS, OTLP traces, file-watch reload, hot cert rotation | later |

## Known limitations

- Saving config through the API rewrites the YAML: **comments are lost** (`.bak` kept).
- Data rules apply to the route's CRUD operation; they do not follow relations (a query via `query.run` or the control plane is not owner-scoped), and `owner` trusts the token's subject as the identity.
- The query language joins on document ids and foreign-key fields (no SQL joins); filtering on anything but plain equality scans (capped), and floats/null equality are decided in the engine, not the database.
- Nested resources (`/owners/{o}/pets`) use the last path segment as collection and are not scoped by parent; set `crud.collection` or write a hook.
- List returns a bare array; wrapper objects (`{items, total}`) need a `post_process` hook.
- A trapped hook shows testers a wasm backtrace, not the guest's own error text; guests should `log` or return `fail`.
- The builder talks plain HTTP (private network / TLS-terminating proxy). `docker/*` files were written but not built or run (no Docker in the dev sandbox); the builder itself was tested against the real toolchains.
- Wasmtime is pinned to 43.0.2 (newest whose MSRV fits Rust 1.91) with `component-model-async` enabled (required for jco-built components).
- The UI has one shared sign-in token (no per-user accounts or audit trail yet), no syntax highlighting, and is tested in jsdom, not in real browsers.
- Rate-limit state is per process; `trust_forwarded_for` only behind a trusted proxy; only JSON bodies validated.
- `database.*` and `wasm.enabled/data_dir/max_memory_mb/max_concurrent/allow_wasi` changes need a process restart.
