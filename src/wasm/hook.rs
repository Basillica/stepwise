//! `WasmHook`: runs a guest component at a pipeline step and *polices what it hands back*.
//!
//! The guest is untrusted. Everything it returns is parsed and checked before it touches the
//! request context: JSON must be valid, status codes sane, headers legal, and a patched
//! request body must still satisfy the route's OpenAPI schema. Violations never reach the
//! client or the database; they become a 500 `hook_contract_violation` that names the hook.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use bytes::Bytes;
use serde_json::{json, Value};

use super::{
    bindings::stepwise::hook::types::{
        Failure, Kv, Outcome as Wo, Patch, Response as WResponse, ResponseDraft as WDraft,
    },
    runtime::{CallFailure, CallMeta, LoadedModule, LogLine, WasmRuntime, WitContext},
    store::Resolved,
};
use crate::{
    error::ApiError,
    hooks::Hook,
    pipeline::{Outcome, RequestContext, ResponseDraft, Step},
};

/// Headers a guest may not set: they would corrupt framing or the connection.
const FORBIDDEN_RESPONSE_HEADERS: [&str; 8] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "host",
    "upgrade",
    "keep-alive",
    "te",
    "trailer",
];
/// Never handed to guests unless `wasm.expose_credentials` is on.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];

pub struct WasmHook {
    pub reference: String,
    pub resolved: Resolved,
    pub module: Arc<LoadedModule>,
    pub runtime: Arc<WasmRuntime>,
    pub timeout: Duration,
    pub max_payload: usize,
    pub expose_credentials: bool,
}

/// Everything a single invocation produced; the control-plane test endpoint reports all of it.
pub struct HookRun {
    pub outcome: Outcome,
    /// ok | respond | fail | timeout | trap | busy | contract
    pub label: &'static str,
    pub logs: Vec<LogLine>,
    pub elapsed: Duration,
    /// Internal detail (trap message, contract problem) — logged, and shown only to the tester.
    pub detail: Option<String>,
}

#[async_trait]
impl Hook for WasmHook {
    fn name(&self) -> &str {
        &self.reference
    }

    fn pinned(&self) -> Option<String> {
        Some(self.resolved.digest.clone())
    }

    async fn call(&self, step: Step, ctx: &mut RequestContext) -> Outcome {
        self.run(step, ctx).await.outcome
    }
}

impl WasmHook {
    pub async fn run(&self, step: Step, ctx: &mut RequestContext) -> HookRun {
        let input = match self.build_input(step, ctx) {
            Ok(i) => i,
            Err(problem) => return self.violation(problem, vec![], Duration::ZERO),
        };
        let meta = CallMeta {
            hook: self.reference.as_str().into(),
            route: ctx.route.id.as_str().into(),
            request_id: ctx.request_id.as_str().into(),
        };
        let db = ctx
            .db
            .clone()
            .filter(|_| !ctx.route.db_grants.is_empty())
            .map(|store| crate::db::DbAccess {
                store,
                grants: ctx.route.db_grants.clone(),
                route: ctx.route.id.as_str().into(),
                max_payload: self.max_payload,
                relations: ctx.relations.clone(),
            });
        let res = self
            .runtime
            .invoke(&self.module, &input, self.timeout, meta, db)
            .await;
        let run = match res.outcome {
            Ok(wo) => match self.apply(step, ctx, wo) {
                Ok((outcome, label)) => {
                    // Writes are only kept if the hook let the request carry on (or answered it);
                    // a hook that returns `fail` writes nothing.
                    if matches!(label, "ok" | "respond") && !res.effects.is_empty() {
                        if ctx.effects.len() + res.effects.len()
                            > crate::db::effects::MAX_EFFECTS_PER_PLAN
                        {
                            return self.violation(
                                format!(
                                    "a request may queue at most {} effects in total",
                                    crate::db::effects::MAX_EFFECTS_PER_PLAN
                                ),
                                res.logs,
                                res.elapsed,
                            );
                        }
                        for e in res.effects {
                            ctx.effects.queue(e);
                        }
                    }
                    HookRun {
                        outcome,
                        label,
                        logs: res.logs,
                        elapsed: res.elapsed,
                        detail: None,
                    }
                }
                Err(problem) => self.violation(problem, res.logs, res.elapsed),
            },
            Err(CallFailure::Timeout) => self.failure(
                ApiError::new(504, "hook_timeout", "a hook exceeded its time budget"),
                "timeout",
                Some(format!("exceeded {} ms", self.timeout.as_millis())),
                res.logs,
                res.elapsed,
            ),
            Err(CallFailure::Busy) => self.failure(
                ApiError::new(503, "hook_busy", "too many hooks running; try again")
                    .with_header("retry-after", "1"),
                "busy",
                None,
                res.logs,
                res.elapsed,
            ),
            Err(CallFailure::Trap(msg)) => self.failure(
                ApiError::new(
                    500,
                    "hook_failed",
                    "a hook failed while handling this request",
                ),
                "trap",
                Some(msg),
                res.logs,
                res.elapsed,
            ),
        };
        self.runtime
            .metrics
            .observe_hook(&self.reference, run.label, run.elapsed.as_secs_f64());
        if let Some(d) = &run.detail {
            tracing::error!(hook = %self.reference, route = %ctx.route.id, request_id = %ctx.request_id, result = run.label, detail = %d, "hook did not complete normally");
        }
        run
    }

