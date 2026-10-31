//! Capability 7: tell an account something, without ever handing a plugin a
//! recipient to name.
//!
//! Before this, a plugin's only way to reach a merchant at all was the
//! checkout itself: refuse an invoice and let the refusal text carry the
//! whole message. A merchant who never opens the page that would have shown
//! them a warning hears nothing until that refusal - which is the same
//! moment a customer's payment fails.
//!
//! A plugin cannot send email. It has no host capability that takes a
//! recipient, and it should not get one - a plugin holding an arbitrary
//! address is a mail relay wearing a billing plugin's name. What this grants
//! instead is narrower: an account id the host already recognises, and a
//! subject and body the host delivers through whichever channel and to
//! whichever address it looks up itself. The plugin never sees either.
//!
//! No new type here for the message content - see [`AccountNotice`] in
//! `crate::services::email`, re-exported alongside the other capabilities'
//! names, the same way capability 1 re-exports `data_service`'s type instead
//! of duplicating it.
//!
//! Right now there is exactly one channel: the address on the account, by
//! email. A wallet-only account has none, and [`notice_address`] says so
//! rather than the notice being silently dropped - a caller that cannot tell
//! "delivered" from "nowhere to deliver to" would have no way to decide
//! whether that is worth surfacing to an operator.
//!
//! Reachable from a plugin now. `payserver-plugin-host`'s `PluginHostCalls`
//! trait has an `account_notice` method and a matching wasm import
//! (`define_answering_call`, the same helper `invoice_create` and
//! `merchant_volume` use), tested the same way those two are - instantiate a
//! module that imports only `account_notice` and check the request reaches
//! that method and no other. This repo's side,
//! [`super::host_calls::PluginCalls::account_notice_impl`], is the same
//! shape as `issuer`/`volume` there: a `DeferredNotifier` cell, since
//! building a [`PluginAccountNotifier`] needs the full `PgAppState` that is
//! not ready at plugin-registration time either, calling
//! [`AccountNotifier::notify_account`] from it. `server.rs` publishes the
//! cell unconditionally - unlike `issuer`, this needs no operator store, only
//! the instance's own user directory and email sender, both of which exist
//! on every instance.
//!
//! Deciding *when* to call this is a second, independent gap: the
//! thresholds that would trigger a warning are a billing plan's own config,
//! not host state. Neither this repository nor `payserver-commons` names a
//! plan, a bracket or a per-plan warning window anywhere (`git grep` finds
//! nothing in either checkout). The host side of invoice creation is
//! structurally blind to the same thing - `InvoiceCreationFilterRequest`
//! carries a store id and an account id and nothing else, so even the
//! decision to *refuse* an invoice already happens entirely inside the
//! plugin, opaque to this process. That decision belongs entirely to the
//! billing plugin's own source, which is in neither checkout this worker
//! has. A caller added here would have to invent the trigger it is calling
//! on, which is a second product decision wearing this ticket's name.

use async_trait::async_trait;
use auth::{SessionService, UserRepository};
use types::UserId;

use crate::services::email::AccountNotice;
use crate::state::PgAppState;

/// Tell one account something, through a channel and an address the caller
/// never names.
#[async_trait]
pub trait AccountNotifier: Send + Sync {
    /// # Errors
    /// A message describing why the account could not be reached - it does
    /// not exist, has no address on any channel this host can use, or
    /// delivery itself failed.
    async fn notify_account(
        &self,
        account_id: UserId,
        notice: &AccountNotice,
    ) -> Result<(), String>;
}

/// The capability, over the instance's own user directory and email sender.
pub struct PluginAccountNotifier<A> {
    state: PgAppState<A>,
}

