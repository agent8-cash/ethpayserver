use super::*;

/// The page endpoint, through the router a request actually reaches.
///
/// The route has been mounted for a while and always answered 404,
/// because `PageHost` was built empty and nothing ever registered a
/// renderer in it. That is indistinguishable from a feature that was
/// never wired up, and it was one: a unit test of `PageHost` passes
/// either way. This asks over HTTP, with a renderer registered, so it
/// fails if either half goes missing.
#[tokio::test]
async fn a_registered_renderer_is_reachable_over_http() {
    use payserver_plugin_api::page::{Badge, Tone};
    use payserver_plugin_host::{PageHost, PageRenderError, PageRenderer};

    struct Billing;

    #[async_trait]
    impl PageRenderer for Billing {
        async fn render_page(
            &self,
            request: &PageRequest,
        ) -> Result<Option<PageElement>, PageRenderError> {
            if request.path != "subscriptions" {
                return Ok(None);
            }
            // Echoes both halves of the identity back, so the test can
            // assert the host resolved them rather than the plugin.
            Ok(Some(PageElement::Badge(Badge {
                text: format!(
                    "{:?}/{}",
                    request.viewer,
                    request.account_id.as_deref().unwrap_or("none")
                ),
                tone: Tone::Info,
            })))
        }
    }

    let mut pages = PageHost::new();
    pages.register(
        PluginId::new("cash.random.billing").unwrap(),
        Arc::new(Billing),
    );

    let mut state = test_state();
    state.plugin_pages = Arc::new(pages);

    let app = Router::new()
        .route(
            "/plugins/{id}/pages/{*path}",
            axum::routing::get(get_page::<FakeSessions>),
        )
        .with_state(state);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/subscriptions")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a registered renderer must be reachable through the mounted route"
    );

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let element: PageElement = serde_json::from_slice(&body).unwrap();
    let PageElement::Badge(badge) = element else {
        panic!("expected the badge the renderer returned");
    };
    assert_eq!(
        badge.text,
        format!("Merchant/{}", uuid::Uuid::from_u128(1)),
        "both the viewer and the account must come from the authenticated \
         session, not from anything the request claimed"
    );

    // A path the plugin does not serve is still a 404, so the test above
    // is not passing because everything answers 200.
    let missing = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/not-a-page")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(missing).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

/// `Figure` survives the round trip through the page endpoint as
/// `type: figure`, not the `#[serde(other)] Unknown` fallback.
///
/// Same shape as `a_registered_renderer_is_reachable_over_http`, but for
/// the element this commons pin adds: on an older pin a plugin page
/// containing a `figure` deserialized into `Unknown` server-side and
/// re-serialized as `{"type":"unknown"}`, silently dropping the label,
/// value and tone a dashboard tile needs. Ablation: pin
/// `payserver-commons` back to a revision before `PageElement::Figure`
/// existed and this test fails because the type does not compile, let
/// alone round-trip.
#[tokio::test]
async fn a_figure_round_trips_through_the_page_endpoint_as_figure_not_unknown() {
    use payserver_plugin_api::page::{Figure, Tone};
    use payserver_plugin_host::{PageHost, PageRenderError, PageRenderer};

    struct Billing;

    #[async_trait]
    impl PageRenderer for Billing {
        async fn render_page(
            &self,
            request: &PageRequest,
        ) -> Result<Option<PageElement>, PageRenderError> {
            if request.path != "summary" {
                return Ok(None);
            }
            Ok(Some(PageElement::Figure(Figure {
                label: "Lapsed".to_string(),
                value: "7".to_string(),
                tone: Tone::Danger,
            })))
        }
    }

    let mut pages = PageHost::new();
    pages.register(
        PluginId::new("cash.random.billing").unwrap(),
        Arc::new(Billing),
    );

    let mut state = test_state();
    state.plugin_pages = Arc::new(pages);

    let app = Router::new()
        .route(
            "/plugins/{id}/pages/{*path}",
            axum::routing::get(get_page::<FakeSessions>),
        )
        .with_state(state);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/summary")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();

    // Asserted on the raw JSON first: this is the exact failure mode the
    // old pin produced — a 200 whose body is `{"type":"unknown"}`, which
    // would still deserialize into `PageElement::Unknown` below and could
    // be mistaken for a passing test if only the typed value were
    // checked.
    let raw: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        raw["type"], "figure",
        "a figure element must serialize as type: figure, not fall back to unknown; got {raw}"
    );

    let element: PageElement = serde_json::from_slice(&body).unwrap();
    let PageElement::Figure(figure) = element else {
        panic!("expected the figure the renderer returned, got {element:?}");
    };
    assert_eq!(figure.label, "Lapsed");
    assert_eq!(figure.value, "7");
    assert_eq!(figure.tone, Tone::Danger);
}

/// A plugin that could not answer is a 502, not a 404. Answering "no
/// such page" for a billing page that merely trapped sends whoever is
/// debugging it looking for a routing mistake that is not there.
#[test]
fn a_broken_plugin_is_a_bad_gateway_not_a_missing_page() {
    use axum::response::IntoResponse;
    use payserver_plugin_host::PageRenderError;

    let unavailable: ApiErr = PageError::Unavailable(PageRenderError::new("wasm trap")).into();
    let missing: ApiErr = PageError::PageNotFound.into();

    assert_eq!(
        unavailable.into_response().status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(missing.into_response().status(), StatusCode::NOT_FOUND);
}
