//! Build an immutable `Runtime` from (config, OpenAPI spec, hook registry).
//! Anything that can be wrong with a config is wrong *here*, at build time, so a
//! candidate config can be proven good before it replaces the live one.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use axum::http::Method;
use jsonschema::{Draft, JSONSchema};
use regex::Regex;
use serde_json::{Map, Value};

use crate::{
    auth::Authenticator,
    config::Config,
    db::{self, crud, DocStore},
    generate::{generate, GenOptions},
    hooks::HookRegistry,
    metrics::Metrics,
    pipeline::Step,
    ratelimit::{CompiledRate, RateLimiter},
    routes::*,
    spec::{load_spec, Resolver},
};

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

pub struct Runtime {
    pub config: Arc<Config>,
    pub table: RouteTable,
    pub authenticator: Authenticator,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
    pub db: Option<Arc<dyn DocStore>>,
    pub relations: Arc<db::query::Relations>,
    pub default_rate: Option<CompiledRate>,
    pub spec_title: String,
    pub spec_version: String,
}

pub fn build_runtime(
    config: Arc<Config>,
    registry: &HookRegistry,
    limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    db: Option<Arc<dyn DocStore>>,
) -> Result<Runtime> {
    config.validate()?;
    let spec = load_spec(Path::new(&config.openapi.spec_path))?;
    let resolver = Resolver::new(&spec);
    let authenticator = Authenticator::new(&config.auth)?;

    let ignore: Vec<Regex> = config
        .ignore_endpoints
        .iter()
        .map(|p| Regex::new(p).with_context(|| format!("bad ignore_endpoints regex `{p}`")))
        .collect::<Result<_>>()?;

    let default_rate = match (&config.rate_limit.enabled, &config.rate_limit.default) {
        (true, Some(p)) => Some(CompiledRate::new(p.clone(), "default")?),
        _ => None,
    };

    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .context("spec has no `paths`")?;
    let root_security = spec.get("security");
    let mut table = RouteTable::default();
    let mut used_overrides: BTreeSet<String> = BTreeSet::new();

    for (path_tpl, item_raw) in paths {
        if ignore.iter().any(|r| r.is_match(path_tpl)) {
            tracing::debug!(path = %path_tpl, "ignored by ignore_endpoints");
            continue;
        }
        let item = resolver.resolve(item_raw);
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        for m in METHODS {
            let Some(op) = item_obj.get(m).and_then(Value::as_object) else {
                continue;
            };
            let method = Method::from_bytes(m.to_uppercase().as_bytes())?;
            let id = format!("{} {}", method, path_tpl);
            let over = config.routes.get(&id);
            if over.is_some() {
                used_overrides.insert(id.clone());
            }
            let route = compile_route(
                &config,
                registry,
                id.clone(),
                method,
                path_tpl,
                item_obj,
                op,
                root_security,
            )
            .with_context(|| format!("compiling route `{id}`"))?;
            if let Err(e) = table.insert(route) {
                tracing::warn!(route = %id, error = %e, "route skipped (path conflicts with another route)");
            }
        }
    }

    for key in config.routes.keys() {
        if !used_overrides.contains(key) {
            bail!("routes: `{key}` does not match any (non-ignored) operation in the OpenAPI spec");
        }
    }

    let relations = Arc::new(config.relations.clone());
    Ok(Runtime {
        spec_title: spec
            .pointer("/info/title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        spec_version: spec
            .pointer("/info/version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        config,
        table,
        authenticator,
        limiter,
        metrics,
        db,
        relations,
        default_rate,
    })
}

#[allow(clippy::too_many_arguments)]
fn compile_route(
    config: &Config,
    registry: &HookRegistry,
    id: String,
    method: Method,
    path_tpl: &str,
    item: &Map<String, Value>,
    op: &Map<String, Value>,
    root_security: Option<&Value>,
) -> Result<CompiledRoute> {
    let over = config.routes.get(&id);

    // ---- auth / authorization
    let required_roles = over.map(|o| o.require_roles.clone()).unwrap_or_default();
    let mut protected = false;
    if config.auth.enabled && config.auth.auth_type != crate::config::AuthType::None {
        protected = match over.and_then(|o| o.auth) {
            Some(b) => b,
            None => {
                prefix_protected(&config.auth.protected_routes, path_tpl)
                    || (config.auth.use_openapi_security && operation_secured(op, root_security))
            }
        };
        if !required_roles.is_empty() {
            protected = true;
        }
    } else if !required_roles.is_empty() {
        bail!("`require_roles` is set but authentication is not enabled");
    }

    // ---- rate limit
    let rate = match over.and_then(|o| o.rate_limit.clone()) {
        Some(p) => Some(CompiledRate::new(p, id.clone())?),
        None => None,
    };

    // ---- parameters (path-level merged with operation-level; operation wins)
    let mut merged: BTreeMap<(String, String), &Value> = BTreeMap::new();
    for src in [item.get("parameters"), op.get("parameters")]
        .into_iter()
        .flatten()
    {
        for p in src.as_array().into_iter().flatten() {
            if let (Some(n), Some(l)) = (
                p.get("name").and_then(Value::as_str),
                p.get("in").and_then(Value::as_str),
            ) {
                merged.insert((n.to_string(), l.to_string()), p);
            }
        }
    }
    let mut params = Vec::new();
    for ((name, loc), p) in merged {
        let location = match loc.as_str() {
            "path" => ParamLocation::Path,
            "query" => ParamLocation::Query,
            "header" => ParamLocation::Header,
            "cookie" => ParamLocation::Cookie,
            _ => continue,
        };
        let schema = p.get("schema").cloned();
        let validator = schema
            .as_ref()
            .and_then(|s| compile_validator(s, &format!("{id} param {name}")));
        params.push(ParamSpec {
            required: location == ParamLocation::Path
                || p.get("required").and_then(Value::as_bool).unwrap_or(false),
            name,
            location,
            schema,
            validator,
        });
    }

    // ---- request body
    let request_body = op.get("requestBody").and_then(Value::as_object).map(|rb| {
        let schema = json_media(rb.get("content")).and_then(|m| m.get("schema"));
        BodySpec {
            required: rb.get("required").and_then(Value::as_bool).unwrap_or(false),
            validator: schema.and_then(|s| compile_validator(s, &format!("{id} requestBody"))),
        }
    });

    // ---- response
    let empty = Map::new();
    let responses = op
        .get("responses")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let (status, resp_obj) = pick_response(responses, over.and_then(|o| o.status))
        .with_context(|| format!("status override not documented in the spec for `{id}`"))?;
    let media = json_media(resp_obj.and_then(|r| r.get("content")));
    let schema = media.and_then(|m| m.get("schema"));
    let example = media.and_then(|m| {
        m.get("example").cloned().or_else(|| {
            m.get("examples")
                .and_then(Value::as_object)
                .and_then(|e| e.values().next())
                .and_then(|e| e.get("value").cloned())
        })
    });
    let mock = match (example, schema) {
        (Some(e), _) => Some(e),
        (None, Some(s)) if config.openapi.dynamic_responses.enabled => Some(generate(
            s,
            &GenOptions {
                array_items: config.openapi.dynamic_responses.array_items,
                ..Default::default()
            },
        )),
        _ => None,
    };
    let validator = schema.and_then(|s| compile_validator(s, &format!("{id} response {status}")));

    // ---- hooks
    let mut steps: HashMap<Step, StepPlan> = HashMap::new();
    if let Some(o) = over {
        for (step, sh) in &o.steps {
            let get = |name: &str| {
                registry
                    .resolve(name, &config.wasm)
                    .with_context(|| format!("hook `{name}` at step `{}`", step.as_str()))
            };
            let plan = StepPlan {
                before: sh.before.iter().map(|n| get(n)).collect::<Result<_>>()?,
                replace: sh.replace.as_deref().map(get).transpose()?,
                after: sh.after.iter().map(|n| get(n)).collect::<Result<_>>()?,
            };
            if !plan.is_empty() {
                steps.insert(*step, plan);
            }
        }
    }

    // ---- database: inferred CRUD + hook grants
    let mut grants: BTreeMap<String, db::Access> = BTreeMap::new();
    let mut crud_plan = None;
    if config.database.enabled {
        let last_param = path_tpl
            .rsplit('/')
            .next()
            .and_then(|s| s.strip_prefix('{'))
            .and_then(|s| s.strip_suffix('}'));
        let id_param_type = last_param
            .and_then(|n| params.iter().find(|p| p.name == n))
            .and_then(|p| p.schema.as_ref())
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str);
        let shape = crud::Shape {
            method: method.as_str(),
            path: path_tpl,
            has_body: request_body.is_some(),
            response_schema: schema,
            id_param_type,
        };
        crud_plan = crud::infer(&shape, &over.map(|o| o.crud.clone()).unwrap_or_default())?;
        if let Some(plan) = &crud_plan {
            grants.insert(
                plan.collection.clone(),
                db::Access {
                    read: true,
                    write: true,
                },
            );
        }
        for (coll, access) in over.map(|o| o.db.grants.clone()).unwrap_or_default() {
            grants.insert(coll, db::Access::parse(&access)?);
        }
    }

    let data = over.map(|o| o.data.clone()).unwrap_or_default();
    if !data.is_empty() {
        if crud_plan.is_none() {
            bail!("`data` rules need a database-backed route, but this operation has no CRUD (set `crud.operation`/`crud.collection` or remove the rules)");
        }
        if data.owner.is_some() && !protected {
            bail!("`data.owner` scopes data to the signed-in user, so the route must be authenticated (enable auth for it, or set `require_roles`)");
        }
    }

    Ok(CompiledRoute {
        id,
        method,
        path_template: path_tpl.to_string(),
        operation_id: op
            .get("operationId")
            .and_then(Value::as_str)
            .map(String::from),
        tags: op
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        protected,
        required_roles,
        rate,
        crud: crud_plan,
        data: Arc::new(data),
        db_grants: Arc::new(grants),
        params,
        request_body,
        response: ResponseSpec {
            status,
            mock,
            validator,
        },
        steps,
    })
}

fn prefix_protected(prefixes: &[String], path: &str) -> bool {
    prefixes.iter().any(|p| {
        let p = p.trim_end_matches('*').trim_end_matches('/');
        path == p || path.starts_with(&format!("{p}/"))
    })
}

/// `security: [{bearerAuth: []}]` protects; `security: []` or an empty requirement `{}` opts out.
fn operation_secured(op: &Map<String, Value>, root: Option<&Value>) -> bool {
    match op.get("security").or(root) {
        Some(Value::Array(a)) => {
            !a.is_empty()
                && !a
                    .iter()
                    .any(|r| r.as_object().is_some_and(|o| o.is_empty()))
        }
        _ => false,
    }
}

fn json_media(content: Option<&Value>) -> Option<&Value> {
    content?
        .as_object()?
        .iter()
        .find(|(k, _)| k.contains("json"))
        .map(|(_, v)| v)
}

fn pick_response(
    responses: &Map<String, Value>,
    forced: Option<u16>,
) -> Option<(u16, Option<&Value>)> {
    if let Some(s) = forced {
        return responses.get(&s.to_string()).map(|r| (s, Some(r)));
    }
    let numeric: Vec<(u16, &Value)> = responses
        .iter()
        .filter_map(|(k, v)| k.parse().ok().map(|c| (c, v)))
        .collect();
    if let Some((c, v)) = numeric
        .iter()
        .filter(|(c, _)| (200..300).contains(c))
        .min_by_key(|(c, _)| *c)
    {
        return Some((*c, Some(*v)));
    }
    if let Some(v) = responses.get("default") {
        return Some((200, Some(v)));
    }
    Some(
        numeric
            .iter()
            .min_by_key(|(c, _)| *c)
            .map(|(c, v)| (*c, Some(*v)))
            .unwrap_or((200, None)),
    )
}

fn compile_validator(schema: &Value, what: &str) -> Option<JSONSchema> {
    match JSONSchema::options()
        .with_draft(Draft::Draft7)
        .should_validate_formats(false)
        .compile(schema)
    {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(schema_for = what, error = %e, "schema could not be compiled; validation disabled for it");
            None
        }
    }
}
