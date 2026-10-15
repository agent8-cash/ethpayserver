//! Authentication extractors for API endpoints.
//!
//! Provides axum extractors for validating authentication and authorization.
//! Supports both session tokens (UUID) and API keys (ak_XXXX_YYYY).

use axum::{
    extract::FromRequestParts,
    http::{StatusCode, header::AUTHORIZATION, request::Parts},
};

use auth::{Permission, Role, SessionId, SessionService, UserId, UserInfo};
use chrono::{DateTime, Utc};

use super::api_key_deprecation::DeprecationSlot;
use super::api_key_hash::hash_api_key;
pub(super) use super::api_key_scope::{
    key_grants_merchant_read, key_grants_standing_push, key_grants_store_permission,
    key_retains_unrestricted_access,
};
use super::auth_freshness::{is_grace_expired, is_reauth_stale};
use super::standing_warning::AccountSlot;
use crate::state::PgAppState;

/// Carried in the shared `DeprecationSlot` that the deprecation-header
/// middleware installs into request extensions before the handler runs.
/// `validate_api_key` writes into the slot when an authenticated request
/// uses a deprecated-but-still-valid API key, and the middleware reads it
/// afterwards to stamp the `X-API-Key-Deprecated` response header.
///
/// Request extensions set from inside a handler/extractor are NOT visible
/// to the outer middleware after `next.run(req)` — that's why the transport
/// is an Arc-shared slot rather than a direct extension write.
#[derive(Debug, Clone)]
pub struct ApiKeyDeprecationInfo {
    pub deprecated_at: DateTime<Utc>,
    pub grace_deadline: DateTime<Utc>,
}

/// Extractor that validates any authenticated user.
///
/// Supports both session tokens and API keys:
/// - `Authorization: Bearer <uuid>` → session-based auth
/// - `Authorization: Bearer ak_...` → API key auth
pub struct AuthenticatedUser(pub UserInfo);

/// Like `AuthenticatedUser`, but also carries the store-permission scope
/// carried by the API key that authenticated this request, if any.
///
/// `None` covers session auth and every key that predates or has not been
/// narrowed by per-key store scoping - both inherit the owner's role (and
/// every store `user_has_store_permission` would grant it) in full, same as
/// before this existed. `Some(set)` is intersected on top of whatever
/// `user_has_store_permission` already grants the owner, never used alone -
/// see `key_grants_store_permission`.
///
/// A separate type from `AuthenticatedUser` rather than a field added to it:
/// that struct's single-field shape is destructured by ~80 call sites across
/// this codebase, and only the handful that gate a store permission need to
/// know a key's scope.
pub struct StoreScopedUser(pub UserInfo, pub Option<Vec<String>>);

/// Like `AuthenticatedUser`, but also carries whether the credential used to
/// authenticate this request has been explicitly granted the operator
/// property, plus the same store-permission scope `StoreScopedUser` carries.
///
/// The operator property lives on the credential (today, only an API key's
/// `is_operator` column), not on the requesting user's role or on any store
/// the request names - it is decided once, at authentication time, and
/// nothing downstream can derive it from *what* is being asked for. Use this
/// instead of `AuthenticatedUser` only where both that distinction and the
/// key's store scope matter - today, only `create_invoice`.
pub struct AuthenticatedCaller {
    pub user: UserInfo,
    pub is_operator: bool,
    pub key_scope: Option<Vec<String>>,
}

/// Extractor for endpoints that must not trust a merely-valid session -
/// changing the account's recovery email being the case this exists for.
///
/// A session is a bearer token good for its full lifetime (up to 24h idle-
/// checked, longer absolute). That is fine for reading data, and wrong for an
/// action where a session hijacked hours after login must not be able to
/// swap the account's recovery address out from under its owner. This
/// codebase has no separate WebAuthn step-up ceremony (see `payserver-commons`
/// auth crate - there is no "prove you hold this passkey without logging in"
/// primitive), so this reuses the one proof already on hand: a session's
/// `created_at` records the moment its login assertion - passkey or wallet -
/// was verified. Requiring that moment to be within `REAUTH_FRESHNESS` is
/// exactly "you just completed a passkey or wallet assertion". The client
/// re-runs the ordinary login ceremony and retries with the session it
/// returns; nothing about the caller's *existing* session changes.
///
/// API keys never satisfy this: a key is not a login assertion, so it is
/// rejected outright rather than checked for freshness it cannot have.
pub struct FreshlyAuthenticatedUser(pub UserInfo);

