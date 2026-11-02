//! Whether an API key's stored permission scope grants a given action.
//!
//! Split out of `extractors` - these are pure predicates over `ApiKeyAuthInfo`
//! data with no axum/request dependency of their own, and their unit tests were
//! most of that file's size.

use auth::Policies;

use ::types::StoreId;

/// Whether a key's stored permission scope still grants everything its
/// owner's role would. `None` (unscoped - every key from before per-key
/// scoping existed, and any key nobody has narrowed) and an explicit
/// `unrestricted` entry both count; any other stored set does not, even if
/// it lists individual server permissions, because nothing in this server
/// gates on those individually today - see `validate_api_key`.
pub(super) fn key_retains_unrestricted_access(permissions: Option<&[String]>) -> bool {
    match permissions {
        None => true,
        Some(set) => set.iter().any(|p| p == Policies::UNRESTRICTED),
    }
}

/// Whether a key's stored scope grants reading the server-wide merchant
/// listing: unscoped, `unrestricted`, or naming `ethpay.server.canviewusers`.
///
/// That policy is the only server-level one enforced individually, and only
/// by the routes that call this; every other server policy still grants
/// nothing (see `validate_requested_permissions`).
pub(super) fn key_grants_merchant_read(permissions: Option<&[String]>) -> bool {
    key_retains_unrestricted_access(permissions)
        || permissions.is_some_and(|set| set.iter().any(|p| p == Policies::SERVER_VIEW_USERS))
}

/// The scope entry that lets a key push an account's standing, and nothing
/// else. Not a commons policy: none of those means "write standing", and a
/// key-creation-only string keeps this server from needing a commons change
/// before it can enforce it.
pub(super) const STANDING_PUSH_SCOPE: &str = "ethpay.server.canpushstanding";

/// Whether a key's stored scope names the standing-push entry.
///
/// Unlike `key_grants_merchant_read`, unscoped and `unrestricted` keys do NOT
/// count: the sender must hold a credential that cannot be used for anything
/// else, so an admin key is refused rather than accepted by default.
pub(super) fn key_grants_standing_push(permissions: Option<&[String]>) -> bool {
    permissions.is_some_and(|set| set.iter().any(|p| p == STANDING_PUSH_SCOPE))
}

/// The scope entry that lets a key ask the host to notify an account
/// holder, and nothing else. Mirrors [`STANDING_PUSH_SCOPE`]: a dedicated
/// string rather than a commons policy, so this server can enforce it
/// without a commons change.
pub(super) const ACCOUNT_NOTICE_SCOPE: &str = "ethpay.server.cansendaccountnotice";

/// Whether a key's stored scope names the account-notice entry.
///
/// Unscoped and `unrestricted` keys do NOT count, for the same reason as
/// [`key_grants_standing_push`]: the sender must hold a credential good for
/// nothing else, since it can name any account id on the instance.
pub(super) fn key_grants_account_notice(permissions: Option<&[String]>) -> bool {
    permissions.is_some_and(|set| set.iter().any(|p| p == ACCOUNT_NOTICE_SCOPE))
}

