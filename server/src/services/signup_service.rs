//! Asking the external signup service to enrol the calling merchant.
//!
//! The merchant never holds a credential for that service. This server knows
//! who is logged in, so it calls the service itself and asserts the account id
//! in a signed, short-lived, single-use statement. The service verifies four
//! things about it: the account, the audience (so one minted for another
//! service cannot be spent there), the expiry, and that its `jti` has not been
//! seen before.
//!
//! The signature is HMAC-SHA256, as outbound webhooks are, under a secret
//! shared with that one service and used for nothing else. Token format:
//! `hex(claims json) "." hex(mac over those exact bytes)`.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use utoipa::ToSchema;
use uuid::Uuid;

/// How long an assertion lives. The service refuses anything claiming more
/// than two minutes, so this is well inside its ceiling.
const TTL_SECS: i64 = 60;

const MIN_SECRET_LEN: usize = 32;

const MAX_PLANS: usize = 50;
const MAX_PLAN_NAME_LEN: usize = 200;
const MAX_PLAN_DESCRIPTION_LEN: usize = 500;

/// A plan a merchant may choose, as the operator describes it. `id` is the
/// identifier the signup service knows the plan by.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SignupPlan {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone)]
pub struct SignupServiceConfig {
    /// Base URL of the service, without a trailing slash.
    pub url: String,
    pub secret: SecretString,
    /// The name the service answers to in an assertion's audience.
    pub audience: String,
    /// What `GET /users/me/plans` offers. Empty when none were configured.
    pub plans: Vec<SignupPlan>,
}

impl std::fmt::Debug for SignupServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupServiceConfig")
            .field("url", &self.url)
            .field("secret", &"[redacted]")
            .field("audience", &self.audience)
            .field("plans", &self.plans)
            .finish()
    }
}

/// Parse the operator's plan list: a JSON array of plans. Blank means none.
/// Ids must be positive and unique; names must be present and bounded.
pub fn parse_plans(raw: &str) -> Result<Vec<SignupPlan>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let plans: Vec<SignupPlan> = serde_json::from_str(raw)
        .map_err(|e| format!("SIGNUP_PLANS is not a valid plan list: {e}"))?;
    if plans.len() > MAX_PLANS {
        return Err(format!("SIGNUP_PLANS lists more than {MAX_PLANS} plans"));
    }
    let mut seen = HashSet::new();
    for p in &plans {
        if p.id <= 0 {
            return Err(format!("SIGNUP_PLANS: plan id {} must be positive", p.id));
        }
        if p.name.trim().is_empty() || p.name.len() > MAX_PLAN_NAME_LEN {
            return Err(format!(
                "SIGNUP_PLANS: plan {} needs a name of 1 to {MAX_PLAN_NAME_LEN} bytes",
                p.id
            ));
        }
        let too_long = p.description.as_deref().map_or(0, str::len) > MAX_PLAN_DESCRIPTION_LEN;
        if too_long {
            return Err(format!(
                "SIGNUP_PLANS: plan {} has a description over {MAX_PLAN_DESCRIPTION_LEN} bytes",
                p.id
            ));
        }
        if !seen.insert(p.id) {
            return Err(format!("SIGNUP_PLANS: plan id {} appears twice", p.id));
        }
    }
    Ok(plans)
}

