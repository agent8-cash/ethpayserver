//! Email service for sending payment receipts to customers.
//!
//! Configured via environment variables. When SMTP is not configured,
//! the service is a no-op.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// SMTP configuration parsed from environment variables.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    /// Optional configuration set name. When present, every outgoing message
    /// is attributed to it so the provider reports bounce and complaint events.
    pub ses_configuration_set: Option<String>,
}

impl SmtpConfig {
    /// Try to load SMTP config from environment. Returns None if required
    /// variables are not set (SMTP is optional).
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("SMTP_HOST").ok()?;
        let port = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        let username = std::env::var("SMTP_USERNAME").ok()?;
        let password = std::env::var("SMTP_PASSWORD").ok()?;
        let from = std::env::var("SMTP_FROM").unwrap_or_else(|_| format!("noreply@{host}"));
        let ses_configuration_set = std::env::var("SES_CONFIGURATION_SET").ok();
        Some(Self {
            host,
            port,
            username,
            password,
            from,
            ses_configuration_set,
        })
    }
}

/// Receipt data needed to compose the email.
pub struct ReceiptData {
    pub invoice_id: String,
    pub amount: String,
    pub currency: String,
    pub tx_hash: String,
    pub explorer_url: String,
    pub paid_at: DateTime<Utc>,
    pub merchant_name: String,
}

/// Data needed to compose an email-change verification message.
pub struct EmailChangeVerificationData {
    /// The single-use code the merchant pastes back into Settings to confirm.
    pub token: String,
    /// How long the token remains redeemable, for the reader's benefit.
    pub expires_in_minutes: i64,
}

/// A message to an account holder, composed by a caller that names no
/// address - see `crate::services::plugins::account_notice` (capability 7).
/// `subject` and `body` are sent verbatim: the caller is trusted to have
/// written something a merchant should read, the same trust `FilterVerdict::
/// Deny`'s `reason` is given today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountNotice {
    pub subject: String,
    pub body: String,
}

/// Email service that sends payment receipts via SMTP.
pub struct EmailService {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
    configuration_set: Option<String>,
}

/// The `X-SES-CONFIGURATION-SET` header. Without it the provider attributes
/// the message to no configuration set and emits no bounce or complaint events.
#[derive(Debug, Clone)]
struct ConfigurationSetHeader(String);

impl Header for ConfigurationSetHeader {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("X-SES-CONFIGURATION-SET")
    }

    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.to_owned()))
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), self.0.clone())
    }
}

