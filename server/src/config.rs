//! Server configuration.
//!
//! # Environment Variables
//!
//! ## Required
//! - `DATABASE_URL` - PostgreSQL connection string
//! - `REDIS_URL` - Redis connection URL for monitor communication
//!
//! ## Server
//! - `HOST` - Server host (default: 127.0.0.1)
//! - `PORT` - Server port (default: 3000)
//! - `LOG_LEVEL` - Log level: trace, debug, info, warn, error (default: info)
//! - `LOG_FORMAT` - Log output format: `pretty` or `json` (default: pretty).
//!   `json` is what a log shipper (e.g. Grafana Cloud's Loki agent) parses;
//!   set it in any environment whose logs are actually collected.
//! - `ENABLE_SWAGGER` - Enable Swagger UI at /swagger-ui (default: true)
//!
//! ## Redis Channels
//! - `REDIS_EVENTS_CHANNEL` - Redis channel for events (default: evmmonitor:events)
//! - `REDIS_COMMANDS_CHANNEL` - Redis channel for commands (default: evmmonitor:commands)
//!
//! ## Invoice Cleanup Service
//! - `CLEANUP_FALLBACK_INTERVAL_SECS` - Fallback check interval (default: 60)
//! - `CLEANUP_UNWATCH_GRACE_PERIOD_SECS` - Grace period before unwatching (default: 60)
//!
//! ## Webhook Service
//! - `WEBHOOK_QUEUE_KEY` - Redis queue key (default: ethpayserver:webhooks)
//! - `WEBHOOK_REQUEST_TIMEOUT_SECS` - HTTP request timeout (default: 30)
//! - `WEBHOOK_POLL_INTERVAL_SECS` - Queue poll interval (default: 5)
//!
//! ## CAPTCHA (optional)
//! - `CAPTCHA_PROVIDER` - Provider name: `turnstile` or `cloudflare` (unset = disabled)
//! - `CAPTCHA_SECRET_KEY` - Provider secret key (required when CAPTCHA_PROVIDER is set)
//! - `CAPTCHA_SITE_KEY` - Provider site key (required when CAPTCHA_PROVIDER is set)
//!
//! ## Rate Limiting
//! - `RATE_LIMIT_AUTH` - Auth endpoint limit, req/min per IP (default: 10)
//! - `RATE_LIMIT_WRITE` - Write endpoint limit, req/min per IP (default: 20)
//! - `RATE_LIMIT_READ` - Read endpoint limit, req/min per IP (default: 60)
//! - `RATE_LIMIT_WS` - WebSocket upgrade limit, req/min per IP (default: 5)
//!
//! ## Watch Retry Service
//! - `WATCH_RETRY_INTERVAL_SECS` - Retry interval in seconds (default: 30)
//! - `WATCH_RETRY_ENABLED` - Enable/disable retry service (default: true)
//!
//! ## Plugins
//! - `ETHPAY_DISABLE_PLUGINS` - Safe mode: boot with every plugin disabled
//!   (default: false). Same effect as the `--disable-plugins` CLI flag.
//! - `ETHPAY_PLUGIN_DIR` - Where installed plugins' wasm lives
//! - `ETHPAY_OPERATOR_STORE_ID` - The operator's own store: where this
//!   instance issues and settles its own invoices, if it issues any to
//!   itself at all. Unset on any instance that does not; a plugin is told
//!   about payments on this store and no other. (default: ./plugins)
//!   `ETHPAY_BILLING_STORE_ID` is read as a deprecated fallback when this is
//!   unset, so an instance still running the old variable name keeps
//!   working until its deploy config catches up.
//! - `ETHPAY_OPERATOR_ACCOUNT_ID` - The account that may own
//!   `ETHPAY_OPERATOR_STORE_ID`. Unset refuses every nomination of an
//!   operator store, since an unowned nomination is exactly the thing that
//!   must never pass by default.

use secrecy::{ExposeSecret, SecretString};
use std::env;
use std::path::PathBuf;

