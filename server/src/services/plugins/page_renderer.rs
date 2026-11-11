//! Capability 5: a plugin draws its own page.
//!
//! A plugin cannot ship Rust into an already-compiled Leptos client, so it
//! ships a [`PageElement`] tree and the client draws it. `PageHost` is the
//! registry of who answers for which plugin id; this is the implementation
//! that answers by actually running the plugin.
//!
//! Until this existed, [`PageRenderer`] had no production implementation at
//! all. `PageHost` was constructed empty and every page request 404'd -
//! correct behaviour for a host with nothing to render, and indistinguishable
//! from a feature that was never wired up. It was the latter.
//!
//! # What the plugin is asked, and what it may answer
//!
//! ```json
//! {"path": "subscriptions", "viewer": "merchant"}
//! ```
//!
//! and it answers a `PageElement`, or `null` for a path it does not serve.
//! The viewer is the host's to decide, never the plugin's to claim: it comes
//! from the authenticated session in `api::plugins`, and this layer only
//! passes it along. A plugin that could name its own viewer could ask to be
//! treated as an admin.
//!
//! # Why a failure is not an empty page
//!
//! `run_query`, not `run_filter`. A filter that cannot run falls back to its
//! manifest's failure mode because it still owes a verdict; a page has no
//! such default. Answering an empty page for a plugin that trapped would put
//! a blank panel where a merchant expects to see what they owe, which is the
//! single worst thing this layer can produce - so the failure travels and the
//! client shows that instead.

use std::sync::Arc;

use async_trait::async_trait;
use payserver_plugin_api::PluginId;
use payserver_plugin_host::{
    PageElement, PageRenderError, PageRenderer, PageRequest, PluginHost, Viewer,
};
use serde::{Deserialize, Serialize};

/// The export asked to draw a page.
pub const RENDER_PAGE: &str = "render_page";

/// A page action triggered by a merchant on a plugin-rendered page.
///
/// The action_id and data come from a form submission on the client.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PageAction {
    /// The action identifier chosen by the plugin
    pub action_id: String,
    /// Form field values collected by the client, optional
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// An effect the plugin requests after handling an action.
///
/// Shipped now for forward compatibility; validation is deferred to a follow-up.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    /// Show a transient message to the user
    Toast { message: String },
    /// Navigate to a different page
    Navigate { url: String },
    /// Refresh the current page
    Refresh,
}

/// A plugin's response after handling a page action.
///
/// The plugin reports whether the action succeeded, and optionally a
/// user-facing message and a reason code for failures. Optionally includes
/// an effect to request from the host.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PageActionResponse {
    /// Whether the action succeeded
    pub success: bool,
    /// Optional user-facing message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional technical reason for failure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional effect requested by the plugin
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
}

#[derive(Debug, Serialize)]
struct WirePageRequest<'a> {
    path: &'a str,
    viewer: Viewer,
    /// Who is asking, as the host resolved them. A plugin needs this to
    /// show a merchant their own anything, and must never be able to supply
    /// it - one that could name an account could read another merchant's
    /// bill.
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<&'a str>,
}

/// What a plugin answers a page request with.
///
/// A failure travels **in the answer**, not in the return value. The ABI packs
/// `(ptr << 32) | len` into the i64 an export returns and has no
/// negative-means-error convention there - that belongs to the host-call
/// direction. A plugin that returned -1 to mean "I failed" had it read as
/// `ptr = len = 0xFFFFFFFF`, which the runtime reports as a malformed answer
/// and counts against the failure budget: three page loads disabled the
/// plugin outright. A plugin that cannot draw one page must not be able to
/// switch itself off by saying so.
///
/// Untagged, and the order matters: `{"error": ...}` is tried first, so a
/// page can never be mistaken for a failure, and `null` and a real element
/// both fall through to `Page`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WirePageAnswer {
    Failed { error: String },
    Page(Option<PageElement>),
}

