//! Every endpoint is a finite state machine over `Step::ALL`.
//! At each step: `before` hooks -> (`replace` hook | built-in) -> `after` hooks.
//! Any of them may `Continue`, `ShortCircuit` with a response, or `Fail`.

use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Instant};

use axum::{
    body::Body,
    http::{header, request::Parts, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    auth::{self, Identity},
    compile::Runtime,
    config::ResponseValidation,
    error::ApiError,
    routes::{CompiledRoute, ParamLocation},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    RateLimit,
    Authenticate,
    Authorize,
    ValidateRequest,
    PreProcess,
    Execute,
    PostProcess,
    ValidateResponse,
}

impl Step {
    pub const ALL: [Step; 8] = [
        Step::RateLimit,
        Step::Authenticate,
        Step::Authorize,
        Step::ValidateRequest,
        Step::PreProcess,
        Step::Execute,
        Step::PostProcess,
        Step::ValidateResponse,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Step::RateLimit => "rate_limit",
            Step::Authenticate => "authenticate",
            Step::Authorize => "authorize",
            Step::ValidateRequest => "validate_request",
            Step::PreProcess => "pre_process",
            Step::Execute => "execute",
            Step::PostProcess => "post_process",
            Step::ValidateResponse => "validate_response",
        }
    }
}

pub enum Outcome {
    Continue,
    /// Stop the machine and send this response as-is.
    ShortCircuit(ResponseDraft),
    Fail(ApiError),
}

#[derive(Debug, Clone)]
pub struct ResponseDraft {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl Default for ResponseDraft {
    fn default() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub step: Step,
    pub outcome: &'static str,
    pub micros: u128,
}

/// Writes queued by hooks. Nothing touches the database until the pipeline commits them.
#[derive(Debug, Default)]
pub struct Effects {
    /// Queued before `execute` ran: applied in the **same transaction** as the route's own write.
    pub in_tx: Vec<crate::db::Effect>,
    /// Queued after that write committed: applied afterwards, best effort, never changing the response.
    pub after_commit: Vec<crate::db::Effect>,
    /// Set once `execute` has committed; decides which list `queue` appends to.
    pub committed: bool,
}

impl Effects {
    pub fn queue(&mut self, e: crate::db::Effect) {
        if self.committed {
            self.after_commit.push(e);
        } else {
            self.in_tx.push(e);
        }
    }