/// Server configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Database connection URL. Secret: carries the Postgres password.
    /// `SecretString` keeps it out of the derived `Debug` above.
    pub database_url: SecretString,

    /// Redis connection URL for monitor communication. Secret: may carry
    /// credentials (`redis://user:pass@host`).
    pub redis_url: Option<SecretString>,

    /// HTTP server host.
    pub host: String,

    /// HTTP server port.
    pub port: u16,

    /// Log level (trace, debug, info, warn, error).
    pub log_level: String,

    /// Log output format: `pretty` or `json`.
    pub log_format: String,

    /// Enable Swagger UI at /swagger-ui.
    pub enable_swagger: bool,

    /// Safe mode: boot with every plugin disabled.
    ///
    /// Set via `ETHPAY_DISABLE_PLUGINS=1` or the `--disable-plugins` CLI flag.
    /// Disables plugins for this boot only - it does not uninstall them or
    /// touch their data, and clearing the flag restores them.
    pub safe_mode: bool,

    /// Lets the admin API write a mainnet chain id into `enabled_chain_ids`.
    ///
    /// Set via `MAINNET_ENABLED=1` or `=true` at process start ONLY - this is
    /// never re-read per request, and the admin API that writes
    /// `enabled_chain_ids` has no way to set this itself. The switch a
    /// handler is gated by must not be settable through that same handler,
    /// or it guards nothing. Off (fail closed) when unset, unreadable or any
    /// other value - enabling mainnet is a deploy-time act, not a request.
    pub mainnet_enabled: bool,

    /// Where installed plugins' wasm artifacts live.
    ///
    /// Set via `ETHPAY_PLUGIN_DIR`. Defaults to `./plugins` rather than a
    /// path under `/var`, so a development run and a test need no privileged
    /// directory to exist; a container image sets it explicitly to whatever
    /// volume survives a redeploy. A missing directory is not an error -
    /// it is what a server with no plugins installed looks like.
    pub plugin_dir: PathBuf,

    /// The operator's own store: where this instance issues, and settles,
    /// its own invoices.
    ///
    /// `None` on an instance that issues no invoices to itself, which is
    /// every deployment without a plugin that needs one. It must stay `None`
    /// rather than defaulting to anything: a wrong value here would hand a
    /// plugin a merchant's payments, and there is no value that is safely
    /// wrong.
    pub operator_store_id: Option<types::StoreId>,

    /// The account `operator_store_id` must be owned by.
    ///
    /// Set once, outside the admin settings API, so that nominating an
    /// operator store answers "does this belong to the operator" against a
    /// value nobody can move by saving a settings form. `None` refuses every
    /// nomination - the same "no value is safely wrong" reasoning as
    /// `operator_store_id` above, since a missing operator account is
    /// indistinguishable from one an attacker chose not to set.
    pub operator_account_id: Option<types::UserId>,
}

