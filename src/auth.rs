//! Authentication schemes: static API keys/bearer tokens, basic, and JWT.
//! Authorization (roles) is enforced in the `authorize` step.

use axum::http::{header, HeaderMap};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{errors::ErrorKind, Algorithm, DecodingKey, Validation};
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::{AuthConfig, AuthType, TokenConfig, TokenSource},
    error::ApiError,
};

#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub subject: String,
    pub roles: Vec<String>,
}

struct Jwt {
    key: DecodingKey,
    validation: Validation,
    roles_claim: String,
}

/// Built once per config version: PEM keys are parsed here, never per request.
pub struct Authenticator {
    cfg: AuthConfig,
    jwt: Option<Jwt>,
}

impl Authenticator {
    pub fn new(cfg: &AuthConfig) -> anyhow::Result<Self> {
        let jwt = if cfg.enabled && cfg.auth_type == AuthType::Jwt {
            Some(build_jwt(cfg)?)
        } else {
            None
        };
        Ok(Self {
            cfg: cfg.clone(),
            jwt,
        })
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Identity, ApiError> {
        let cfg = &self.cfg;
        match cfg.auth_type {
            AuthType::None => Ok(Identity {
                subject: "anonymous".into(),
                roles: vec![],
            }),
            AuthType::Token => {
                let presented = credential(&cfg.token, headers)
                    .ok_or_else(|| bearer_error(401, "missing_token", "authentication required"))?;
                // Compare against every key (no early exit) to keep timing independent of which key matched.
                let mut hit: Option<Identity> = None;
                let single = (!cfg.token.value.is_empty()).then(|| crate::config::ApiKey {
                    value: cfg.token.value.clone(),
                    subject: "token".into(),
                    roles: vec![],
                });
                for k in single.iter().chain(cfg.token.keys.iter()) {
                    if ct_eq(presented.as_bytes(), k.value.as_bytes()) && !k.value.is_empty() {
                        hit = Some(Identity {
                            subject: if k.subject.is_empty() {
                                "token".into()
                            } else {
                                k.subject.clone()
                            },
                            roles: k.roles.clone(),
                        });
                    }
                }
                hit.ok_or_else(|| {
                    bearer_error(401, "invalid_token", "the supplied token is not valid")
                })
            }
            AuthType::Basic => {
                let challenge = |code: &str, msg: &str| {
                    ApiError::new(401, code, msg)
                        .with_header("www-authenticate", "Basic realm=\"stepwise\"")
                };
                let raw = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| strip_prefix_ci(v, "basic "))
                    .ok_or_else(|| challenge("missing_credentials", "authentication required"))?;
                let decoded = STANDARD
                    .decode(raw.trim())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or_else(|| {
                        challenge("invalid_credentials", "malformed basic credentials")
                    })?;
                let (user, pass) = decoded.split_once(':').ok_or_else(|| {
                    challenge("invalid_credentials", "malformed basic credentials")
                })?;
                if ct_eq(user.as_bytes(), cfg.basic.username.as_bytes())
                    & ct_eq(pass.as_bytes(), cfg.basic.password.as_bytes())
                {
                    Ok(Identity {
                        subject: user.to_string(),
                        roles: cfg.basic.roles.clone(),
                    })
                } else {
                    Err(challenge(
                        "invalid_credentials",
                        "wrong username or password",
                    ))
                }
            }
            AuthType::Jwt => {
                let jwt = self.jwt.as_ref().ok_or_else(|| {
                    ApiError::new(500, "auth_misconfigured", "jwt not initialised")
                })?;
                let token = credential(&cfg.token, headers)
                    .ok_or_else(|| bearer_error(401, "missing_token", "authentication required"))?;
                let data = jsonwebtoken::decode::<Value>(&token, &jwt.key, &jwt.validation)
                    .map_err(|e| match e.kind() {
                        ErrorKind::ExpiredSignature => {
                            bearer_error(401, "token_expired", "token has expired")
                        }
                        _ => bearer_error(401, "invalid_token", "token verification failed"),
                    })?;
                let claims = data.claims;
                let subject = claims
                    .get("sub")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let roles = match claims.get(&jwt.roles_claim) {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect(),
                    Some(Value::String(s)) => s.split_whitespace().map(String::from).collect(),
                    _ => vec![],
                };
                Ok(Identity { subject, roles })
            }
            AuthType::Oauth2 => Err(ApiError::new(
                501,
                "unsupported_auth",
                "oauth2 is not supported yet",
            )),
        }
    }
}

fn build_jwt(cfg: &AuthConfig) -> anyhow::Result<Jwt> {
    use anyhow::Context;
    let j = &cfg.jwt;
    let alg: Algorithm = j
        .algorithm
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown JWT algorithm `{}`", j.algorithm))?;
    let key = match alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            DecodingKey::from_secret(j.secret.as_bytes())
        }
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            DecodingKey::from_rsa_pem(&read_key(j.public_key_path.as_deref())?)
                .context("bad RSA public key PEM")?
        }
        Algorithm::ES256 | Algorithm::ES384 => {
            DecodingKey::from_ec_pem(&read_key(j.public_key_path.as_deref())?)
                .context("bad EC public key PEM")?
        }
        other => anyhow::bail!("JWT algorithm {other:?} is not supported"),
    };
    let mut validation = Validation::new(alg);
    validation.leeway = j.leeway_secs;
    if let Some(iss) = &j.issuer {
        validation.set_issuer(&[iss]);
    }
    if j.audience.is_empty() {
        validation.validate_aud = false;
    } else {
        validation.set_audience(&j.audience);
    }
    Ok(Jwt {
        key,
        validation,
        roles_claim: j.roles_claim.clone(),
    })
}