impl SignupServiceConfig {
    /// All three of `SIGNUP_SERVICE_URL`, `SIGNUP_SERVICE_SECRET` and
    /// `SIGNUP_SERVICE_AUDIENCE`, or none. Some without the rest is an error:
    /// a half-configured instance would offer signup and fail on the first
    /// call. Names the variable, never the secret. `SIGNUP_PLANS` is optional.
    pub fn from_lookup<F>(lookup: F) -> Result<Option<Self>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
        let (url, secret, audience) = (
            get("SIGNUP_SERVICE_URL"),
            get("SIGNUP_SERVICE_SECRET"),
            get("SIGNUP_SERVICE_AUDIENCE"),
        );
        let (url, secret, audience) = match (url, secret, audience) {
            (None, None, None) => return Ok(None),
            (Some(u), Some(s), Some(a)) => (u, s, a),
            _ => {
                return Err("SIGNUP_SERVICE_URL, SIGNUP_SERVICE_SECRET and \
                            SIGNUP_SERVICE_AUDIENCE are all required if any is set"
                    .into());
            }
        };
        // The assertion is a bearer credential, so it only travels in the
        // clear to a loopback listener, where nothing can read it in transit.
        let loopback = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
            .iter()
            .any(|p| {
                url.strip_prefix(p)
                    .is_some_and(|r| r.is_empty() || r.starts_with([':', '/']))
            });
        if !url.starts_with("https://") && !loopback {
            return Err(
                "SIGNUP_SERVICE_URL must start with https:// (http:// only for loopback)".into(),
            );
        }
        if secret.trim() != secret || secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "SIGNUP_SERVICE_SECRET must be at least {MIN_SECRET_LEN} bytes with no \
                 surrounding whitespace"
            ));
        }
        let plans = parse_plans(&lookup("SIGNUP_PLANS").unwrap_or_default())?;
        Ok(Some(Self {
            url: url.trim_end_matches('/').to_string(),
            secret: SecretString::from(secret),
            audience,
            plans,
        }))
    }

    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    #[allow(
        clippy::expect_used,
        reason = "serialising plain strings and integers cannot fail, and HMAC takes a key of any length"
    )]
    /// A fresh assertion that `account_id` is the account signing up.
    pub fn assertion(&self, account_id: Uuid, now: DateTime<Utc>) -> String {
        #[derive(Serialize)]
        struct Claims<'a> {
            sub: String,
            aud: &'a str,
            jti: String,
            iat: i64,
            exp: i64,
        }
        let claims = Claims {
            sub: account_id.to_string(),
            aud: &self.audience,
            jti: Uuid::new_v4().to_string(),
            iat: now.timestamp(),
            exp: now.timestamp() + TTL_SECS,
        };
        let body = serde_json::to_vec(&claims).expect("claims serialise");
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.expose_secret().as_bytes())
            .expect("HMAC takes a key of any size");
        mac.update(&body);
        format!(
            "{}.{}",
            hex::encode(&body),
            hex::encode(mac.finalize().into_bytes())
        )
    }

    /// Ask the service to enrol `account_id` on `plan_id`. Returns the status
    /// the service answered with and its JSON body; the caller decides what to
    /// show. A transport failure is an `Err`.
    pub async fn subscribe(
        &self,
        account_id: Uuid,
        plan_id: i64,
        client_ip: std::net::IpAddr,
    ) -> Result<(u16, serde_json::Value), reqwest::Error> {
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?
            .post(format!("{}/signup", self.url))
            .bearer_auth(self.assertion(account_id, Utc::now()))
            .header("x-forwarded-for", client_ip.to_string())
            .json(&serde_json::json!({
                "account_id": account_id.to_string(),
                "plan_id": plan_id,
            }))
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or_else(|e| {
            // A mangled 409 would silently lose the pending checkout link.
            tracing::warn!(status, error = %e, "the signup service sent an unreadable body");
            serde_json::Value::Null
        });
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::{Router, extract::Request, http::StatusCode, routing::post};

    fn config(url: &str) -> SignupServiceConfig {
        SignupServiceConfig {
            url: url.into(),
            secret: SecretString::from("k".repeat(40)),
            audience: "signup.test".into(),
            plans: Vec::new(),
        }
    }

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn an_assertion_carries_account_audience_a_fresh_jti_and_a_short_ttl() {
        let c = config("https://s.example");
        let account = Uuid::new_v4();
        let now = Utc::now();
        let (a, b) = (c.assertion(account, now), c.assertion(account, now));

        let (body, sig) = a.split_once('.').unwrap();
        let body = hex::decode(body).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&[b'k'; 40]).unwrap();
        mac.update(&body);
        mac.verify_slice(&hex::decode(sig).unwrap())
            .expect("signed with the shared secret over the exact claims bytes");

        let claims: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(claims["sub"], account.to_string());
        assert_eq!(claims["aud"], "signup.test");
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            60
        );
        assert_ne!(a, b, "every assertion has its own jti");
    }

    #[test]
    fn configuration_is_all_three_or_none() {
        assert!(
            SignupServiceConfig::from_lookup(lookup(&[]))
                .unwrap()
                .is_none()
        );
        assert!(
            SignupServiceConfig::from_lookup(lookup(&[("SIGNUP_SERVICE_URL", "https://s")]))
                .is_err()
        );
        const SECRET: &str = "0123456789012345678901234567890123456789";
        let ok = SignupServiceConfig::from_lookup(lookup(&[
            ("SIGNUP_SERVICE_URL", "https://s.example/"),
            ("SIGNUP_SERVICE_SECRET", SECRET),
            ("SIGNUP_SERVICE_AUDIENCE", "a"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(ok.url, "https://s.example");
        let short = SignupServiceConfig::from_lookup(lookup(&[
            ("SIGNUP_SERVICE_URL", "https://s"),
            ("SIGNUP_SERVICE_SECRET", "tooshort-marker"),
            ("SIGNUP_SERVICE_AUDIENCE", "a"),
        ]));
        assert!(short.is_err());
        let padded = SignupServiceConfig::from_lookup(lookup(&[
            ("SIGNUP_SERVICE_URL", "https://s"),
            (
                "SIGNUP_SERVICE_SECRET",
                " 0123456789012345678901234567890123456789\n",
            ),
            ("SIGNUP_SERVICE_AUDIENCE", "a"),
        ]));
        assert!(padded.is_err(), "surrounding whitespace is refused");
        assert!(
            !format!("{short:?}").contains("tooshort-marker"),
            "no secret in errors"
        );
        assert!(
            !format!("{ok:?}").contains(SECRET),
            "no secret in the config's Debug output"
        );
    }

    #[test]
    fn a_cleartext_url_is_refused_except_to_loopback() {
        const SECRET: &str = "0123456789012345678901234567890123456789";
        let with = |url: &'static str| {
            SignupServiceConfig::from_lookup(move |k| match k {
                "SIGNUP_SERVICE_URL" => Some(url.to_string()),
                "SIGNUP_SERVICE_SECRET" => Some(SECRET.to_string()),
                "SIGNUP_SERVICE_AUDIENCE" => Some("a".to_string()),
                _ => None,
            })
        };
        assert!(with("https://billing.example").is_ok());
        assert!(with("http://127.0.0.1:8080").is_ok());
        assert!(with("http://localhost").is_ok());
        assert!(with("http://billing.example").is_err());
        assert!(with("http://localhost.evil.example").is_err());
        assert!(with("http://127.0.0.1.evil.example").is_err());
    }

    #[test]
    fn plans_are_parsed_and_default_to_none() {
        let plans = parse_plans(
            r#"[{"id":1,"name":"Basic","description":"One store"},{"id":2,"name":"Pro"}]"#,
        )
        .unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].description.as_deref(), Some("One store"));
        assert_eq!(plans[1].description, None);

        let cfg = SignupServiceConfig::from_lookup(lookup(&[
            ("SIGNUP_SERVICE_URL", "https://s.example"),
            (
                "SIGNUP_SERVICE_SECRET",
                "0123456789012345678901234567890123456789",
            ),
            ("SIGNUP_SERVICE_AUDIENCE", "a"),
        ]))
        .unwrap()
        .unwrap();
        assert!(cfg.plans.is_empty());
    }

    #[test]
    fn a_bad_plan_list_is_refused() {
        let long = format!(r#"[{{"id":1,"name":"{}"}}]"#, "a".repeat(201));
        for bad in [
            r#"[{"id":0,"name":"x"}]"#,
            r#"[{"id":-1,"name":"x"}]"#,
            r#"[{"id":1,"name":" "}]"#,
            r#"[{"id":1,"name":"x"},{"id":1,"name":"y"}]"#,
            r#"[{"id":1,"name":"x","extra":1}]"#,
            "not json",
            long.as_str(),
        ] {
            assert!(parse_plans(bad).is_err(), "should refuse: {bad}");
        }
        assert!(parse_plans("").unwrap().is_empty());
        assert!(parse_plans("  ").unwrap().is_empty());
    }

    #[test]
    fn a_malformed_plan_list_fails_the_whole_configuration() {
        let with = |plans: &'static str| {
            SignupServiceConfig::from_lookup(move |k| match k {
                "SIGNUP_SERVICE_URL" => Some("https://s.example".to_string()),
                "SIGNUP_SERVICE_SECRET" => {
                    Some("0123456789012345678901234567890123456789".to_string())
                }
                "SIGNUP_SERVICE_AUDIENCE" => Some("a".to_string()),
                "SIGNUP_PLANS" => Some(plans.to_string()),
                _ => None,
            })
        };
        assert!(with("not json").is_err());
        assert_eq!(
            with(r#"[{"id":3,"name":"x"}]"#)
                .unwrap()
                .unwrap()
                .plans
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn subscribe_forwards_the_client_address_as_a_bare_x_forwarded_for() {
        let app = Router::new().route(
            "/signup",
            post(|req: Request| async move {
                let xff = req.headers()["x-forwarded-for"]
                    .to_str()
                    .unwrap()
                    .to_string();
                (
                    StatusCode::CREATED,
                    axum::Json(serde_json::json!({ "xff": xff })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        let (status, body) = config(&url)
            .subscribe(Uuid::new_v4(), 7, client)
            .await
            .unwrap();

        assert_eq!(status, 201);
        // Exact string: a socket address would carry a port and is refused here.
        assert_eq!(body["xff"], "203.0.113.9");
    }

    #[tokio::test]
    async fn subscribe_presents_the_assertion_and_names_the_account_in_the_body() {
        let app = Router::new().route(
            "/signup",
            post(|req: Request| async move {
                let auth = req.headers()["authorization"].to_str().unwrap().to_string();
                let body = axum::body::to_bytes(req.into_body(), 4096).await.unwrap();
                let mut seen: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen["authorization"] = auth.into();
                (StatusCode::CREATED, axum::Json(seen))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let account = Uuid::new_v4();
        let (status, body) = config(&url)
            .subscribe(account, 7, std::net::IpAddr::from([127, 0, 0, 1]))
            .await
            .unwrap();

        assert_eq!(status, 201);
        assert_eq!(body["account_id"], account.to_string());
        assert_eq!(body["plan_id"], 7);
        let token = body["authorization"].as_str().unwrap();
        let claims = hex::decode(
            token
                .strip_prefix("Bearer ")
                .unwrap()
                .split_once('.')
                .unwrap()
                .0,
        )
        .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(
            claims["sub"],
            account.to_string(),
            "the assertion is about the same account"
        );
    }
}