/// Valid log levels.
const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Required:
    /// - `DATABASE_URL` - PostgreSQL connection string
    ///
    /// Optional:
    /// - `REDIS_URL` - Redis connection URL for monitor communication
    /// - `HOST` - Server host (default: 127.0.0.1)
    /// - `PORT` - Server port (default: 3000)
    /// - `LOG_LEVEL` - Log level (default: info)
    /// - `LOG_FORMAT` - Log output format: `pretty` or `json` (default: pretty)
    /// - `ENABLE_SWAGGER` - Enable Swagger UI (default: true)
    /// - `ETHPAY_PLUGIN_DIR` - Plugin artifact directory (default: ./plugins)
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = SecretString::from(
            env::var("DATABASE_URL")
                .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is required"))?,
        );

        let redis_url = env::var("REDIS_URL").ok().map(SecretString::from);

        let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()
            .map_err(|_| anyhow::anyhow!("PORT must be a valid number"))?;

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        let log_format = env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".to_string());

        let enable_swagger = parse_enable_swagger(env::var("ENABLE_SWAGGER").ok().as_deref());

        let cli_args: Vec<String> = env::args().collect();
        let safe_mode = safe_mode_requested(|key| env::var(key).ok(), &cli_args);
        let mainnet_enabled =
            mainnet_enabled_requested(env::var("MAINNET_ENABLED").ok().as_deref());

        let plugin_dir = plugin_dir_from(|key| env::var(key).ok());

        let config = Self {
            database_url,
            redis_url,
            host,
            port,
            log_level,
            log_format,
            enable_swagger,
            safe_mode,
            mainnet_enabled,
            plugin_dir,
            operator_store_id: operator_store_id_from(|key| env::var(key).ok()),
            operator_account_id: operator_account_id_from(|key| env::var(key).ok()),
        };

        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values.
    fn validate(&self) -> anyhow::Result<()> {
        // Validate DATABASE_URL format
        let database_url = self.database_url.expose_secret();
        if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
            anyhow::bail!("DATABASE_URL must start with 'postgres://' or 'postgresql://'");
        }

        // Validate REDIS_URL format (if provided)
        if let Some(redis_url) = self.redis_url.as_ref().map(ExposeSecret::expose_secret)
            && !redis_url.starts_with("redis://")
            && !redis_url.starts_with("rediss://")
        {
            anyhow::bail!("REDIS_URL must start with 'redis://' or 'rediss://'");
        }

        // Validate LOG_LEVEL
        let log_level_lower = self.log_level.to_lowercase();
        if !VALID_LOG_LEVELS.contains(&log_level_lower.as_str()) {
            anyhow::bail!("LOG_LEVEL must be one of: {}", VALID_LOG_LEVELS.join(", "));
        }

        // Validate PORT range
        if self.port == 0 {
            anyhow::bail!("PORT must be between 1 and 65535");
        }

        Ok(())
    }

    /// Get the server bind address.
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Derive the WebAuthn RP ID from the origin URL's host.
/// Returns `None` if the URL can't be parsed or has no host.
pub fn derive_rp_id_from_origin(origin: &str) -> Option<String> {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
}

/// Validate that the WebAuthn RP ID is a registrable domain suffix of the origin host.
pub fn validate_rp_id(rp_id: &str, rp_origin: &str) -> bool {
    url::Url::parse(rp_origin)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .is_some_and(|host| host.ends_with(rp_id))
}

/// Parse the CAPTCHA provider from a variable lookup.
/// Returns `Ok(None)` if CAPTCHA is disabled (no CAPTCHA_PROVIDER set).
///
/// The lookup is a parameter so the rules can be exercised without mutating
/// process-global environment, which races when tests run in parallel threads.
pub fn parse_captcha<F>(lookup: F) -> anyhow::Result<Option<(String, String, String)>>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup("CAPTCHA_PROVIDER").as_deref() {
        Some(provider @ ("turnstile" | "cloudflare")) => {
            let secret = lookup("CAPTCHA_SECRET_KEY").ok_or_else(|| {
                anyhow::anyhow!("CAPTCHA_SECRET_KEY required when CAPTCHA_PROVIDER is set")
            })?;
            let site_key = lookup("CAPTCHA_SITE_KEY").ok_or_else(|| {
                anyhow::anyhow!("CAPTCHA_SITE_KEY required when CAPTCHA_PROVIDER is set")
            })?;
            Ok(Some((provider.to_string(), secret, site_key)))
        }
        Some(other) => {
            anyhow::bail!("Unknown CAPTCHA_PROVIDER: {other}. Supported: turnstile, cloudflare")
        }
        None => Ok(None),
    }
}

/// Parse the CAPTCHA provider from environment variables.
/// Returns `Ok(None)` if CAPTCHA is disabled (no CAPTCHA_PROVIDER set).
pub fn parse_captcha_env() -> anyhow::Result<Option<(String, String, String)>> {
    parse_captcha(|key| env::var(key).ok())
}