fn read_key(path: Option<&str>) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    let p = path.context("auth.jwt.public_key_path is required")?;
    std::fs::read(p).with_context(|| format!("cannot read JWT public key {p}"))
}

fn bearer_error(status: u16, code: &str, msg: &str) -> ApiError {
    ApiError::new(status, code, msg)
        .with_header("www-authenticate", format!("Bearer error=\"{code}\""))
}

fn credential(t: &TokenConfig, headers: &HeaderMap) -> Option<String> {
    match t.source {
        TokenSource::Header => headers.get(&t.name).and_then(|v| v.to_str().ok()).map(|v| {
            let v = if t.name.eq_ignore_ascii_case("authorization") {
                strip_prefix_ci(v, "bearer ").unwrap_or(v)
            } else {
                v
            };
            v.trim().to_string()
        }),
        TokenSource::Cookie => cookie_value(headers, &t.name),
    }
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix))
    .then(|| &s[prefix.len()..])
}

/// Constant-time comparison (length is not secret here).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKey, JwtConfig};
    use axum::http::HeaderValue;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn hdr(name: &'static str, v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn multi_key_tokens_carry_roles() {
        let cfg = AuthConfig {
            enabled: true,
            auth_type: AuthType::Token,
            token: TokenConfig {
                keys: vec![
                    ApiKey {
                        value: "k1".into(),
                        subject: "alice".into(),
                        roles: vec!["admin".into()],
                    },
                    ApiKey {
                        value: "k2".into(),
                        subject: "bob".into(),
                        roles: vec![],
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let a = Authenticator::new(&cfg).unwrap();
        let id = a.authenticate(&hdr("authorization", "Bearer k1")).unwrap();
        assert_eq!(
            (id.subject.as_str(), id.roles.as_slice()),
            ("alice", &["admin".to_string()][..])
        );
        assert_eq!(
            a.authenticate(&hdr("authorization", "bearer k2"))
                .unwrap()
                .subject,
            "bob"
        );
        assert_eq!(
            a.authenticate(&hdr("authorization", "Bearer nope"))
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            a.authenticate(&HeaderMap::new()).unwrap_err().code,
            "missing_token"
        );
    }

    #[test]
    fn jwt_hs256_roles_expiry_issuer() {
        let cfg = AuthConfig {
            enabled: true,
            auth_type: AuthType::Jwt,
            jwt: JwtConfig {
                secret: "s3cret".into(),
                issuer: Some("me".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let a = Authenticator::new(&cfg).unwrap();
        let mk = |claims: Value| {
            let t = encode(
                &Header::default(),
                &claims,
                &EncodingKey::from_secret(b"s3cret"),
            )
            .unwrap();
            hdr("authorization", &format!("Bearer {t}"))
        };
        let ok = mk(json!({"sub":"u1","iss":"me","exp":now()+600,"roles":["a","b"]}));
        let id = a.authenticate(&ok).unwrap();
        assert_eq!((id.subject.as_str(), id.roles.len()), ("u1", 2));
        let space = mk(json!({"sub":"u2","iss":"me","exp":now()+600,"roles":"x y"}));
        assert_eq!(a.authenticate(&space).unwrap().roles, vec!["x", "y"]);
        let expired = mk(json!({"sub":"u","iss":"me","exp":now()-600}));
        assert_eq!(a.authenticate(&expired).unwrap_err().code, "token_expired");
        let wrong_iss = mk(json!({"sub":"u","iss":"evil","exp":now()+600}));
        assert_eq!(
            a.authenticate(&wrong_iss).unwrap_err().code,
            "invalid_token"
        );
        let forged = encode(
            &Header::default(),
            &json!({"sub":"u","iss":"me","exp":now()+600}),
            &EncodingKey::from_secret(b"other"),
        )
        .unwrap();
        assert_eq!(
            a.authenticate(&hdr("authorization", &format!("Bearer {forged}")))
                .unwrap_err()
                .code,
            "invalid_token"
        );
    }

    #[test]
    fn basic_and_cookie() {
        let cfg = AuthConfig {
            enabled: true,
            auth_type: AuthType::Basic,
            basic: crate::config::BasicConfig {
                username: "u".into(),
                password: "p".into(),
                roles: vec!["r".into()],
            },
            ..Default::default()
        };
        let a = Authenticator::new(&cfg).unwrap();
        let good = format!("Basic {}", STANDARD.encode("u:p"));
        assert_eq!(
            a.authenticate(&hdr("authorization", &good)).unwrap().roles,
            vec!["r"]
        );
        let bad = format!("Basic {}", STANDARD.encode("u:x"));
        let e = a.authenticate(&hdr("authorization", &bad)).unwrap_err();
        assert!(e.headers.iter().any(|(k, _)| k == "www-authenticate"));

        let cookie_cfg = AuthConfig {
            enabled: true,
            auth_type: AuthType::Token,
            token: TokenConfig {
                value: "tok".into(),
                source: TokenSource::Cookie,
                name: "sid".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let c = Authenticator::new(&cookie_cfg).unwrap();
        assert!(c.authenticate(&hdr("cookie", "a=1; sid=tok; b=2")).is_ok());
        assert!(c.authenticate(&hdr("cookie", "sid=wrong")).is_err());
    }
}