impl<A> FromRequestParts<PgAppState<A>> for FreshlyAuthenticatedUser
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(parts)?;

        if token.starts_with("ak_") {
            return Err((
                StatusCode::UNAUTHORIZED,
                "This action requires a fresh sign-in, not an API key",
            ));
        }

        let uuid = uuid::Uuid::parse_str(&token)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid session ID format"))?;

        let (user_info, session) = state
            .auth_service
            .validate_session(SessionId(uuid))
            .await
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;

        if is_reauth_stale(session.created_at, Utc::now()) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "This action requires a fresh sign-in. Please log in again and retry.",
            ));
        }

        Ok(FreshlyAuthenticatedUser(user_info))
    }
}

/// Extractor that validates server admin authentication.
///
/// Same as AuthenticatedUser but requires ServerAdmin role.
pub struct AdminAuth(pub UserInfo);

/// Extract the bearer token string from the Authorization header.
fn extract_bearer_token(parts: &Parts) -> Result<String, (StatusCode, &'static str)> {
    let auth_header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header"))?;

    let auth_str = auth_header
        .to_str()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid Authorization header"))?;

    let token = auth_str
        .strip_prefix("Bearer ")
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid Authorization format"))?;

    Ok(token.to_string())
}

/// Validate session and return user info.
///
/// Takes `&mut Parts` so the API-key branch can stamp `ApiKeyDeprecationInfo`
/// into request extensions when a deprecated-but-still-valid key is used.
async fn validate_session<A>(
    parts: &mut Parts,
    state: &PgAppState<A>,
) -> Result<UserInfo, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let (user_info, _is_operator, scope) = validate_session_with_scope(parts, state, true).await?;

    // A key that carries a narrowed scope must not reach a caller that did
    // not ask for one.
    //
    // This function is, by construction, the "I do not handle scope" path:
    // the extractors that do handle it (`StoreScopedUser`,
    // `AuthenticatedCaller`) call `validate_session_with_scope` directly and
    // get the scope to enforce. Everything reaching *here* discards it, and
    // discarding a restriction grants everything it was meant to withhold -
    // most of this API is still on that path, key management and account
    // deletion included. Narrowing a key would then be reversible by using
    // it: one call to an unmigrated route is enough to act outside the scope,
    // or to mint a second key carrying none.
    //
    // Refusing is the safe direction, and it is free right now because
    // narrowed keys are new: no key in existence has a scope to be refused
    // for. It also fails in the direction a caller can see and report, rather
    // than silently granting more than the owner asked to give. The cost is
    // that migrating a route to honour scope is now the only way to make it
    // reachable by a scoped key, which is the incentive pointing the right
    // way.
    //
    // A session carries no scope at all, so this never affects one.
    if !key_retains_unrestricted_access(scope.as_deref()) {
        return Err((
            StatusCode::FORBIDDEN,
            "This API key is limited to specific store permissions and cannot be used here",
        ));
    }

    Ok(user_info)
}

/// Same as `validate_session`, but also returns whether the credential is
/// explicitly granted the operator property (see `AuthenticatedCaller`) and
/// the API key's stored store-permission scope (`None` for session auth).
/// Split out so the ~80 call sites that only ever want `UserInfo` don't have
/// to carry data they never look at - see `StoreScopedUser`.
///
/// `narrow_role` is whether a scoped key's admin owner is downgraded to
/// `Role::User` (see `validate_api_key`). Only `MerchantReader` passes false,
/// because it has to know the owner's real role to check the scope against it.
async fn validate_session_with_scope<A>(
    parts: &mut Parts,
    state: &PgAppState<A>,
    narrow_role: bool,
) -> Result<(UserInfo, bool, Option<Vec<String>>), (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let token = extract_bearer_token(parts)?;

    // If the token starts with "ak_", validate as API key
    if token.starts_with("ak_") {
        let validated = validate_api_key(&token, parts, state, narrow_role).await?;
        record_account(parts, &validated.0);
        return Ok(validated);
    }

    // Otherwise treat as session UUID
    let uuid = uuid::Uuid::parse_str(&token)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid session ID format"))?;

    let session_id = SessionId(uuid);

    let (user_info, _session) = state
        .auth_service
        .validate_session(session_id)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;

    record_account(parts, &user_info);

    // A session carries no operator property or key scope of its own - the
    // caller is bound only by their role and store membership, same as
    // before either existed.
    Ok((user_info, false, None))
}