/// Where installed plugins' wasm lives.
///
/// An empty value is treated as unset, not as the empty path. A compose file
/// that declares `ETHPAY_PLUGIN_DIR` and an `.env` that does not fill it in
/// produces an empty string rather than an absent variable - and the empty
/// path resolves relative to the process working directory, so every plugin
/// would be looked for at `./<id>/<version>.wasm` and none would be found.
/// The cost of getting this wrong is paid at the next boot, not at the
/// misconfiguration, which is what makes it worth a line here.
pub fn plugin_dir_from<F>(lookup: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    lookup("ETHPAY_PLUGIN_DIR")
        .filter(|value| !value.trim().is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_PLUGIN_DIR), PathBuf::from)
}

/// The operator's own store, from `ETHPAY_OPERATOR_STORE_ID`, falling back to
/// the deprecated `ETHPAY_BILLING_STORE_ID` when the new name is unset.
///
/// The fallback exists because a rename of this variable is not something a
/// running instance's deploy config picks up on its own: an instance still
/// injecting the old name would otherwise silently lose its own-store
/// reporting on next restart, with no error and no way to tell "never
/// configured" apart from "config went stale under a rename". Once every
/// deployment's config carries the new name, the fallback and this comment
/// can go.
///
/// Absent, blank and unparseable all yield `None`, and all three are logged as
/// nothing rather than guessed at. This id decides which payments a plugin is
/// told about, so the failure mode for a typo has to be "the plugin hears
/// nothing" - noticed quickly and harmlessly - rather than "the plugin hears
/// about some merchant's store", which is a disclosure nobody would spot.
pub fn operator_store_id_from<F>(lookup: F) -> Option<types::StoreId>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = match lookup("ETHPAY_OPERATOR_STORE_ID") {
        Some(raw) => raw,
        None => {
            let raw = lookup("ETHPAY_BILLING_STORE_ID")?;
            tracing::warn!(
                "ETHPAY_BILLING_STORE_ID is deprecated; rename it to ETHPAY_OPERATOR_STORE_ID"
            );
            raw
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match uuid::Uuid::parse_str(trimmed) {
        Ok(id) => Some(types::StoreId(id)),
        Err(e) => {
            tracing::error!(
                error = %e,
                "ETHPAY_OPERATOR_STORE_ID is not a UUID; this instance will report no own-store payments"
            );
            None
        }
    }
}

/// The account that may own the operator store, from
/// `ETHPAY_OPERATOR_ACCOUNT_ID`.
///
/// Absent, blank and unparseable all yield `None`, the same treatment
/// [`operator_store_id_from`] gives its variable and for the same reason: a
/// typo here must fail closed (every nomination refused) rather than fail
/// open (nobody's account required).
pub fn operator_account_id_from<F>(lookup: F) -> Option<types::UserId>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup("ETHPAY_OPERATOR_ACCOUNT_ID")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match uuid::Uuid::parse_str(trimmed) {
        Ok(id) => Some(types::UserId(id)),
        Err(e) => {
            tracing::error!(
                error = %e,
                "ETHPAY_OPERATOR_ACCOUNT_ID is not a UUID; this instance will refuse every operator store nomination"
            );
            None
        }
    }
}

/// Relative on purpose: a development run and a test need no privileged
/// directory to exist. A container image sets `ETHPAY_PLUGIN_DIR` explicitly
/// to a volume that survives a redeploy.
const DEFAULT_PLUGIN_DIR: &str = "./plugins";

/// Whether safe mode (every plugin disabled) was requested, via either
/// `ETHPAY_DISABLE_PLUGINS=1`/`true` or a bare `--disable-plugins` argument.
///
/// The env lookup and the argument list are parameters, like [`parse_captcha`],
/// so this can be exercised without mutating process-global environment or
/// `std::env::args`, which race when tests run in parallel threads.
pub fn safe_mode_requested<F>(lookup: F, args: &[String]) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let env_disabled = lookup("ETHPAY_DISABLE_PLUGINS").is_some_and(|v| v == "true" || v == "1");
    let flag_present = args.iter().any(|a| a == "--disable-plugins");
    env_disabled || flag_present
}

