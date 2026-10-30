//! Uniform error type that every step and hook can fail with.

use axum::{
    body::Body,
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
    pub headers: Vec<(String, String)>,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
            headers: Vec::new(),
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn not_found() -> Self {
        Self::new(404, "not_found", "no route matches this path")
    }

    pub fn to_body(&self) -> Value {
        let mut e = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.details {
            e["details"] = d.clone();
        }
        json!({ "error": e })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = Response::new(Body::from(
            serde_json::to_vec(&self.to_body()).unwrap_or_default(),
        ));
        *resp.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        for (k, v) in &self.headers {
            if let (Ok(n), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                resp.headers_mut().append(n, val);
            }
        }
        resp
    }
}

impl From<crate::db::crud::CrudError> for ApiError {
    fn from(e: crate::db::crud::CrudError) -> Self {
        let mut api = ApiError::new(e.status, e.code, e.message);
        api.details = e.details;
        api.headers = e.headers;
        api
    }
}