/// Tell the expiry-warning middleware which account this request is for. A
/// missing slot (tests, unusual router builds) just means no header.
fn record_account(parts: &Parts, user: &UserInfo) {
    if let Some(slot) = parts.extensions.get::<AccountSlot>()
        && let Ok(mut guard) = slot.lock()
    {
        *guard = Some(user.id.0);
    }
}

/// Validate an API key and return the associated user info, plus the key's
/// own `is_operator` flag and stored store-permission scope.
///
/// When the key is deprecated but within its grace window, stamps an
/// `ApiKeyDeprecationInfo` into `parts.extensions` so the response-header
/// middleware can emit `X-API-Key-Deprecated: rotate before <iso>`.
async fn validate_api_key<A>(
    raw_key: &str,
    parts: &mut Parts,
    state: &PgAppState<A>,
    narrow_role: bool,
) -> Result<(UserInfo, bool, Option<Vec<String>>), (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let key_hash = hash_api_key(raw_key);

    let key_info = state
        .data_service
        .get_api_key_auth_info(&key_hash)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid API key"))?;

    // Check if active
    if !key_info.is_active {
        return Err((StatusCode::UNAUTHORIZED, "API key revoked"));
    }

    // Check expiration
    if key_info
        .expires_at
        .is_some_and(|expires_at| Utc::now() > expires_at)
    {
        return Err((StatusCode::UNAUTHORIZED, "API key expired"));
    }

    // Check deprecation grace window
    if let Some(deprecated_at) = key_info.deprecated_at {
        let grace_secs = deprecation_grace_secs();
        let now = Utc::now();
        if is_grace_expired(deprecated_at, now, grace_secs) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "API key deprecated and grace period expired",
            ));
        }
        let deadline = deprecated_at + chrono::Duration::seconds(grace_secs);
        // Write into the shared slot the deprecation-header middleware
        // installed. Direct `parts.extensions.insert` would be invisible to
        // the outer middleware after `next.run` — only an Arc-shared handle
        // survives that boundary. If the middleware isn't wired (tests,
        // unusual router builds) we quietly skip: auth still succeeds, the
        // consumer just doesn't see the header on this one request.
        if let Some(slot) = parts.extensions.get::<DeprecationSlot>()
            && let Ok(mut guard) = slot.lock()
        {
            *guard = Some(ApiKeyDeprecationInfo {
                deprecated_at,
                grace_deadline: deadline,
            });
        }
    }

    // Resolve the user via data_service (PgDataService implements UserRepository)
    use auth::UserRepository;
    let user = state
        .data_service
        .get_user(UserId(key_info.user_id))
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve user"))?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "User not found"))?;
    let mut user = UserInfo::from(&user);

    // Narrow the in-memory role to what this specific key is actually scoped
    // to do. `Role` has exactly two levels, and every ServerAdmin gate in
    // this codebase (there are over a dozen, from plugin install to user
    // role management) is a bare `role == Role::ServerAdmin` comparison, not
    // a per-`Permission` one - so "scoped below ServerAdmin" can only mean
    // "this request runs as a regular User", and setting that once here
    // makes all of those checks respect the key's scope for free, without
    // threading a wider permission type through every call site. A key
    // authenticates as its owner's full role only when its stored
    // `permissions` is null (never narrowed - every key that predates this
    // column, and any key an admin has not deliberately scoped) or
    // explicitly includes `unrestricted`.
    if narrow_role
        && user.role == Role::ServerAdmin
        && !key_retains_unrestricted_access(key_info.permissions.as_deref())
    {
        user.role = Role::User;
    }

    // Fire-and-forget: update last_used_at
    let ds = state.data_service.clone();
    let key_id = key_info.id;
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id = $1")
            .bind(key_id)
            .execute(ds.pool())
            .await;
    });

    Ok((user, key_info.is_operator, key_info.permissions))
}

/// Get the deprecation grace period in seconds (default: 48 hours).
///
/// Also used by `users::list_api_keys` to compute the
/// `deprecation_expires_at` deadline surfaced on list responses, so the
/// client can show an accurate expiry even when operators override the
/// grace window via `API_KEY_DEPRECATION_GRACE_SECS`.
pub(super) fn deprecation_grace_secs() -> i64 {
    std::env::var("API_KEY_DEPRECATION_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(172_800) // 48 hours
}

impl<A> FromRequestParts<PgAppState<A>> for AuthenticatedUser
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let user_info = validate_session(parts, state).await?;
        Ok(AuthenticatedUser(user_info))
    }
}

impl<A> FromRequestParts<PgAppState<A>> for StoreScopedUser
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let (user_info, _is_operator, scope) =
            validate_session_with_scope(parts, state, true).await?;
        Ok(StoreScopedUser(user_info, scope))
    }
}