/// Renders one plugin's pages by calling its `render_page` export.
pub struct WasmPageRenderer {
    host: Arc<PluginHost>,
    plugin: PluginId,
}

impl WasmPageRenderer {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, plugin: PluginId) -> Self {
        Self { host, plugin }
    }
}

#[async_trait]
impl PageRenderer for WasmPageRenderer {
    async fn render_page(
        &self,
        request: &PageRequest,
    ) -> Result<Option<PageElement>, PageRenderError> {
        let wire = WirePageRequest {
            path: &request.path,
            viewer: request.viewer,
            account_id: request.account_id.as_deref(),
        };

        let answer = self
            .host
            .run_query::<_, WirePageAnswer>(&self.plugin, RENDER_PAGE, &wire)
            .await
            .map_err(|reason| self.refused(&request.path, reason))?;

        match answer {
            WirePageAnswer::Page(page) => Ok(page),
            WirePageAnswer::Failed { error } => Err(self.refused(&request.path, error)),
        }
    }
}

impl WasmPageRenderer {
    /// One place to log a refusal, so the two ways a page can fail - the call
    /// not completing, and the plugin saying it could not - read the same in
    /// the log and to the caller.
    fn refused(&self, path: &str, reason: String) -> PageRenderError {
        // Logged with the plugin id because the message the caller gets is
        // deliberately about the page, not about wasm.
        tracing::warn!(
            plugin_id = %self.plugin,
            path,
            reason = %reason,
            "a plugin could not render one of its pages"
        );
        PageRenderError::new(reason)
    }
}

/// The export invoked when a page action is triggered.
pub const HANDLE_ACTION: &str = "handle_action";

/// A page action request sent to a plugin.
///
/// The action identifier and data come from the client's form submission; the
/// viewer and account_id come from the authenticated session and cannot be
/// forged. A plugin receives these as JSON and decides what to do.
#[derive(Debug, Clone)]
pub struct PageActionRequest {
    /// The page path the action originated from
    pub path: String,
    /// The action the client requested, including any form data
    pub action: PageAction,
    /// Who the client is authenticated as (from session, never client-supplied)
    pub viewer: Viewer,
    /// The authenticated account id (from session, never client-supplied)
    pub account_id: Option<String>,
}

/// Wire format for action requests sent to plugins.
#[derive(Debug, Serialize)]
struct WirePageActionRequest<'a> {
    path: &'a str,
    /// The action identifier chosen by the plugin
    action_id: &'a str,
    /// Form field values collected by the client
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a serde_json::Value>,
    /// Who the host says is asking (never client-supplied)
    viewer: Viewer,
    /// Account id from the session (never client-supplied)
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<&'a str>,
}

/// Wire format for action responses from plugins.
///
/// Mirrors [`PageActionResponse`] but allows for wire-level errors
/// (plugin crashed, timed out, etc) to be distinguished from application
/// errors (action ran but reported failure).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WirePageActionAnswer {
    /// Plugin reported a wire-level error (trapped, etc)
    Failed { error: String },
    /// Plugin ran and reported success/failure with optional message
    Response(PageActionResponse),
}

/// Handles page actions by calling a plugin's `handle_action` export.
pub struct WasmPageActionHandler {
    host: Arc<PluginHost>,
    plugin: PluginId,
}

impl WasmPageActionHandler {
    /// Creates a handler for one plugin's actions.
    #[must_use]
    pub fn new(host: Arc<PluginHost>, plugin: PluginId) -> Self {
        Self { host, plugin }
    }

