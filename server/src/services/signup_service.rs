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

use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sha2::Sha256;
use uuid::Uuid;

/// How long an assertion lives. The service refuses anything claiming more
/// than two minutes, so this is well inside its ceiling.
const TTL_SECS: i64 = 60;

const MIN_SECRET_LEN: usize = 32;

#[derive(Clone)]
pub struct SignupServiceConfig {
    /// Base URL of the service, without a trailing slash.
    pub url: String,
    pub secret: SecretString,
    /// The name the service answers to in an assertion's audience.
    pub audience: String,
}

impl std::fmt::Debug for SignupServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupServiceConfig")
            .field("url", &self.url)
            .field("secret", &"[redacted]")
            .field("audience", &self.audience)
            .finish()
    }
}

impl SignupServiceConfig {
    /// All three of `SIGNUP_SERVICE_URL`, `SIGNUP_SERVICE_SECRET` and
    /// `SIGNUP_SERVICE_AUDIENCE`, or none. Some without the rest is an error:
    /// a half-configured instance would offer signup and fail on the first
    /// call. Names the variable, never the secret.
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
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("SIGNUP_SERVICE_URL must start with http:// or https://".into());
        }
        if secret.trim() != secret || secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "SIGNUP_SERVICE_SECRET must be at least {MIN_SECRET_LEN} bytes with no \
                 surrounding whitespace"
            ));
        }
        Ok(Some(Self {
            url: url.trim_end_matches('/').to_string(),
            secret: SecretString::from(secret),
            audience,
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
    ) -> Result<(u16, serde_json::Value), reqwest::Error> {
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?
            .post(format!("{}/signup", self.url))
            .bearer_auth(self.assertion(account_id, Utc::now()))
            .json(&serde_json::json!({
                "account_id": account_id.to_string(),
                "plan_id": plan_id,
            }))
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
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
            ("SIGNUP_SERVICE_SECRET", "short"),
            ("SIGNUP_SERVICE_AUDIENCE", "a"),
        ]));
        assert!(short.is_err());
        assert!(
            !format!("{short:?}").contains("short\""),
            "no secret in errors"
        );
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
        let (status, body) = config(&url).subscribe(account, 7).await.unwrap();

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
