//! The compiled, immutable route table. Everything expensive (ref resolution,
//! schema compilation, mock generation, hook binding) happens once at build time.

use std::{collections::BTreeMap, collections::HashMap, sync::Arc};

use axum::http::Method;
use jsonschema::JSONSchema;
use serde_json::Value;

use crate::{
    db::{crud::CrudPlan, Access},
    hooks::Hook,
    pipeline::Step,
    ratelimit::CompiledRate,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    Path,
    Query,
    Header,
    Cookie,
}

pub struct ParamSpec {
    pub name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub schema: Option<Value>,
    pub validator: Option<JSONSchema>,
}

pub struct BodySpec {
    pub required: bool,
    pub validator: Option<JSONSchema>,
}

pub struct ResponseSpec {
    pub status: u16,
    /// Precomputed mock body (example or generated). Cloned per request.
    pub mock: Option<Value>,
    pub validator: Option<JSONSchema>,
}

/// Hooks bound to one step: `before*` -> (`replace` | built-in) -> `after*`.
#[derive(Default, Clone)]
pub struct StepPlan {
    pub before: Vec<Arc<dyn Hook>>,
    pub replace: Option<Arc<dyn Hook>>,
    pub after: Vec<Arc<dyn Hook>>,
}

impl StepPlan {
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.replace.is_none() && self.after.is_empty()
    }
}

pub struct CompiledRoute {
    /// `"GET /pets/{petId}"`
    pub id: String,
    pub method: Method,
    pub path_template: String,
    pub operation_id: Option<String>,
    pub tags: Vec<String>,
    pub protected: bool,
    /// Caller needs at least one of these roles (empty = no role check).
    pub required_roles: Vec<String>,
    pub rate: Option<CompiledRate>,
    /// Database operation for `execute`, if the route looks like a resource (or is configured).
    pub crud: Option<CrudPlan>,
    /// No-code data rules (empty = none).
    pub data: Arc<crate::db::rules::DataRules>,
    /// Collections hooks on this route may access.
    pub db_grants: Arc<BTreeMap<String, Access>>,
    pub params: Vec<ParamSpec>,
    pub request_body: Option<BodySpec>,
    pub response: ResponseSpec,
    pub steps: HashMap<Step, StepPlan>,
}

pub enum Lookup {
    Found(Arc<CompiledRoute>, BTreeMap<String, String>),
    MethodNotAllowed(Vec<Method>),
    NotFound,
}

#[derive(Default)]
pub struct RouteTable {
    routers: HashMap<Method, matchit::Router<usize>>,
    routes: Vec<Arc<CompiledRoute>>,
}

impl RouteTable {
    pub fn insert(&mut self, route: CompiledRoute) -> Result<(), String> {
        let pattern = to_matchit(&route.path_template);
        let idx = self.routes.len();
        self.routers
            .entry(route.method.clone())
            .or_default()
            .insert(pattern, idx)
            .map_err(|e| e.to_string())?;
        self.routes.push(Arc::new(route));
        Ok(())
    }

    pub fn routes(&self) -> &[Arc<CompiledRoute>] {
        &self.routes
    }

    pub fn lookup(&self, method: &Method, path: &str) -> Lookup {
        let path = if path.len() > 1 {
            path.trim_end_matches('/')
        } else {
            path
        };
        if let Some(router) = self.routers.get(method) {
            if let Ok(m) = router.at(path) {
                let params = m
                    .params
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string(),
                            percent_encoding::percent_decode_str(v)
                                .decode_utf8_lossy()
                                .into_owned(),
                        )
                    })
                    .collect();
                return Lookup::Found(self.routes[*m.value].clone(), params);
            }
        }
        let mut allowed: Vec<Method> = self
            .routers
            .iter()
            .filter(|(m, r)| *m != method && r.at(path).is_ok())
            .map(|(m, _)| m.clone())
            .collect();
        if allowed.is_empty() {
            Lookup::NotFound
        } else {
            allowed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            Lookup::MethodNotAllowed(allowed)
        }
    }
}

/// `/pets/{petId}` -> `/pets/:petId` (matchit 0.7 syntax).
fn to_matchit(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                out.push(':');
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
}