/// Whether a key's stored scope grants `policy` on `store_id` - the other
/// half of "effective permission is the intersection of the key's set and
/// the owner's role", specifically for store permissions.
///
/// Unlike `ethpay.server.*`/`ethpay.user.*`, store policies ARE enforced
/// individually today: `user_has_store_permission` checks one directly in
/// SQL. So a call site gates a store action on both this AND that check,
/// never this alone - a key can never exceed its owner, and this only ever
/// narrows what the owner's own store membership already allows.
///
/// `None` (unscoped key, same as `key_retains_unrestricted_access`) and an
/// `unrestricted` entry both grant everything. A bare policy string (e.g.
/// `"ethpay.store.cancreateinvoice"`) grants it on every store the owner can
/// reach - today's behaviour, and the default so an unscoped grant does not
/// silently start requiring a store to be named. `"policy:storeId"` grants
/// it only on that one store, matching BTCPay's own scoping convention.
pub(super) fn key_grants_store_permission(
    permissions: Option<&[String]>,
    policy: &str,
    store_id: StoreId,
) -> bool {
    let Some(set) = permissions else {
        return true;
    };
    set.iter().any(|entry| {
        if entry == Policies::UNRESTRICTED || entry == policy {
            return true;
        }
        match entry.split_once(':') {
            // Parsed rather than string-compared: a store id was only ever
            // validated as `Uuid::parse_str`-able when the key was written
            // (`is_store_scope_entry` in `users.rs`), not canonicalized, so
            // upper-case hex or a form without hyphens parses fine but would
            // never string-match `Uuid::to_string()`'s lower-case hyphenated
            // output - silently denying a legitimately-scoped grant.
            Some((entry_policy, entry_store_id)) => {
                entry_policy == policy
                    && uuid::Uuid::parse_str(entry_store_id).is_ok_and(|id| id == store_id.0)
            }
            None => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::Permission;

    #[test]
    fn a_key_never_scoped_keeps_unrestricted_access() {
        // NULL permissions: every key before this column existed, and any
        // key nobody has deliberately narrowed since.
        assert!(key_retains_unrestricted_access(None));
    }

    #[test]
    fn a_key_explicitly_marked_unrestricted_keeps_full_access() {
        let perms = vec![Policies::UNRESTRICTED.to_string()];
        assert!(key_retains_unrestricted_access(Some(&perms)));
    }

    #[test]
    fn a_key_scoped_to_specific_permissions_loses_admin_access() {
        // Selecting individual server permissions is not the same as
        // `unrestricted` - nothing in this server gates on them
        // individually, so a key like this authenticates as a plain User.
        let perms = vec![Policies::SERVER_MANAGE_TOKENS.to_string()];
        assert!(!key_retains_unrestricted_access(Some(&perms)));
    }

    #[test]
    fn a_key_scoped_to_an_empty_set_loses_admin_access() {
        // The default for a newly created key: nothing was selected.
        assert!(!key_retains_unrestricted_access(Some(&[])));
    }

    #[test]
    fn unrestricted_sentinel_matches_the_permission_enums_own_policy_string() {
        // This module checks the raw `Policies::UNRESTRICTED` string;
        // users.rs's validate_requested_permissions checks
        // `Permission::Unrestricted.as_policy()` instead. Pin them equal so
        // a future edit to either can't silently desync a key that was
        // written as "unrestricted" from the one check that retains it.
        assert_eq!(Permission::Unrestricted.as_policy(), Policies::UNRESTRICTED);
    }

    fn store(n: u128) -> StoreId {
        StoreId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn merchant_read_is_granted_by_no_scope_unrestricted_or_its_own_policy() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(key_grants_merchant_read(None));
        assert!(key_grants_merchant_read(Some(&strs(&["unrestricted"]))));
        assert!(key_grants_merchant_read(Some(&strs(&[
            "ethpay.store.cancreateinvoice",
            "ethpay.server.canviewusers",
        ]))));
        assert!(!key_grants_merchant_read(Some(&strs(&[
            "ethpay.store.cancreateinvoice"
        ]))));
        assert!(!key_grants_merchant_read(Some(&[])));
        assert!(!key_grants_merchant_read(Some(&strs(&[
            "ethpay.server.canmanageusers"
        ]))));
    }

    #[test]
    fn standing_push_is_granted_only_by_its_own_entry() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(key_grants_standing_push(Some(&strs(&[
            "ethpay.server.canpushstanding"
        ]))));
        assert!(
            !key_grants_standing_push(None),
            "an unscoped key is an admin key"
        );
        assert!(!key_grants_standing_push(Some(&strs(&["unrestricted"]))));
        assert!(!key_grants_standing_push(Some(&strs(&[
            "ethpay.server.canviewusers"
        ]))));
        assert!(!key_grants_standing_push(Some(&[])));
    }

    #[test]
    fn account_notice_is_granted_only_by_its_own_entry() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(key_grants_account_notice(Some(&strs(&[
            "ethpay.server.cansendaccountnotice"
        ]))));
        assert!(
            !key_grants_account_notice(None),
            "an unscoped key is an admin key"
        );
        assert!(!key_grants_account_notice(Some(&strs(&["unrestricted"]))));
        assert!(!key_grants_account_notice(Some(&strs(&[
            "ethpay.server.canviewusers"
        ]))));
        assert!(!key_grants_account_notice(Some(&[])));
    }

    #[test]
    fn an_unscoped_key_grants_every_store_permission() {
        assert!(key_grants_store_permission(
            None,
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
    }

    #[test]
    fn unrestricted_grants_every_store_permission() {
        let perms = vec![Policies::UNRESTRICTED.to_string()];
        assert!(key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
    }

    #[test]
    fn a_bare_store_policy_grants_it_on_any_store() {
        let perms = vec![Policies::STORE_CREATE_INVOICE.to_string()];
        assert!(key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
        assert!(key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(2)
        ));
    }

    #[test]
    fn a_store_scoped_policy_is_refused_on_a_different_store() {
        let perms = vec![format!("{}:{}", Policies::STORE_CREATE_INVOICE, store(1).0)];
        assert!(key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
        assert!(!key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(2)
        ));
    }

    #[test]
    fn a_store_scoped_policy_matches_regardless_of_uuid_casing() {
        // `is_store_scope_entry` only checks `Uuid::parse_str` succeeds when a
        // key is written, not that the stored suffix is already in
        // `Uuid::to_string()`'s canonical lower-case hyphenated form - so an
        // upper-case entry must still match a lower-case query.
        let perms = vec![format!(
            "{}:{}",
            Policies::STORE_CREATE_INVOICE,
            store(1).0.to_string().to_uppercase()
        )];
        assert!(key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
    }

    #[test]
    fn a_grant_for_a_different_action_does_not_grant_this_one() {
        let perms = vec![Policies::STORE_VIEW_SETTINGS.to_string()];
        assert!(!key_grants_store_permission(
            Some(&perms),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
    }

    #[test]
    fn an_empty_scope_grants_no_store_permission() {
        assert!(!key_grants_store_permission(
            Some(&[]),
            Policies::STORE_CREATE_INVOICE,
            store(1)
        ));
    }
}
