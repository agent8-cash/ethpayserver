use super::*;
use auth::Policies;

// These tests used to assert a `requested_scope` helper that mapped an empty
// permissions list to "absent". The helper existed because the wire type was a
// `Vec<String>` with `#[serde(default)]`, which made an absent field and `[]`
// the SAME VALUE by the time the handler saw them - so the handler had to
// guess, and it guessed "inherit the owner's role".
//
// `permissions` is now `Option<Vec<String>>`, so the distinction is carried by
// deserialization rather than reconstructed after the fact. That is where it is
// now worth pinning: the helper has no subject left, but the mapping from JSON
// to the two cases is exactly what a future `#[serde(default)]` would silently
// undo.
//
// Written without `expect`/`unwrap` because this module is compiled into the
// library's test target, where clippy denies both.

/// Deserialize a payload and hand back the `permissions` field, or a reason the
/// JSON was not accepted at all - so a parse failure reads as a parse failure
/// rather than as a scope that happened to be absent.
fn permissions_of(json: &str) -> Result<Option<Vec<String>>, String> {
    serde_json::from_str::<CreateApiKeyPayload>(json)
        .map(|p| p.permissions)
        .map_err(|e| e.to_string())
}

/// No `permissions` key at all - what the dashboard posts - must arrive as
/// `None`, which the handler stores as NULL: inherits the owner's role.
#[test]
fn an_absent_permissions_field_deserializes_to_none() {
    assert_eq!(
        permissions_of(r#"{"name":"dashboard key"}"#),
        Ok(None),
        "an absent field must stay absent; serde(default) on a Vec is what used to erase it"
    );
}

/// `"permissions": []` is a DIFFERENT request and must survive as `Some([])`.
/// Collapsing it to `None` is what handed a caller asking for no permissions
/// the owner's full role instead.
#[test]
fn an_explicitly_empty_permissions_field_deserializes_to_some_empty() {
    assert_eq!(
        permissions_of(r#"{"name":"narrow","permissions":[]}"#),
        Ok(Some(vec![])),
        "an explicitly empty scope is a request for a key that can do nothing, \
         not a request for no opinion"
    );
}

/// An explicit scope passes through unchanged - the only way to get a narrowed
/// key, and it has to be asked for.
#[test]
fn an_explicit_scope_deserializes_unchanged() {
    let json = format!(
        r#"{{"name":"scoped","permissions":["{}"]}}"#,
        Policies::STORE_VIEW_INVOICES
    );
    assert_eq!(
        permissions_of(&json),
        Ok(Some(vec![Policies::STORE_VIEW_INVOICES.to_string()])),
    );
}

/// `["unrestricted"]` is an explicit scope too and must not read as absent. It
/// means the same thing today, but it says so deliberately, and a reader of the
/// row can tell a key nobody scoped from one scoped to everything on purpose.
#[test]
fn an_explicit_unrestricted_scope_is_not_absent() {
    let json = format!(
        r#"{{"name":"everything","permissions":["{}"]}}"#,
        Policies::UNRESTRICTED
    );
    let got = permissions_of(&json);
    assert_eq!(got, Ok(Some(vec![Policies::UNRESTRICTED.to_string()])),);
    assert_ne!(
        got,
        Ok(None),
        "scoped-to-everything is not the same row as nobody-scoped-it"
    );
}

/// An empty scope must still pass validation - it is a legal request, just a
/// maximally narrow one. If this ever returned 400, `permissions: []` would be
/// unaskable and the distinction above would be decorative.
#[test]
fn an_empty_scope_is_a_legal_request() {
    assert!(validate_requested_permissions(&[]).is_ok());
}
