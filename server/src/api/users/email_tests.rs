use super::*;

#[test]
fn accepts_ordinary_addresses() {
    assert!(looks_like_an_email("merchant@example.com"));
    assert!(looks_like_an_email("  merchant@example.com  "));
}

#[test]
fn rejects_obviously_wrong_shapes() {
    for bad in [
        "",
        "no-at-sign",
        "@nodomain.com",
        "nolocal@",
        "two@ats@example.com",
        "nodot@localhost",
        "trailing@dot.",
        "leading@.dot.com",
    ] {
        assert!(!looks_like_an_email(bad), "{bad:?} should not pass");
    }
}

#[test]
fn removal_is_refused_with_no_wallet_to_fall_back_on() {
    assert!(matches!(
        email_removal_blocker(true, false),
        Some((StatusCode::CONFLICT, _))
    ));
}

#[test]
fn removal_is_allowed_when_a_wallet_remains() {
    assert!(
        email_removal_blocker(true, true).is_none(),
        "a wallet is still a recovery handle, so removal is safe"
    );
}

#[test]
fn removing_an_email_that_is_not_there_is_refused_too() {
    assert!(matches!(
        email_removal_blocker(false, true),
        Some((StatusCode::BAD_REQUEST, _))
    ));
}

#[test]
fn status_reports_an_unconfigured_server_as_not_configured() {
    let status = email_status(&crate::services::email::NoopEmailSender);
    assert!(!status.configured);
    assert_eq!(status.change_ttl_minutes, 30);
}

#[test]
fn status_reports_a_server_with_smtp_as_configured() {
    let config = crate::services::email::SmtpConfig {
        host: "smtp.example.com".to_string(),
        port: 587,
        username: "user".to_string(),
        password: "pass".to_string(),
        from: "noreply@example.com".to_string(),
    };
    // Building the transport only assembles config; it never connects.
    #[allow(
        clippy::expect_used,
        reason = "transport construction cannot fail without a real network call"
    )]
    let service = crate::services::email::EmailService::new(&config).expect("build transport");
    let status = email_status(&service);
    assert!(status.configured);
    assert_eq!(status.change_ttl_minutes, 30);
}