impl<A> PluginAccountNotifier<A> {
    #[must_use]
    pub fn new(state: PgAppState<A>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl<A: SessionService + 'static> AccountNotifier for PluginAccountNotifier<A> {
    async fn notify_account(
        &self,
        account_id: UserId,
        notice: &AccountNotice,
    ) -> Result<(), String> {
        let user = UserRepository::get_user(&*self.state.data_service, account_id)
            .await
            .map_err(|e| format!("could not read this account: {e}"))?
            .ok_or_else(|| format!("{account_id} is not an account on this instance"))?;

        let email = notice_address(user.email)?;

        self.state
            .email_sender
            .send_account_notice(&email, notice)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The address this capability would deliver a notice to, or why it cannot.
///
/// Pulled out as a pure function so the one property that matters here - a
/// whitespace-only email is exactly as absent as no email at all - has a
/// test that needs no database. A wallet-only account (`email: None`) is the
/// ordinary case this refuses, not an edge case.
fn notice_address(email: Option<String>) -> Result<String, String> {
    match email.map(|e| e.trim().to_string()) {
        Some(email) if !email.is_empty() => Ok(email),
        _ => Err(
            "this account has no email on file; there is no channel this host can reach it through"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use data_service::test_support::pg_service;
    use std::sync::{Arc, Mutex};

    use auth::{Result as AuthResult, Session, SessionId, UserInfo};

    use super::*;
    use crate::services::email::{
        EmailChangeVerificationData, EmailError, EmailSender, ReceiptData,
    };
    use crate::state::PgAppState;

    #[test]
    fn an_account_with_an_email_is_notifiable() {
        assert_eq!(
            notice_address(Some("merchant@example.com".to_string())),
            Ok("merchant@example.com".to_string())
        );
    }

    /// A wallet-only account is the ordinary case, not an edge case: it has
    /// no email at all, and that must be a named refusal rather than a
    /// notice that silently goes nowhere.
    #[test]
    fn a_wallet_only_account_has_no_channel() {
        let Err(err) = notice_address(None) else {
            panic!("an account with no email must not resolve to an address");
        };
        assert!(err.contains("no email on file"), "{err}");
    }

    /// Whitespace is not an address. Treating it as one would hand the email
    /// transport a string that is not a mailbox, and the failure would come
    /// back from SMTP instead of naming the actual cause.
    #[test]
    fn a_blank_email_is_treated_as_no_email() {
        let Err(err) = notice_address(Some("   ".to_string())) else {
            panic!("a whitespace-only email must not resolve to an address");
        };
        assert!(err.contains("no email on file"), "{err}");
    }

    // =====================================================================
    // notify_account - through the trait method itself, against a real
    // database.
    //
    // Every test above exercises `notice_address` directly. None would
    // notice if `notify_account` stopped calling it, read the wrong field
    // off `User`, swallowed a repository error, or called the sender with
    // the wrong address. `#[ignore]`d and skipped with no `DATABASE_URL`,
    // matching every other database-backed test in this codebase - but not
    // silently: CI's integration-test job runs `-p server --run-ignored
    // only` with `DATABASE_URL` set, so these three do run and do gate
    // merges, the same as the rest of that job.
    // =====================================================================

    /// Exists only to give `PgAppState<A>` a concrete auth-service type;
    /// `notify_account` never calls it.
    struct UnusedSessionService;

    #[async_trait]
    impl SessionService for UnusedSessionService {
        async fn validate_session(
            &self,
            _session_id: SessionId,
        ) -> AuthResult<(UserInfo, Session)> {
            unimplemented!("not exercised by notify_account")
        }
        async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
            unimplemented!("not exercised by notify_account")
        }
        async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
            unimplemented!("not exercised by notify_account")
        }
        async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
            unimplemented!("not exercised by notify_account")
        }
    }

    /// Records every call instead of actually sending, so a test can assert
    /// on the address and notice `notify_account` handed it - the one thing
    /// a `NoopEmailSender` or a real SMTP transport can't tell a test.
    #[derive(Default)]
    struct RecordingEmailSender {
        calls: Mutex<Vec<(String, AccountNotice)>>,
    }

    #[async_trait]
    impl EmailSender for RecordingEmailSender {
        async fn send_receipt(&self, _to: &str, _data: &ReceiptData) -> Result<(), EmailError> {
            unimplemented!("not exercised by notify_account")
        }

        async fn send_email_change_verification(
            &self,
            _to: &str,
            _data: &EmailChangeVerificationData,
        ) -> Result<(), EmailError> {
            unimplemented!("not exercised by notify_account")
        }

        async fn send_account_notice(
            &self,
            to: &str,
            notice: &AccountNotice,
        ) -> Result<(), EmailError> {
            self.calls
                .lock()
                .unwrap()
                .push((to.to_string(), notice.clone()));
            Ok(())
        }

        fn is_configured(&self) -> bool {
            true
        }
    }

    /// `kdf_params`/`encrypted_symmetric_key` need real shape, not `{}`:
    /// `get_user` deserialises both into `crypto::KdfParams`/`EncryptedBlob`,
    /// and a `{}` blob fails there before `notify_account` ever gets a
    /// `User` to read `email` off.
    async fn seed_user(pool: &sqlx::PgPool, email: Option<&str>) -> UserId {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, email, kdf_params, encrypted_symmetric_key, \
             recovery_verification_hash, kdf_salt_identifier) \
             VALUES ($1, $2, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
             '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
        )
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .expect("seed user");
        UserId(id)
    }

    fn notifier(
        service: data_service::PgDataService,
        sender: Arc<RecordingEmailSender>,
    ) -> PluginAccountNotifier<UnusedSessionService> {
        let state = PgAppState::new(
            Arc::new(service),
            Arc::new(UnusedSessionService),
            None,
            Arc::new(rates::NoOpRateProvider),
            sender,
        );
        PluginAccountNotifier::new(state)
    }

    /// The happy path: an account with an email is delivered to, through
    /// `notify_account` itself rather than `notice_address` in isolation.
    #[tokio::test]
    #[ignore]
    async fn notify_account_delivers_to_the_accounts_own_email() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        // Unique per run: `email` is unique on `users`, and the same fixture
        // is shared with every other test in this suite.
        let email = format!("merchant-{}@example.com", uuid::Uuid::new_v4());
        let account_id = seed_user(&pool, Some(email.as_str())).await;
        let sender = Arc::new(RecordingEmailSender::default());
        let api = notifier(service, sender.clone());

        let notice = AccountNotice {
            subject: "You are approaching your bracket".to_string(),
            body: "…".to_string(),
        };
        api.notify_account(account_id, &notice)
            .await
            .expect("an account with an email must be notifiable");

        let calls = sender.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "must deliver exactly once");
        assert_eq!(calls[0].0, email);
        assert_eq!(calls[0].1, notice);
    }

    /// A wallet-only account, reached through the real method rather than
    /// `notice_address` directly - proves `notify_account` actually threads
    /// `User::email` through rather than, say, always resolving `Some`.
    #[tokio::test]
    #[ignore]
    async fn notify_account_refuses_a_wallet_only_account() {
        let service = pg_service().await;
        let pool = service.pool().clone();
        let account_id = seed_user(&pool, None).await;
        let sender = Arc::new(RecordingEmailSender::default());
        let api = notifier(service, sender.clone());

        let notice = AccountNotice {
            subject: "subject".to_string(),
            body: "body".to_string(),
        };
        let err = api
            .notify_account(account_id, &notice)
            .await
            .expect_err("a wallet-only account has no channel to notify through");
        assert!(err.contains("no email on file"), "{err}");
        assert!(sender.calls.lock().unwrap().is_empty());
    }

    /// An account id that names nobody must be a named refusal, not a panic
    /// on `Option::unwrap` or a silent no-op that looks like success.
    #[tokio::test]
    #[ignore]
    async fn notify_account_refuses_an_account_that_does_not_exist() {
        let service = pg_service().await;
        let sender = Arc::new(RecordingEmailSender::default());
        let missing = UserId(uuid::Uuid::new_v4());
        let api = notifier(service, sender.clone());

        let notice = AccountNotice {
            subject: "subject".to_string(),
            body: "body".to_string(),
        };
        let err = api
            .notify_account(missing, &notice)
            .await
            .expect_err("an unknown account id must not resolve to a notice sent");
        assert!(err.contains("is not an account on this instance"), "{err}");
        assert!(sender.calls.lock().unwrap().is_empty());
    }
}