impl<A> FromRequestParts<PgAppState<A>> for AuthenticatedCaller
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let (user, is_operator, key_scope) =
            validate_session_with_scope(parts, state, true).await?;
        Ok(AuthenticatedCaller {
            user,
            is_operator,
            key_scope,
        })
    }
}

/// Read-only access to the server-wide merchant listing, for a session, an
/// unrestricted key, or a key scoped to `ethpay.server.canviewusers`.
///
/// `AdminAuth` cannot serve this: it refuses every scoped key, which is right
/// for everything that can change something and leaves a read-only
/// integration no choice but an unrestricted admin key. The owner must still
/// be a `ServerAdmin` - a key never exceeds its owner - and that role is read
/// before the scoped-key downgrade, which would otherwise hide it.
pub struct MerchantReader(pub UserInfo);

impl<A> FromRequestParts<PgAppState<A>> for MerchantReader
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let (user_info, _is_operator, scope) =
            validate_session_with_scope(parts, state, false).await?;

        if user_info.role != Role::ServerAdmin {
            return Err((StatusCode::FORBIDDEN, "Admin access required"));
        }
        if !key_grants_merchant_read(scope.as_deref()) {
            return Err((
                StatusCode::FORBIDDEN,
                "This API key is not scoped to read the merchant listing",
            ));
        }

        Ok(MerchantReader(user_info))
    }
}

/// The caller of the account-standing push: an API key scoped to exactly that
/// action, owned by a `ServerAdmin`.
///
/// Stricter than `MerchantReader` on purpose. A session, an unscoped key and an
/// `unrestricted` key are all refused: the sender is meant to hold a credential
/// that cannot be used for anything else, and accepting an admin key would let
/// it be given one without anyone noticing. The owner's real role is read
/// before the scoped-key downgrade, as there, so a key never exceeds its owner.
pub struct StandingPusher(pub UserInfo);

impl<A> FromRequestParts<PgAppState<A>> for StandingPusher
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let (user_info, _is_operator, scope) =
            validate_session_with_scope(parts, state, false).await?;

        if !key_grants_standing_push(scope.as_deref()) {
            return Err((
                StatusCode::FORBIDDEN,
                "This credential is not scoped to push account standing",
            ));
        }
        if user_info.role != Role::ServerAdmin {
            return Err((StatusCode::FORBIDDEN, "Admin access required"));
        }

        Ok(StandingPusher(user_info))
    }
}

/// Admin-ness without requiring it.
///
/// `AdminAuth` rejects a non-admin, which is right for an admin-only route and
/// wrong for one that answers everyone but says more to an admin. This never
/// rejects: no session, an expired session or a non-admin all resolve to
/// `false`, so the caller decides what to withhold rather than whether to
/// answer at all.
///
/// Used by `/health/chains`, where whether a chain is up is public and the
/// block heights behind that answer are not.
#[derive(Debug, Clone, Copy)]
pub struct MaybeAdmin(pub bool);

impl<A> FromRequestParts<PgAppState<A>> for MaybeAdmin
where
    A: SessionService + 'static,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        // A failed lookup is "not an admin", never an error: an anonymous
        // caller is the expected case on a public route.
        let is_admin = validate_session(parts, state)
            .await
            .is_ok_and(|user| user.role == Role::ServerAdmin);
        Ok(MaybeAdmin(is_admin))
    }
}

impl<A> FromRequestParts<PgAppState<A>> for AdminAuth
where
    A: SessionService + 'static,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PgAppState<A>,
    ) -> Result<Self, Self::Rejection> {
        let user_info = validate_session(parts, state).await?;

        // Check for ServerAdmin role
        if user_info.role != Role::ServerAdmin {
            return Err((StatusCode::FORBIDDEN, "Admin access required"));
        }

        Ok(AdminAuth(user_info))
    }
}

impl AuthenticatedUser {
    /// Get the user ID.
    pub fn user_id(&self) -> UserId {
        self.0.id
    }

    /// Get the user's role.
    pub fn role(&self) -> Role {
        self.0.role
    }

    /// Check if user has a specific permission.
    pub fn has_permission(&self, permission: Permission) -> bool {
        self.0.role.has_permission(permission)
    }
}

impl AdminAuth {
    /// Get the user ID.
    pub fn user_id(&self) -> UserId {
        self.0.id
    }
}

#[cfg(test)]
#[path = "extractors_reachability_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test-only setup")]
mod reachability_tests;
