use super::*;

#[test]
fn server_admin_is_the_admin_viewer() {
    assert_eq!(viewer_for(Role::ServerAdmin), Viewer::Admin);
}

/// Navigation is filtered by role, and the order a plugin declared is
/// the order a menu is built in.
#[test]
fn admin_only_pages_are_offered_only_to_an_admin() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![
        PageDeclaration {
            path: "subscription".to_string(),
            label: "Subscription".to_string(),
            placement: PagePlacement::Nav,
            icon: PageIcon::Plug,
            admin_only: false,
        },
        PageDeclaration {
            path: "subscriptions".to_string(),
            label: "Subscriptions".to_string(),
            placement: PagePlacement::Nav,
            icon: PageIcon::Plug,
            admin_only: true,
        },
    ];

    let merchant = visible_pages(declared.clone(), false);
    assert_eq!(merchant.len(), 1);
    assert_eq!(merchant[0].path, "subscription");

    let admin = visible_pages(declared, true);
    assert_eq!(admin.len(), 2);
    assert_eq!(
        admin.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        vec!["subscription", "subscriptions"],
        "the plugin's declared order is what a menu is built from"
    );
}

/// A page placed in admin settings is admin-only by definition - there is
/// nowhere else it is reachable from. A plugin should not have to say so
/// twice, and a merchant must not be offered it because the author said
/// it only once.
#[test]
fn an_admin_settings_page_is_admin_only_without_saying_so_twice() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscriptions".to_string(),
        label: "All merchants".to_string(),
        placement: PagePlacement::AdminSettings,
        icon: PageIcon::Plug,
        // Deliberately NOT set: the placement alone must be enough.
        admin_only: false,
    }];

    assert!(
        visible_pages(declared.clone(), false).is_empty(),
        "a merchant must not be offered a page about other merchants"
    );
    assert_eq!(visible_pages(declared, true).len(), 1);
}

/// The icon a plugin declares is what a client is told to draw, not
/// whatever the generic default happens to be - otherwise the field
/// could parse correctly in the manifest and still never reach anyone
/// reading the response.
#[test]
fn a_declared_icon_reaches_the_response() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscription".to_string(),
        label: "Subscription".to_string(),
        placement: PagePlacement::Nav,
        icon: PageIcon::Card,
        admin_only: false,
    }];

    let pages = visible_pages(declared, false);
    assert_eq!(pages[0].icon, PluginPageIcon::Card);
}

/// A manifest predating this field, or one written against a newer icon
/// vocabulary than this build knows, both land on [`PageIcon::Plug`]
/// before `visible_pages` ever sees them (that defaulting and fallback
/// is `payserver-plugin-api`'s own, and is tested there). What this repo
/// owns is the mapping onto [`PluginPageIcon`], so this only needs to
/// confirm the default variant survives that mapping unchanged.
#[test]
fn a_page_with_the_default_icon_maps_to_the_generic_one() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscription".to_string(),
        label: "Subscription".to_string(),
        placement: PagePlacement::Nav,
        icon: PageIcon::default(),
        admin_only: false,
    }];

    let pages = visible_pages(declared, false);
    assert_eq!(pages[0].icon, PluginPageIcon::Plug);
}

#[test]
fn everyone_else_is_the_merchant_viewer() {
    assert_eq!(viewer_for(Role::User), Viewer::Merchant);
}

/// A slug resolves to the plugin that declared it, and the id still
/// works - an admin tool, a log line and an older client all carry ids,
/// and breaking those to make a URL pretty would be a poor trade.
#[test]
fn a_manifest_declares_the_slug_a_url_is_built_from() {
    let manifest: Manifest = r#"
        id = "cash.random.billing"
        version = "0.1.0"
        dependencies = ["ethpayserver:^1.2.0"]
        kind = "filter"
        slug = "billing"

        [[pages]]
        path = "subscriptions"
        label = "Subscriptions"
    "#
    .parse()
    .unwrap();

    assert_eq!(manifest.slug.as_ref().unwrap().as_str(), "billing");
    assert_eq!(
        manifest.id.as_str(),
        "cash.random.billing",
        "the id is unchanged - the slug is an addition, not a rename"
    );
}

/// The lookup validates before it compares, so a traversal attempt never
/// reaches a manifest at all. Without this, `..` would be compared
/// against every installed plugin's slug - harmless today, and exactly
/// the sort of input that should be refused at the door rather than
/// relied on to match nothing.
#[test]
fn a_malformed_slug_is_refused_before_any_lookup() {
    use payserver_plugin_api::PluginSlug;

    for bad in ["..", "../admin", "bil/ling", "Billing", ""] {
        assert!(
            PluginSlug::new(bad).is_err(),
            "{bad:?} must not survive to be compared against a manifest"
        );
    }
    assert!(PluginSlug::new("billing").is_ok());
}