    fn violation(&self, problem: String, logs: Vec<LogLine>, elapsed: Duration) -> HookRun {
        let err = ApiError::new(
            500,
            "hook_contract_violation",
            "a hook produced invalid output",
        )
        .with_details(json!({ "hook": self.reference, "problem": problem }));
        self.failure(err, "contract", Some(problem), logs, elapsed)
    }

    fn failure(
        &self,
        err: ApiError,
        label: &'static str,
        detail: Option<String>,
        logs: Vec<LogLine>,
        elapsed: Duration,
    ) -> HookRun {
        HookRun {
            outcome: Outcome::Fail(err),
            label,
            logs,
            elapsed,
            detail,
        }
    }

    // ------------------------------------------------------------------ host -> guest

    fn build_input(&self, step: Step, ctx: &RequestContext) -> Result<WitContext, String> {
        let kvs = |it: &mut dyn Iterator<Item = (String, String)>| {
            it.map(|(name, value)| Kv { name, value })
                .collect::<Vec<_>>()
        };
        let headers = kvs(&mut ctx.headers.iter().filter_map(|(k, v)| {
            let name = k.as_str();
            if !self.expose_credentials && CREDENTIAL_HEADERS.contains(&name) {
                return None;
            }
            v.to_str().ok().map(|v| (name.to_string(), v.to_string()))
        }));
        let text = |v: &Value| v.to_string();
        let ctx_in = WitContext {
            request_id: ctx.request_id.clone(),
            route_id: ctx.route.id.clone(),
            step: step.as_str().to_string(),
            method: ctx.method.as_str().to_string(),
            path: ctx.path.clone(),
            path_params: kvs(&mut ctx.path_params.iter().map(|(k, v)| (k.clone(), v.clone()))),
            query: kvs(&mut ctx.query.iter().cloned()),
            headers,
            request_body: ctx.body.as_ref().map(text),
            identity: ctx
                .identity
                .as_ref()
                .and_then(|i| serde_json::to_string(i).ok()),
            state: Value::Object(ctx.state.clone()).to_string(),
            response: WDraft {
                status: ctx.response.status,
                headers: kvs(&mut ctx.response.headers.iter().cloned()),
                body: ctx.response.body.as_ref().map(text),
            },
        };
        let total = ctx_in.state.len()
            + ctx_in.request_body.as_ref().map_or(0, String::len)
            + ctx_in.response.body.as_ref().map_or(0, String::len);
        if total > self.max_payload {
            return Err(format!(
                "context payload is {total} bytes, over wasm.max_payload_bytes ({})",
                self.max_payload
            ));
        }
        Ok(ctx_in)
    }

    // ------------------------------------------------------------------ guest -> host

    fn json_text(&self, what: &str, s: &str) -> Result<Value, String> {
        if s.len() > self.max_payload {
            return Err(format!(
                "{what} is {} bytes, over wasm.max_payload_bytes ({})",
                s.len(),
                self.max_payload
            ));
        }
        serde_json::from_str(s).map_err(|e| format!("{what} is not valid JSON: {e}"))
    }