    pub fn len(&self) -> usize {
        self.in_tx.len() + self.after_commit.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything a step or hook can see and change.
pub struct RequestContext {
    pub route: Arc<CompiledRoute>,
    pub method: Method,
    pub path: String,
    pub client_ip: IpAddr,
    pub request_id: String,
    pub path_params: BTreeMap<String, String>,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub raw_body: Bytes,
    /// Parsed JSON body, if the body was non-empty valid JSON.
    pub body: Option<Value>,
    pub body_error: Option<String>,
    pub identity: Option<Identity>,
    /// Scratch space shared between hooks within one request.
    pub state: serde_json::Map<String, Value>,
    pub response: ResponseDraft,
    pub error: Option<ApiError>,
    pub trace: Vec<TraceEntry>,
    /// The database, for hooks (they still need a grant per collection).
    pub db: Option<Arc<dyn crate::db::DocStore>>,
    /// Writes hooks have queued (see `Effects`).
    pub effects: Effects,
    /// Named relations for `query.run`.
    pub relations: Arc<crate::db::query::Relations>,
}

impl RequestContext {
    pub fn new(
        route: Arc<CompiledRoute>,
        parts: &Parts,
        path_params: BTreeMap<String, String>,
        raw_body: Bytes,
        client_ip: IpAddr,
        request_id: String,
    ) -> Self {
        let query = parts
            .uri
            .query()
            .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default();
        let (body, body_error) = if raw_body.is_empty() {
            (None, None)
        } else {
            match serde_json::from_slice::<Value>(&raw_body) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        Self {
            route,
            method: parts.method.clone(),
            path: parts.uri.path().to_string(),
            client_ip,
            request_id,
            path_params,
            query,
            headers: parts.headers.clone(),
            raw_body,
            body,
            body_error,
            identity: None,
            state: Default::default(),
            response: ResponseDraft::default(),
            error: None,
            trace: Vec::new(),
            db: None,
            effects: Effects::default(),
            relations: Arc::default(),
        }
    }

    pub fn into_response(self, include_trace: bool) -> Response {
        let (status, headers, body) = match &self.error {
            Some(e) => (e.status, e.headers.clone(), Some(e.to_body())),
            None => (
                self.response.status,
                self.response.headers.clone(),
                self.response.body.clone(),
            ),
        };
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() =
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        for (k, v) in headers {
            if let (Ok(n), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(&v),
            ) {
                resp.headers_mut().append(n, val);
            }
        }
        if let Some(b) = body {
            if !resp.headers().contains_key(header::CONTENT_TYPE) {
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            *resp.body_mut() = Body::from(serde_json::to_vec(&b).unwrap_or_default());
        }
        if include_trace {
            let t = self
                .trace
                .iter()
                .map(|t| format!("{}={}({}us)", t.step.as_str(), t.outcome, t.micros))
                .collect::<Vec<_>>()
                .join("; ");
            if let Ok(v) = HeaderValue::from_str(&t) {
                resp.headers_mut().insert("x-stepwise-trace", v);
            }
        }
        resp
    }
}

/// Run the state machine to completion. Result is left in `ctx.response` / `ctx.error`.
///
/// Writes queued by hooks follow one rule: effects queued before `execute` commit atomically with the
/// route's own database operation (or alone, if the route has none); effects queued afterwards are
/// applied once the response is decided, best effort. A failing request discards everything unapplied.
pub async fn run(rt: &Runtime, ctx: &mut RequestContext) {
    run_steps(rt, ctx).await;
    finish_effects(rt, ctx).await;
}

async fn run_steps(rt: &Runtime, ctx: &mut RequestContext) {
    let route = ctx.route.clone();
    for step in Step::ALL {
        let t0 = Instant::now();
        let plan = route.steps.get(&step);
        let mut outcome = Outcome::Continue;

        if let Some(p) = plan {
            for h in &p.before {
                outcome = h.call(step, ctx).await;
                if !matches!(outcome, Outcome::Continue) {
                    break;
                }
            }
        }
        if matches!(outcome, Outcome::Continue) {
            outcome = match plan.and_then(|p| p.replace.as_ref()) {
                Some(h) => h.call(step, ctx).await,
                None => builtin(rt, step, ctx).await,
            };
            if step == Step::Execute && matches!(outcome, Outcome::Continue) {
                // covers mock responses and `replace` hooks; the CRUD executor has already committed
                outcome = commit_pending(rt, ctx).await;
            }
        }
        if matches!(outcome, Outcome::Continue) {
            if let Some(p) = plan {
                for h in &p.after {
                    outcome = h.call(step, ctx).await;
                    if !matches!(outcome, Outcome::Continue) {
                        break;
                    }
                }
            }
        }

        let label = match outcome {
            Outcome::Continue => "ok",
            Outcome::ShortCircuit(_) => "short-circuit",
            Outcome::Fail(_) => "fail",
        };
        ctx.trace.push(TraceEntry {
            step,
            outcome: label,
            micros: t0.elapsed().as_micros(),
        });
        match outcome {
            Outcome::Continue => {}
            Outcome::ShortCircuit(r) => {
                // a hook that answers early still gets the writes it queued (one transaction, no route write)
                if let Outcome::Fail(e) = commit_pending(rt, ctx).await {
                    ctx.error = Some(e);
                    return;
                }
                ctx.response = r;
                return;
            }
            Outcome::Fail(e) => {
                ctx.error = Some(e);
                return;
            }
        }
    }
}

/// Apply the effects queued so far in one transaction (no-op if there are none).
async fn commit_pending(rt: &Runtime, ctx: &mut RequestContext) -> Outcome {
    if ctx.effects.in_tx.is_empty() {
        ctx.effects.committed = true;
        return Outcome::Continue;
    }
    let plan = std::mem::take(&mut ctx.effects.in_tx);
    let n = plan.len() as u64;
    let Some(db) = &rt.db else {
        return Outcome::Fail(ApiError::new(
            500,
            "database_unavailable",
            "a hook queued a write but the database is not enabled",
        ));
    };
    match db.apply(&plan).await {
        Ok(_) => {
            rt.metrics.add_effects("in_tx_committed", n);
            ctx.effects.committed = true;
            Outcome::Continue
        }
        Err(e) => {
            rt.metrics.add_effects("in_tx_rolled_back", n);
            Outcome::Fail(crate::db::crud::http_error(&e).into())
        }
    }
}

/// Effects queued after the route's write committed: applied now, never affecting the response.
async fn finish_effects(rt: &Runtime, ctx: &mut RequestContext) {
    if ctx.error.is_some() || ctx.effects.after_commit.is_empty() {
        return;
    }
    let plan = std::mem::take(&mut ctx.effects.after_commit);
    let (n, Some(db)) = (plan.len() as u64, &rt.db) else {
        return;
    };
    match db.apply(&plan).await {
        Ok(_) => rt.metrics.add_effects("after_commit_ok", n),
        Err(e) => {
            rt.metrics.add_effects("after_commit_failed", n);
            tracing::warn!(route = %ctx.route.id, request_id = %ctx.request_id, error = %e, "after-commit effects failed (response unaffected)");
        }
    }
}

async fn builtin(rt: &Runtime, step: Step, ctx: &mut RequestContext) -> Outcome {
    match step {
        Step::RateLimit => rate_limit(rt, ctx),
        Step::Authenticate => {
            if !ctx.route.protected {
                return Outcome::Continue;
            }
            match rt.authenticator.authenticate(&ctx.headers) {
                Ok(id) => {
                    ctx.identity = Some(id);
                    Outcome::Continue
                }
                Err(e) => {
                    rt.metrics.inc_auth_failure();
                    Outcome::Fail(e)
                }
            }
        }
        Step::Authorize => {
            let need = &ctx.route.required_roles;
            if need.is_empty() {
                return Outcome::Continue;
            }
            match &ctx.identity {
                Some(id) if id.roles.iter().any(|r| need.contains(r)) => Outcome::Continue,
                Some(_) => Outcome::Fail(
                    ApiError::new(403, "forbidden", "insufficient role for this operation")
                        .with_details(json!({ "required_any_of": need })),
                ),
                None => Outcome::Fail(ApiError::new(
                    401,
                    "missing_token",
                    "authentication required",
                )),
            }
        }
        Step::PreProcess | Step::PostProcess => Outcome::Continue,
        Step::ValidateRequest => validate_request(rt, ctx),
        Step::Execute => {
            let route = ctx.route.clone();
            match (&route.crud, &rt.db) {
                (Some(plan), Some(db)) => {
                    let pending = std::mem::take(&mut ctx.effects.in_tx);
                    let n = pending.len() as u64;
                    let declared: Vec<&str> = route
                        .params
                        .iter()
                        .filter(|p| p.location == ParamLocation::Query)
                        .map(|p| p.name.as_str())
                        .collect();
                    let vars = if route.data.is_empty() {
                        Value::Null
                    } else {
                        let mut query = serde_json::Map::new();
                        for (k, v) in &ctx.query {
                            query.insert(k.clone(), Value::String(v.clone()));
                        }
                        json!({
                            "body": ctx.body.clone().unwrap_or(Value::Null),
                            "query": query,
                            "path": ctx.path_params,
                            "identity": { "subject": ctx.identity.as_ref().map(|i| i.subject.clone()), "roles": ctx.identity.as_ref().map(|i| i.roles.clone()).unwrap_or_default() },
                            "request_id": ctx.request_id,
                            "route": route.id,
                        })
                    };
                    let input = crate::db::crud::CrudInput {
                        path_id: plan
                            .id_param
                            .as_deref()
                            .and_then(|p| ctx.path_params.get(p))
                            .map(String::as_str),
                        body: ctx.body.as_ref(),
                        query: &ctx.query,
                        declared_query: &declared,
                        if_match: ctx
                            .headers
                            .get(header::IF_MATCH)
                            .and_then(|v| v.to_str().ok()),
                        path: &ctx.path,
                        rules: (!route.data.is_empty()).then(|| crate::db::crud::RuleInput {
                            rules: &route.data,
                            subject: ctx.identity.as_ref().map(|i| i.subject.as_str()),
                            vars: &vars,
                        }),
                    };
                    match crate::db::crud::run(db.as_ref(), plan, &input, pending).await {
                        Ok(out) => {
                            rt.metrics.add_effects("in_tx_committed", n);
                            ctx.response.status = route.response.status;
                            ctx.response.body = out.body;
                            ctx.response.headers.extend(out.headers);
                            ctx.effects.committed = true;
                            Outcome::Continue
                        }
                        Err(e) => {
                            rt.metrics.add_effects("in_tx_rolled_back", n);
                            Outcome::Fail(e.into())
                        }
                    }
                }
                _ => {
                    ctx.response.status = route.response.status;
                    ctx.response.body = route.response.mock.clone();
                    Outcome::Continue
                }
            }
        }
        Step::ValidateResponse => validate_response(rt, ctx),
    }
}

fn rate_limit(rt: &Runtime, ctx: &mut RequestContext) -> Outcome {
    use crate::ratelimit::RateKey;
    let Some(rate) = ctx.route.rate.as_ref().or(rt.default_rate.as_ref()) else {
        return Outcome::Continue;
    };
    let who = match &rate.key {
        RateKey::Ip => ctx.client_ip.to_string(),
        RateKey::Global => "*".to_string(),
        RateKey::Header(h) => ctx
            .headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .unwrap_or_else(|| ctx.client_ip.to_string()),
    };
    let d = rt
        .limiter
        .check(&format!("{}|{}", rate.scope, who), &rate.policy);
    let limit = ("x-ratelimit-limit".to_string(), d.limit.to_string());
    let remaining = ("x-ratelimit-remaining".to_string(), d.remaining.to_string());
    if d.allowed {
        ctx.response.headers.push(limit);
        ctx.response.headers.push(remaining);
        return Outcome::Continue;
    }
    rt.metrics.inc_rate_limited(&ctx.route.id);
    let mut e = ApiError::new(429, "rate_limited", "too many requests")
        .with_header("retry-after", d.retry_after_secs.to_string());
    e.headers.push(limit);
    e.headers.push(remaining);
    Outcome::Fail(e)
}

fn validate_request(rt: &Runtime, ctx: &mut RequestContext) -> Outcome {
    if !rt.config.openapi.validation.enabled {
        return Outcome::Continue;
    }
    let route = ctx.route.clone();
    let mut problems: Vec<Value> = Vec::new();

    for p in &route.params {
        let raw: Option<String> = match p.location {
            ParamLocation::Path => ctx.path_params.get(&p.name).cloned(),
            ParamLocation::Query => ctx
                .query
                .iter()
                .find(|(k, _)| *k == p.name)
                .map(|(_, v)| v.clone()),
            ParamLocation::Header => ctx
                .headers
                .get(p.name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            ParamLocation::Cookie => auth::cookie_value(&ctx.headers, &p.name),
        };
        let loc = format!("{:?}", p.location).to_lowercase();
        match raw {
            None if p.required => {
                problems.push(
                    json!({"in": loc, "name": p.name, "message": "required parameter is missing"}),
                );
            }
            None => {}
            Some(raw) => {
                if let Some(v) = &p.validator {
                    let inst = coerce(p.schema.as_ref(), &raw);
                    let msgs: Vec<String> = match v.validate(&inst) {
                        Ok(()) => vec![],
                        Err(errs) => errs.map(|e| e.to_string()).collect(),
                    };
                    for m in msgs {
                        problems.push(json!({"in": loc, "name": p.name, "message": m}));
                    }
                }
            }
        }
    }
    if !problems.is_empty() {
        return Outcome::Fail(
            ApiError::new(
                400,
                "invalid_parameters",
                "request parameters do not match the API spec",
            )
            .with_details(Value::Array(problems)),
        );
    }

    if let Some(spec) = &route.request_body {
        if ctx.raw_body.is_empty() {
            if spec.required {
                return Outcome::Fail(ApiError::new(
                    400,
                    "body_required",
                    "a request body is required",
                ));
            }
        } else if let Some(err) = &ctx.body_error {
            return Outcome::Fail(ApiError::new(
                400,
                "malformed_json",
                format!("body is not valid JSON: {err}"),
            ));
        } else if let (Some(v), Some(body)) = (&spec.validator, &ctx.body) {
            if let Err(errs) = v.validate(body) {
                let details: Vec<Value> = errs
                    .map(|e| json!({"path": e.instance_path.to_string(), "message": e.to_string()}))
                    .collect();
                return Outcome::Fail(
                    ApiError::new(
                        422,
                        "validation_failed",
                        "request body does not match the API spec",
                    )
                    .with_details(Value::Array(details)),
                );
            }
        }
    }
    Outcome::Continue
}

fn validate_response(rt: &Runtime, ctx: &mut RequestContext) -> Outcome {
    let mode = rt.config.openapi.validation.responses;
    if mode == ResponseValidation::Off {
        return Outcome::Continue;
    }
    let (Some(v), Some(body)) = (&ctx.route.response.validator, &ctx.response.body) else {
        return Outcome::Continue;
    };
    if let Err(errs) = v.validate(body) {
        let details: Vec<Value> = errs
            .map(|e| json!({"path": e.instance_path.to_string(), "message": e.to_string()}))
            .collect();
        if mode == ResponseValidation::Error {
            return Outcome::Fail(
                ApiError::new(
                    500,
                    "response_contract_violation",
                    "response does not match the API spec",
                )
                .with_details(Value::Array(details)),
            );
        }
        tracing::warn!(route = %ctx.route.id, ?details, "response does not match the API spec");
    }
    Outcome::Continue
}

/// Query/path/header values arrive as strings; coerce by declared schema type before validating.
fn coerce(schema: Option<&Value>, raw: &str) -> Value {
    let ty = schema.and_then(|s| s.get("type")).and_then(|t| match t {
        Value::String(s) => Some(s.as_str()),
        Value::Array(a) => a.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    });
    match ty {
        Some("integer") => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.into())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.into())),
        Some("boolean") => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.into()),
        },
        Some("array") => {
            let items = schema.and_then(|s| s.get("items"));
            Value::Array(raw.split(',').map(|p| coerce(items, p)).collect())
        }
        _ => Value::String(raw.into()),
    }
}