    /// Dispatches an action to the plugin.
    ///
    /// Returns the plugin's response if it ran, or an error if the plugin
    /// crashed, timed out, or did not implement the export.
    pub async fn handle_action(
        &self,
        request: &PageActionRequest,
    ) -> Result<PageActionResponse, PageActionError> {
        let wire = WirePageActionRequest {
            path: &request.path,
            action_id: &request.action.action_id,
            data: request.action.data.as_ref(),
            viewer: request.viewer,
            account_id: request.account_id.as_deref(),
        };

        let answer = self
            .host
            .run_query::<_, WirePageActionAnswer>(&self.plugin, HANDLE_ACTION, &wire)
            .await
            .map_err(|reason| self.action_failed(&request.path, reason))?;

        match answer {
            WirePageActionAnswer::Response(response) => Ok(response),
            WirePageActionAnswer::Failed { error } => Err(self.action_failed(&request.path, error)),
        }
    }

    /// Logs an action failure and returns a standardized error.
    fn action_failed(&self, path: &str, reason: String) -> PageActionError {
        tracing::warn!(
            plugin_id = %self.plugin,
            path,
            reason = %reason,
            "a plugin could not handle a page action"
        );
        PageActionError::new(reason)
    }
}

/// Error handling a page action.
#[derive(Debug, Clone)]
pub struct PageActionError(String);

impl PageActionError {
    /// Creates a new action error from a reason string.
    fn new(reason: String) -> Self {
        Self(reason)
    }