    fn headers(&self, what: &str, hs: Vec<Kv>) -> Result<Vec<(String, String)>, String> {
        hs.into_iter()
            .map(|Kv { name, value }| {
                let lname = name.to_ascii_lowercase();
                HeaderName::from_bytes(lname.as_bytes())
                    .map_err(|_| format!("{what}: `{name}` is not a valid header name"))?;
                HeaderValue::from_str(&value).map_err(|_| {
                    format!("{what}: value of `{name}` is not a valid header value")
                })?;
                if FORBIDDEN_RESPONSE_HEADERS.contains(&lname.as_str()) {
                    return Err(format!("{what}: hooks may not set `{lname}`"));
                }
                Ok((lname, value))
            })
            .collect()
    }

    fn status(&self, what: &str, s: u16, min: u16) -> Result<u16, String> {
        if (min..=599).contains(&s) {
            Ok(s)
        } else {
            Err(format!("{what}: status {s} is outside {min}..=599"))
        }
    }

    /// Validate the guest's outcome and apply it to the context.
    fn apply(
        &self,
        step: Step,
        ctx: &mut RequestContext,
        wo: Wo,
    ) -> Result<(Outcome, &'static str), String> {
        match wo {
            Wo::Proceed(p) => {
                self.apply_patch(step, ctx, p)?;
                Ok((Outcome::Continue, "ok"))
            }
            Wo::Respond(WResponse {
                status,
                headers,
                body,
            }) => {
                let status = self.status("respond", status, 100)?;
                Ok((
                    Outcome::ShortCircuit(ResponseDraft {
                        status,
                        headers: self.headers("respond", headers)?,
                        body: body
                            .as_deref()
                            .map(|b| self.json_text("respond.body", b))
                            .transpose()?,
                    }),
                    "respond",
                ))
            }
            Wo::Fail(Failure {
                status,
                code,
                message,
                details,
            }) => {
                let status = self.status("fail", status, 400)?;
                if code.is_empty() || code.len() > 64 {
                    return Err("fail: `code` must be 1-64 characters".into());
                }
                let mut e =
                    ApiError::new(status, code, message.chars().take(1024).collect::<String>());
                if let Some(d) = details {
                    e = e.with_details(
                        self.json_text("fail.details", &d)
                            .unwrap_or(Value::String(d)),
                    );
                }
                Ok((Outcome::Fail(e), "fail"))
            }
        }
    }

    fn apply_patch(&self, step: Step, ctx: &mut RequestContext, p: Patch) -> Result<(), String> {
        // Validate everything first so a bad patch changes nothing.
        let state = match &p.state {
            Some(s) => match self.json_text("patch.state", s)? {
                Value::Object(m) => Some(m),
                _ => return Err("patch.state must be a JSON object".into()),
            },
            None => None,
        };
        let request_body = match &p.request_body {
            Some(s) => {
                if step > Step::Execute {
                    return Err(format!("patch.request-body is not allowed at step `{}` (the request has already been handled)", step.as_str()));
                }
                let v = self.json_text("patch.request-body", s)?;
                if let Some(validator) = ctx
                    .route
                    .request_body
                    .as_ref()
                    .and_then(|b| b.validator.as_ref())
                {
                    let problems: Vec<String> = match validator.validate(&v) {
                        Ok(()) => vec![],
                        Err(errs) => errs.map(|e| format!("{}: {e}", e.instance_path)).collect(),
                    };
                    if !problems.is_empty() {
                        return Err(format!(
                            "patched request body violates the route's schema: {}",
                            problems.join("; ")
                        ));
                    }
                }
                Some(v)
            }
            None => None,
        };
        let status = p
            .response_status
            .map(|s| self.status("patch.response-status", s, 100))
            .transpose()?;
        let headers = p
            .response_headers
            .map(|h| self.headers("patch.response-headers", h))
            .transpose()?;
        let response_body = p
            .response_body
            .as_deref()
            .map(|b| self.json_text("patch.response-body", b))
            .transpose()?;

        if let Some(m) = state {
            ctx.state = m;
        }
        if let Some(v) = request_body {
            ctx.raw_body = Bytes::from(serde_json::to_vec(&v).unwrap_or_default());
            ctx.body = Some(v);
        }
        if let Some(s) = status {
            ctx.response.status = s;
        }
        if let Some(h) = headers {
            ctx.response.headers = h;
        }
        if let Some(b) = response_body {
            ctx.response.body = Some(b);
        }
        Ok(())
    }
}