impl EmailService {
    /// Build an `EmailService` from an `SmtpConfig`.
    pub fn new(config: &SmtpConfig) -> Result<Self, lettre::transport::smtp::Error> {
        let creds = Credentials::new(config.username.clone(), config.password.clone());
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)?
            .port(config.port)
            .credentials(creds)
            .build();
        Ok(Self {
            transport,
            from: config.from.clone(),
            configuration_set: config.ses_configuration_set.clone(),
        })
    }

    /// Assemble a message from this service's sender, attributed to the
    /// configuration set when one is configured.
    fn build_message(
        &self,
        to: &str,
        subject: String,
        body: String,
    ) -> Result<Message, EmailError> {
        let builder = Message::builder()
            .from(self.from.parse().map_err(|_| EmailError::InvalidFrom)?)
            .to(to.parse().map_err(|_| EmailError::InvalidRecipient)?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN);
        let builder = match &self.configuration_set {
            Some(set) => builder.header(ConfigurationSetHeader(set.clone())),
            None => builder,
        };
        builder
            .body(body)
            .map_err(|e| EmailError::Build(e.to_string()))
    }

    /// Compose the payment receipt for the customer.
    fn receipt_message(&self, to: &str, data: &ReceiptData) -> Result<Message, EmailError> {
        let subject = format!(
            "Payment receipt — {} {} ({})",
            data.amount, data.currency, data.invoice_id
        );

        let body = format!(
            "Payment Receipt\n\
             ================\n\
             \n\
             Merchant:    {merchant}\n\
             Invoice:     {invoice}\n\
             Amount:      {amount} {currency}\n\
             Transaction: {tx}\n\
             Explorer:    {explorer}\n\
             Paid at:     {paid_at}\n\
             \n\
             This is an automated receipt from {merchant} powered by random.cash.\n\
             If you did not make this payment, please contact the merchant.\n",
            merchant = data.merchant_name,
            invoice = data.invoice_id,
            amount = data.amount,
            currency = data.currency,
            tx = data.tx_hash,
            explorer = data.explorer_url,
            paid_at = data.paid_at.format("%Y-%m-%d %H:%M:%S UTC"),
        );

        self.build_message(to, subject, body)
    }

    /// Send a payment receipt to the customer.
    pub async fn send_receipt(&self, to: &str, data: &ReceiptData) -> Result<(), EmailError> {
        let email = self.receipt_message(to, data)?;
        self.transport
            .send(email)
            .await
            .map_err(|e| EmailError::Send(e.to_string()))?;

        Ok(())
    }

    /// Compose a notice to an account holder.
    fn account_notice_message(
        &self,
        to: &str,
        notice: &AccountNotice,
    ) -> Result<Message, EmailError> {
        self.build_message(to, notice.subject.clone(), notice.body.clone())
    }

    /// Send a notice to an account holder. The caller supplies `to` itself -
    /// this is the SMTP-facing implementation the capability's host-side
    /// lookup delivers through, not a second place that resolves an address.
    pub async fn send_account_notice(
        &self,
        to: &str,
        notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        let email = self.account_notice_message(to, notice)?;
        self.transport
            .send(email)
            .await
            .map_err(|e| EmailError::Send(e.to_string()))?;

        Ok(())
    }

    /// Compose the verification code for a pending email-address change.
    fn email_change_verification_message(
        &self,
        to: &str,
        data: &EmailChangeVerificationData,
    ) -> Result<Message, EmailError> {
        let subject = "Confirm your new email address".to_string();

        let body = format!(
            "We received a request to use this address for a random.cash account.\n\
             \n\
             Verification code: {token}\n\
             \n\
             Enter this code in Account Settings to confirm the change. It expires in \
             {minutes} minutes and can only be used once.\n\
             \n\
             If you did not request this, no action is needed - the address will not \
             change unless this code is entered.\n",
            token = data.token,
            minutes = data.expires_in_minutes,
        );

        self.build_message(to, subject, body)
    }

    /// Send the verification code for a pending email-address change.
    pub async fn send_email_change_verification(
        &self,
        to: &str,
        data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        let email = self.email_change_verification_message(to, data)?;
        self.transport
            .send(email)
            .await
            .map_err(|e| EmailError::Send(e.to_string()))?;

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("invalid from address")]
    InvalidFrom,
    #[error("invalid recipient address")]
    InvalidRecipient,
    #[error("email build error: {0}")]
    Build(String),
    #[error("smtp send error: {0}")]
    Send(String),
    #[error("email is not configured on this host")]
    NotConfigured,
}

/// Trait abstracting email sending for testability.
#[async_trait::async_trait]
pub trait EmailSender: Send + Sync {
    async fn send_receipt(&self, to: &str, data: &ReceiptData) -> Result<(), EmailError>;

    async fn send_email_change_verification(
        &self,
        to: &str,
        data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError>;

    /// Send a notice to an account holder. Like email-change verification and
    /// unlike a receipt, this must not silently succeed when nothing was
    /// actually sent: a caller that reads `Ok(())` here believes an account
    /// has been notified, and that belief is the exact failure this
    /// capability exists to close.
    async fn send_account_notice(&self, to: &str, notice: &AccountNotice)
    -> Result<(), EmailError>;

    /// Whether this sender actually delivers mail.
    ///
    /// A receipt is best-effort - nobody's payment fails because a customer
    /// email quietly did not send - so `NoopEmailSender` answering `Ok(())`
    /// there is the right default. It is exactly the wrong default for
    /// confirming an email-address change: a pending change that never
    /// arrives leaves the address stuck with no error anywhere, so that
    /// caller must check this before it creates the pending state at all,
    /// rather than trust the sender's return value.
    fn is_configured(&self) -> bool;
}

#[async_trait::async_trait]
impl EmailSender for EmailService {
    async fn send_receipt(&self, to: &str, data: &ReceiptData) -> Result<(), EmailError> {
        self.send_receipt(to, data).await
    }

    async fn send_email_change_verification(
        &self,
        to: &str,
        data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        self.send_email_change_verification(to, data).await
    }

    async fn send_account_notice(
        &self,
        to: &str,
        notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        self.send_account_notice(to, notice).await
    }

    fn is_configured(&self) -> bool {
        true
    }
}

/// No-op email sender for when SMTP is not configured.
pub struct NoopEmailSender;

#[async_trait::async_trait]
impl EmailSender for NoopEmailSender {
    async fn send_receipt(&self, _to: &str, _data: &ReceiptData) -> Result<(), EmailError> {
        Ok(())
    }

    async fn send_email_change_verification(
        &self,
        _to: &str,
        _data: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        Ok(())
    }

    async fn send_account_notice(
        &self,
        _to: &str,
        _notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        // Unlike a receipt, silently succeeding here is the failure this
        // capability exists to close: a caller reading `Ok(())` believes an
        // account was warned. `is_configured` already exists for exactly this
        // check (see `request_email_change`); this is its second caller.
        Err(EmailError::NotConfigured)
    }

