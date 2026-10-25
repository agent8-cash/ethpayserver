//! Store webhook management endpoints: get, configure, delete.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;

use auth::repository::StoreRepository;
use auth::{SessionService, StoreId};
use types::{StoreWebhookReader, StoreWebhookWriter};

use super::super::extractors::StoreScopedUser;
use super::require_store_settings_permission;
use crate::state::PgAppState;
pub use api_types::{ConfigureWebhookRequest, WebhookResponse};

/// Extra docker-network aliases, beyond `https://` and `http://localhost`,
/// this deploy trusts as a webhook target - read from deploy config, never
/// hardcoded here. Nothing in this public repository should name which
/// internal service, if any, that is; the value lives in the private deploy
/// config that sets this variable, not in source any outside reader can see.
fn trusted_internal_webhook_hosts() -> Vec<String> {
    std::env::var("TRUSTED_INTERNAL_WEBHOOK_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// A webhook URL must be HTTPS, loopback, or a deploy-configured internal host.
///
/// Parses the URL rather than matching a string prefix: a trusted host must
/// be checked against the parsed *host*, not a `starts_with`, or
/// `http://<host>.attacker.example` would pass a prefix check while
/// resolving nowhere near this deploy.
fn webhook_url_allowed(url: &str, trusted_hosts: &[String]) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http"
        && matches!(
            parsed.host_str(),
            Some(h) if h == "localhost" || trusted_hosts.iter().any(|t| t == h)
        )
}

#[cfg(test)]
mod webhook_url_allowed_tests {
    use super::webhook_url_allowed;

    #[test]
    fn https_is_allowed() {
        assert!(webhook_url_allowed("https://example.com/hook", &[]));
    }

    #[test]
    fn localhost_is_allowed_for_dev() {
        assert!(webhook_url_allowed("http://localhost:4000/hook", &[]));
    }

    #[test]
    fn a_configured_trusted_host_is_allowed() {
        let trusted = vec!["internal-svc".to_string()];
        assert!(webhook_url_allowed(
            "http://internal-svc:3000/hook",
            &trusted
        ));
    }

    #[test]
    fn an_unconfigured_host_is_refused() {
        assert!(!webhook_url_allowed("http://internal-svc:3000/hook", &[]));
    }

    #[test]
    fn a_lookalike_host_is_not_the_trusted_one() {
        // The whole point of parsing: a naive `starts_with` would pass this.
        // It must not.
        let trusted = vec!["internal-svc".to_string()];
        assert!(!webhook_url_allowed(
            "http://internal-svc.attacker.example/hook",
            &trusted
        ));
        assert!(!webhook_url_allowed(
            "http://internal-svcx:3000/hook",
            &trusted
        ));
    }

    #[test]
    fn an_arbitrary_plain_http_host_is_refused() {
        assert!(!webhook_url_allowed("http://example.com/hook", &[]));
    }

    #[test]
    fn garbage_is_refused_not_panicking() {
        assert!(!webhook_url_allowed("not a url at all", &[]));
    }
}

/// Get webhook configuration for a store.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/webhook",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "Webhook configuration", body = WebhookResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Webhook not configured"),
    )
)]
pub async fn get_store_webhook<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<WebhookResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let webhook = StoreWebhookReader::get_webhook(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(WebhookResponse {
        id: webhook.id,
        store_id: webhook.store_id,
        webhook_url: webhook.webhook_url,
        webhook_secret: None, // Don't expose secret on GET
        enabled: webhook.enabled,
        created_at: webhook.created_at,
        updated_at: webhook.updated_at,
    }))
}

/// Configure webhook for a store.
///
/// Creates or updates the webhook configuration. A new secret is generated
/// on each update and returned in the response.
#[utoipa::path(
    put,
    path = "/stores/{store_id}/webhook",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    request_body = ConfigureWebhookRequest,
    responses(
        (status = 200, description = "Webhook configured", body = WebhookResponse),
        (status = 400, description = "Invalid webhook URL"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn configure_store_webhook<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<ConfigureWebhookRequest>,
) -> Result<Json<WebhookResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    if !webhook_url_allowed(&req.webhook_url, &trusted_internal_webhook_hosts()) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Verify store exists
    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Generate new secret
    let secret = uuid::Uuid::new_v4().to_string();

    let webhook = StoreWebhookWriter::upsert_webhook(
        &*state.data_service,
        store_id,
        &req.webhook_url,
        &secret,
        req.enabled,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(WebhookResponse {
        id: webhook.id,
        store_id: webhook.store_id,
        webhook_url: webhook.webhook_url,
        webhook_secret: Some(webhook.webhook_secret), // Show secret on create/update
        enabled: webhook.enabled,
        created_at: webhook.created_at,
        updated_at: webhook.updated_at,
    }))
}

/// Delete webhook configuration for a store.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}/webhook",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 204, description = "Webhook deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Webhook not found"),
    )
)]
pub async fn delete_store_webhook<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let deleted = StoreWebhookWriter::delete_webhook(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !deleted {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(StatusCode::NO_CONTENT)
}