/// `ENABLE_SWAGGER` is on by default; only `true` or `1`, in any letter case,
/// keeps it on once the variable is set. Anything else turns it off.
fn parse_enable_swagger(raw: Option<&str>) -> bool {
    raw.map(|v| matches!(v.to_lowercase().as_str(), "true" | "1"))
        .unwrap_or(true)
}

/// `MAINNET_ENABLED` is OFF by default - the opposite default from
/// `ENABLE_SWAGGER` on purpose. Unset, empty, unreadable, or any value other
/// than exactly `true` or `1` all mean disabled: the admin API handler this
/// gates refuses a mainnet chain id unless this is exactly enabled, so an
/// ambiguous reading here must resolve to the refusing side.
fn mainnet_enabled_requested(raw: Option<&str>) -> bool {
    matches!(raw, Some("true") | Some("1"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn debug_does_not_leak_database_credentials() {
        // DATABASE_URL carries the Postgres password in every deployment and
        // Config derives Debug — SecretString is what keeps `{:?}` safe here.
        let config = Config {
            database_url: SecretString::from(
                "postgres://ethpayserver:hunter2@postgres/ethpayserver".to_string(),
            ),
            redis_url: Some(SecretString::from("redis://:r3dis@redis:6379".to_string())),
            host: "127.0.0.1".to_string(),
            port: 3000,
            log_level: "info".to_string(),
            log_format: "pretty".to_string(),
            enable_swagger: false,
            safe_mode: false,
            mainnet_enabled: false,
            plugin_dir: PathBuf::from(DEFAULT_PLUGIN_DIR),
            operator_store_id: None,
            operator_account_id: None,
        };
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("hunter2"),
            "db password leaked: {rendered}"
        );
        assert!(
            !rendered.contains("r3dis"),
            "redis password leaked: {rendered}"
        );
    }

    // ========================================================================
    // WebAuthn RP ID derivation
    // ========================================================================

    #[test]
    fn derive_rp_id_https() {
        assert_eq!(
            derive_rp_id_from_origin("https://app.example.com"),
            Some("app.example.com".to_string())
        );
    }

    #[test]
    fn derive_rp_id_with_port() {
        assert_eq!(
            derive_rp_id_from_origin("http://localhost:8080"),
            Some("localhost".to_string())
        );
    }

    #[test]
    fn derive_rp_id_ip() {
        assert_eq!(
            derive_rp_id_from_origin("http://192.168.1.1:3000"),
            Some("192.168.1.1".to_string())
        );
    }

    #[test]
    fn derive_rp_id_invalid_url() {
        assert_eq!(derive_rp_id_from_origin("not-a-url"), None);
    }

    #[test]
    fn derive_rp_id_empty() {
        assert_eq!(derive_rp_id_from_origin(""), None);
    }

    // ========================================================================
    // WebAuthn RP ID validation
    // ========================================================================

    #[test]
    fn validate_rp_id_exact_match() {
        assert!(validate_rp_id("example.com", "https://example.com"));
    }

    #[test]
    fn validate_rp_id_subdomain() {
        assert!(validate_rp_id("example.com", "https://app.example.com"));
    }

    #[test]
    fn validate_rp_id_mismatch() {
        assert!(!validate_rp_id("localhost", "https://app.example.com"));
    }

    #[test]
    fn validate_rp_id_localhost() {
        assert!(validate_rp_id("localhost", "http://localhost:8080"));
    }

    // ========================================================================
    // CAPTCHA config parsing
    // ========================================================================

    // These drive `parse_captcha` with an explicit lookup rather than mutating
    // process-global env vars, so they are safe under parallel test threads.
    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn captcha_disabled_when_no_env() {
        assert!(parse_captcha(lookup(&[])).unwrap().is_none());
    }

    #[test]
    fn captcha_turnstile_requires_keys() {
        let result = parse_captcha(lookup(&[("CAPTCHA_PROVIDER", "turnstile")]));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("CAPTCHA_SECRET_KEY")
        );
    }

    #[test]
    fn captcha_turnstile_with_keys() {
        let result = parse_captcha(lookup(&[
            ("CAPTCHA_PROVIDER", "turnstile"),
            ("CAPTCHA_SECRET_KEY", "secret123"),
            ("CAPTCHA_SITE_KEY", "site123"),
        ]))
        .unwrap();
        assert!(result.is_some());
        let (provider, secret, site_key) = result.unwrap();
        assert_eq!(provider, "turnstile");
        assert_eq!(secret, "secret123");
        assert_eq!(site_key, "site123");
    }

    #[test]
    fn captcha_cloudflare_alias() {
        let result = parse_captcha(lookup(&[
            ("CAPTCHA_PROVIDER", "cloudflare"),
            ("CAPTCHA_SECRET_KEY", "s"),
            ("CAPTCHA_SITE_KEY", "k"),
        ]))
        .unwrap();
        assert_eq!(result.unwrap().0, "cloudflare");
    }

    #[test]
    fn captcha_unknown_provider() {
        let result = parse_captcha(lookup(&[("CAPTCHA_PROVIDER", "recaptcha")]));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown CAPTCHA_PROVIDER")
        );
    }

    // ========================================================================
    // Safe mode (plugins disabled)
    // ========================================================================

    /// A declared-but-empty variable is the normal result of a compose file
    /// whose `.env` does not fill it in, and the empty path would send every
    /// artifact lookup to the process working directory instead.
    #[test]
    fn an_empty_plugin_dir_falls_back_to_the_default() {
        assert_eq!(
            plugin_dir_from(lookup(&[("ETHPAY_PLUGIN_DIR", "")])),
            PathBuf::from("./plugins")
        );
        assert_eq!(
            plugin_dir_from(lookup(&[("ETHPAY_PLUGIN_DIR", "   ")])),
            PathBuf::from("./plugins")
        );
    }

    #[test]
    fn plugin_dir_defaults_when_unset_and_is_used_when_set() {
        assert_eq!(plugin_dir_from(lookup(&[])), PathBuf::from("./plugins"));
        assert_eq!(
            plugin_dir_from(lookup(&[(
                "ETHPAY_PLUGIN_DIR",
                "/var/lib/ethpayserver/plugins"
            )])),
            PathBuf::from("/var/lib/ethpayserver/plugins")
        );
    }

    #[test]
    fn safe_mode_off_by_default() {
        assert!(!safe_mode_requested(lookup(&[]), &[]));
    }

    #[test]
    fn mainnet_enabled_off_by_default() {
        assert!(!mainnet_enabled_requested(None));
    }

    #[test]
    fn mainnet_enabled_on_only_for_true_or_1() {
        assert!(mainnet_enabled_requested(Some("true")));
        assert!(mainnet_enabled_requested(Some("1")));
    }

    #[test]
    fn mainnet_enabled_off_for_anything_else() {
        // DELIBERATELY NOT case-insensitive like ENABLE_SWAGGER: unlike a
        // UI toggle, an ambiguous reading of the one setting that re-arms a
        // constant-time defect and real-money chains must resolve to OFF,
        // not to a best guess at what the deployer meant.
        assert!(!mainnet_enabled_requested(Some("TRUE")));
        assert!(!mainnet_enabled_requested(Some("yes")));
        assert!(!mainnet_enabled_requested(Some("")));
        assert!(!mainnet_enabled_requested(Some("0")));
    }

    #[test]
    fn safe_mode_via_env_var_1() {
        assert!(safe_mode_requested(
            lookup(&[("ETHPAY_DISABLE_PLUGINS", "1")]),
            &[]
        ));
    }

    #[test]
    fn safe_mode_via_env_var_true() {
        assert!(safe_mode_requested(
            lookup(&[("ETHPAY_DISABLE_PLUGINS", "true")]),
            &[]
        ));
    }

    #[test]
    fn safe_mode_env_var_other_value_is_not_enabled() {
        assert!(!safe_mode_requested(
            lookup(&[("ETHPAY_DISABLE_PLUGINS", "yes")]),
            &[]
        ));
    }

    #[test]
    fn safe_mode_via_cli_flag() {
        let args = vec!["ethpayserver".to_string(), "--disable-plugins".to_string()];
        assert!(safe_mode_requested(lookup(&[]), &args));
    }

    // ========================================================================
    // Operator store resolution
    // ========================================================================

    #[test]
    fn operator_store_id_unset_is_none() {
        assert_eq!(operator_store_id_from(lookup(&[])), None);
    }

    #[test]
    fn operator_store_id_valid_uuid_is_parsed() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            operator_store_id_from(lookup(&[("ETHPAY_OPERATOR_STORE_ID", &id.to_string())])),
            Some(types::StoreId(id))
        );
    }

    #[test]
    fn operator_store_id_falls_back_to_deprecated_billing_var() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            operator_store_id_from(lookup(&[("ETHPAY_BILLING_STORE_ID", &id.to_string())])),
            Some(types::StoreId(id))
        );
    }

    #[test]
    fn operator_store_id_prefers_new_var_over_deprecated_one() {
        let new_id = uuid::Uuid::new_v4();
        let old_id = uuid::Uuid::new_v4();
        assert_eq!(
            operator_store_id_from(lookup(&[
                ("ETHPAY_OPERATOR_STORE_ID", &new_id.to_string()),
                ("ETHPAY_BILLING_STORE_ID", &old_id.to_string()),
            ])),
            Some(types::StoreId(new_id))
        );
    }

    // ========================================================================
    // Operator account resolution
    // ========================================================================

    #[test]
    fn operator_account_id_unset_is_none() {
        assert_eq!(operator_account_id_from(lookup(&[])), None);
    }

    #[test]
    fn operator_account_id_blank_is_none() {
        assert_eq!(
            operator_account_id_from(lookup(&[("ETHPAY_OPERATOR_ACCOUNT_ID", "   ")])),
            None
        );
    }

    #[test]
    fn operator_account_id_unparseable_is_none() {
        assert_eq!(
            operator_account_id_from(lookup(&[("ETHPAY_OPERATOR_ACCOUNT_ID", "not-a-uuid")])),
            None
        );
    }

    #[test]
    fn operator_account_id_valid_uuid_is_parsed() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            operator_account_id_from(lookup(&[("ETHPAY_OPERATOR_ACCOUNT_ID", &id.to_string())])),
            Some(types::UserId(id))
        );
    }

    // ========================================================================
    // ENABLE_SWAGGER parsing - case-insensitive "true"/"1", on by default
    // ========================================================================

    #[test]
    fn enable_swagger_true_lowercase_is_on() {
        assert!(parse_enable_swagger(Some("true")));
    }

    #[test]
    fn enable_swagger_true_uppercase_is_on() {
        assert!(parse_enable_swagger(Some("TRUE")));
    }

    #[test]
    fn enable_swagger_true_titlecase_is_on() {
        assert!(parse_enable_swagger(Some("True")));
    }

    #[test]
    fn enable_swagger_one_is_on() {
        assert!(parse_enable_swagger(Some("1")));
    }

    #[test]
    fn enable_swagger_false_lowercase_is_off() {
        assert!(!parse_enable_swagger(Some("false")));
    }

    #[test]
    fn enable_swagger_false_uppercase_is_off() {
        assert!(!parse_enable_swagger(Some("FALSE")));
    }

    #[test]
    fn enable_swagger_garbage_is_off() {
        assert!(!parse_enable_swagger(Some("not-a-boolean")));
    }

    #[test]
    fn enable_swagger_unset_defaults_on() {
        assert!(parse_enable_swagger(None));
    }
}
