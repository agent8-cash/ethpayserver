use super::*;
use auth::Policies;

/// The dashboard posts `{name, expires_at}` and no `permissions` at all, which
/// `#[serde(default)]` turns into an empty vec. Persisting that as `Some([])`
/// would hand back a key that authenticates and is then refused every
/// permission check, with a 201 and a key on screen to save.
#[test]
fn an_absent_scope_is_stored_as_inherit_not_as_an_empty_scope() {
    assert_eq!(requested_scope(None), None);
}

/// An explicitly empty list is the same request as an absent one today.
#[test]
fn an_explicitly_empty_scope_is_stored_as_inherit() {
    assert_eq!(requested_scope(Some(&[])), None);
}

/// An explicit scope passes through unchanged - this is the only way to get a
/// narrowed key, and it has to be asked for.
#[test]
fn an_explicit_scope_is_passed_through() {
    let asked = [Policies::STORE_VIEW_INVOICES.to_string()];
    assert_eq!(requested_scope(Some(&asked)), Some(asked.as_slice()));
}

/// `["unrestricted"]` is an explicit scope too and must not collapse into the
/// absent case. It means the same thing today, but it says so deliberately, and
/// a reader of the row can tell a key nobody scoped from one scoped to
/// everything on purpose.
#[test]
fn an_explicit_unrestricted_scope_is_not_flattened_to_absent() {
    let asked = [Policies::UNRESTRICTED.to_string()];
    assert_eq!(requested_scope(Some(&asked)), Some(asked.as_slice()));
}