    fn is_configured(&self) -> bool {
        false
    }
}

/// Create the appropriate email sender based on environment configuration.
pub fn create_email_sender() -> Arc<dyn EmailSender> {
    match SmtpConfig::from_env() {
        Some(config) => match EmailService::new(&config) {
            Ok(service) => {
                tracing::info!(host = %config.host, "SMTP email service configured");
                Arc::new(service)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to configure SMTP, receipts disabled");
                Arc::new(NoopEmailSender)
            }
        },
        None => {
            tracing::info!("SMTP not configured, customer receipts disabled");
            Arc::new(NoopEmailSender)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property `request_email_change` (server/src/api/users/email.rs) relies
    /// on to fail loudly instead of queuing a change nobody can confirm: a
    /// no-op sender must say so, unlike its `Ok(())` from `send_receipt`.
    #[test]
    fn noop_sender_reports_itself_unconfigured() {
        assert!(!NoopEmailSender.is_configured());
    }

    /// The same property as above, at the call a plugin (once this is
    /// reachable from one) actually makes: a host with no SMTP configured
    /// must return an error, not `Ok(())` that reads as "the account was
    /// warned".
    #[tokio::test]
    async fn an_unconfigured_sender_refuses_an_account_notice_rather_than_pretending() {
        let notice = AccountNotice {
            subject: "You are approaching your bracket".to_string(),
            body: "…".to_string(),
        };
        let result = NoopEmailSender
            .send_account_notice("merchant@example.com", &notice)
            .await;
        assert!(matches!(result, Err(EmailError::NotConfigured)));
    }

    #[test]
    fn a_real_sender_reports_itself_configured() {
        let config = SmtpConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            username: "user".to_string(),
            password: "pass".to_string(),
            from: "noreply@example.com".to_string(),
            ses_configuration_set: None,
        };
        // Building the transport only assembles config - it never connects,
        // so this cannot fail for a reason the test needs to handle.
        #[allow(
            clippy::expect_used,
            reason = "transport construction cannot fail without a real network call"
        )]
        let service = EmailService::new(&config).expect("build transport");
        assert!(service.is_configured());
    }

    fn config_with_set(set: Option<&str>) -> SmtpConfig {
        SmtpConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            username: "user".to_string(),
            password: "pass".to_string(),
            from: "noreply@example.com".to_string(),
            ses_configuration_set: set.map(str::to_string),
        }
    }

    fn receipt() -> ReceiptData {
        ReceiptData {
            invoice_id: "inv_1".to_string(),
            amount: "1.00".to_string(),
            currency: "EUR".to_string(),
            tx_hash: "0xabc".to_string(),
            explorer_url: "https://explorer.example.com/tx/0xabc".to_string(),
            paid_at: Utc::now(),
            merchant_name: "Merchant".to_string(),
        }
    }

    /// Every outgoing message, built by each of the three send paths, carries
    /// the configured set so the provider reports its bounce events.
    #[test]
    fn each_send_path_attributes_messages_to_the_configured_set()
    -> Result<(), Box<dyn std::error::Error>> {
        let service = EmailService::new(&config_with_set(Some("ag8-receipts")))?;
        let notice = AccountNotice {
            subject: "Notice".to_string(),
            body: "Body".to_string(),
        };
        let verification = EmailChangeVerificationData {
            token: "123456".to_string(),
            expires_in_minutes: 15,
        };
        let messages = [
            service.receipt_message("buyer@example.com", &receipt())?,
            service.account_notice_message("owner@example.com", &notice)?,
            service.email_change_verification_message("owner@example.com", &verification)?,
        ];
        for message in &messages {
            assert_eq!(
                message.headers().get_raw("X-SES-CONFIGURATION-SET"),
                Some("ag8-receipts")
            );
        }
        Ok(())
    }

    /// Without a configured set the header is omitted, so an unconfigured host
    /// does not attribute mail to a set it does not own.
    #[test]
    fn no_send_path_sets_the_header_when_no_set_is_configured()
    -> Result<(), Box<dyn std::error::Error>> {
        let service = EmailService::new(&config_with_set(None))?;
        let notice = AccountNotice {
            subject: "Notice".to_string(),
            body: "Body".to_string(),
        };
        let receipt_message = service.receipt_message("buyer@example.com", &receipt())?;
        let notice_message = service.account_notice_message("owner@example.com", &notice)?;
        assert_eq!(
            receipt_message.headers().get_raw("X-SES-CONFIGURATION-SET"),
            None
        );
        assert_eq!(
            notice_message.headers().get_raw("X-SES-CONFIGURATION-SET"),
            None
        );
        Ok(())
    }
}