    /// Returns the error reason for logging or display.
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PageActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PageActionError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The viewer must cross as the host's own vocabulary. A plugin matching
    /// on `"merchant"` and receiving `"Merchant"` would fall through to its
    /// default branch, which for a billing page is the wrong audience.
    #[test]
    fn the_request_names_the_viewer_the_way_the_page_vocabulary_does() {
        let json = serde_json::to_string(&WirePageRequest {
            path: "subscriptions",
            viewer: Viewer::Merchant,
            account_id: Some("acct-7"),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"path":"subscriptions","viewer":"merchant","account_id":"acct-7"}"#
        );

        let admin = serde_json::to_string(&WirePageRequest {
            path: "subscriptions",
            viewer: Viewer::Admin,
            account_id: None,
        })
        .unwrap();
        assert!(admin.contains(r#""viewer":"admin""#), "{admin}");
        assert!(
            !admin.contains("account_id"),
            "an absent identity must be absent, not null: {admin}"
        );
    }

    /// `null` is how a plugin says "not one of my pages", and it has to
    /// survive as `None` rather than failing to parse - otherwise an unknown
    /// path reports a broken plugin instead of a missing page.
    #[test]
    fn a_null_answer_is_a_missing_page_not_a_parse_failure() {
        let parsed: WirePageAnswer = serde_json::from_str("null").unwrap();
        assert!(matches!(parsed, WirePageAnswer::Page(None)));
    }

    /// The path that shipped untested and cost a live outage. A plugin
    /// reports a failure in its answer; returning a negative from the export
    /// instead had the runtime read it as `ptr = len = 0xFFFFFFFF`, call the
    /// answer malformed, and disable the plugin after three page loads.
    #[test]
    fn a_plugin_can_report_a_failure_without_looking_broken() {
        let parsed: WirePageAnswer =
            serde_json::from_str(r#"{"error":"the database refused the query"}"#).unwrap();
        let WirePageAnswer::Failed { error } = parsed else {
            panic!("an error answer must not parse as a page");
        };
        assert_eq!(error, "the database refused the query");
    }

    /// The untagged order has to keep a page out of the failure arm. A page
    /// that parsed as a failure would take the plugin down for rendering
    /// correctly.
    #[test]
    fn a_real_page_never_parses_as_a_failure() {
        let parsed: WirePageAnswer =
            serde_json::from_str(r#"{"type":"section","title":"Your subscription"}"#).unwrap();
        assert!(
            matches!(parsed, WirePageAnswer::Page(Some(PageElement::Section(_)))),
            "a section must parse as a page"
        );
    }

    /// An answer this client does not recognise must still render. The
    /// vocabulary grows, and a plugin built against a newer host would
    /// otherwise produce a page that fails to parse here and 500s.
    #[test]
    fn an_unrecognised_element_parses_as_unknown_rather_than_failing() {
        let parsed: WirePageAnswer =
            serde_json::from_str(r#"{"type":"hologram","glow":true}"#).unwrap();
        assert!(matches!(
            parsed,
            WirePageAnswer::Page(Some(PageElement::Unknown))
        ));
    }

    /// A page action request serializes with correct names for viewer
    /// and field references.
    #[test]
    fn page_action_request_names_viewer_and_account_correctly() {
        let request = WirePageActionRequest {
            path: "subscriptions",
            action_id: "cancel",
            data: None,
            viewer: Viewer::Merchant,
            account_id: Some("acct-7"),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"path":"subscriptions","action_id":"cancel","viewer":"merchant","account_id":"acct-7"}"#
        );

        let admin_request = WirePageActionRequest {
            path: "overview",
            action_id: "export",
            data: None,
            viewer: Viewer::Admin,
            account_id: None,
        };
        let admin_json = serde_json::to_string(&admin_request).unwrap();
        assert!(admin_json.contains(r#""viewer":"admin""#));
        assert!(!admin_json.contains("account_id"));
    }

    /// A page action request with form data serializes the data correctly.
    #[test]
    fn page_action_request_with_data_serializes() {
        let data = serde_json::json!({"amount": "100", "reason": "upgrade"});
        let request = WirePageActionRequest {
            path: "billing",
            action_id: "update_amount",
            data: Some(&data),
            viewer: Viewer::Merchant,
            account_id: Some("acct-42"),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains(r#""action_id":"update_amount""#));
        assert!(json.contains(r#""amount":"100""#));
        assert!(json.contains(r#""reason":"upgrade""#));
    }

    /// A successful action response parses correctly.
    #[test]
    fn page_action_success_response_parses() {
        let json = r#"{"success":true,"message":"Subscription cancelled"}"#;
        let parsed: WirePageActionAnswer = serde_json::from_str(json).unwrap();
        match parsed {
            WirePageActionAnswer::Response(response) => {
                assert!(response.success);
                assert_eq!(response.message.as_deref(), Some("Subscription cancelled"));
            }
            _ => panic!("expected Response, not Failed"),
        }
    }

    /// A failed action response with reason parses correctly.
    #[test]
    fn page_action_failure_response_parses() {
        let json = r#"{"success":false,"message":"Cannot cancel","reason":"invoice_pending"}"#;
        let parsed: WirePageActionAnswer = serde_json::from_str(json).unwrap();
        match parsed {
            WirePageActionAnswer::Response(response) => {
                assert!(!response.success);
                assert_eq!(response.message.as_deref(), Some("Cannot cancel"));
                assert_eq!(response.reason.as_deref(), Some("invoice_pending"));
            }
            _ => panic!("expected Response, not Failed"),
        }
    }

    /// A plugin that reports a wire-level error (trapped, etc) is
    /// distinguished from an application error.
    #[test]
    fn page_action_wire_error_parses_as_failed() {
        let json = r#"{"error":"plugin trapped at 0x1f4"}"#;
        let parsed: WirePageActionAnswer = serde_json::from_str(json).unwrap();
        match parsed {
            WirePageActionAnswer::Failed { error } => {
                assert!(error.contains("trapped"));
            }
            _ => panic!("expected Failed, not Response"),
        }
    }

    /// An action response with no optional fields serializes minimal.
    #[test]
    fn page_action_minimal_response_serializes() {
        let response = PageActionResponse {
            success: true,
            message: None,
            reason: None,
            effect: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"success":true}"#);
    }

    /// An action response with an effect serializes correctly.
    #[test]
    fn page_action_response_with_effect_serializes() {
        let response = PageActionResponse {
            success: true,
            message: Some("Payment processed".to_string()),
            reason: None,
            effect: Some(Effect::Refresh),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""success":true"#));
        assert!(json.contains(r#""message":"Payment processed""#));
        assert!(json.contains(r#""type":"refresh""#));
    }
}
