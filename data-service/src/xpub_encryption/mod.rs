//! Encryption at rest for merchant xpubs, keyed by a master key obtained
//! from a pluggable provider.
//!
//! A leaked xpub cannot move money - non-custodial, no spending key on the
//! server - but it is a total, permanent privacy breach: the holder derives
//! every address the merchant will ever receive payments at. This module is
//! what stands between that and a plain database dump.
//!
//! Deliberately provider-agnostic by default: the AWS-specific provider
//! lives in [`kms`], behind the `kms` cargo feature, and is the only thing
//! in this crate that links the AWS SDK. ethpayserver is an open-source
//! project that has to run on a plain VPS as well as on AWS, so a build
//! without that feature pulls in nothing AWS-specific at all.
//!
//! [`provider_from_env`] is the one place that picks between providers, and
//! every binary that can reach a wallet calls it the same way at startup -
//! `server`, `migrate_postgres` (both in the `server` crate) and `mcp-server`
//! all call through here rather than each repeating the same env-var
//! contract, which is what let this drift out of sync with `mcp-server`
//! once already (it linked `data_service::xpub_encryption` for decryption
//! but never called `init`, since the provider-selection logic used to live
//! in the `server` crate, which `mcp-server` does not and should not depend
//! on just for this).
//!
//! Reuses the same AES-256-CBC + HMAC-SHA256 Encrypt-then-MAC primitive
//! `crypto::symmetric` already provides for `auth`'s client-side encrypted
//! blobs, just keyed by a master key this module obtains instead of one
//! derived from a user's password - the server, not the client, has to be
//! able to decrypt an xpub on every invoice to derive its address.

use std::sync::{Arc, OnceLock};

use crypto::kdf::stretch_master_key;
use crypto::types::{EncryptedBlob, StretchedKey, SymmetricKey};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroize;

#[cfg(feature = "kms")]
pub mod kms;

/// Supplies the 32-byte master key xpubs are encrypted under.
///
/// One implementation per key-custody option - a plain secret for any VPS,
/// AWS KMS for an AWS deployment - so this repo is never locked to one cloud
/// provider.
#[async_trait::async_trait]
pub trait XpubKeyProvider: Send + Sync {
    async fn master_key(&self) -> Result<SymmetricKey, XpubKeyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum XpubKeyError {
    #[error("xpub encryption is not configured: {0}")]
    NotConfigured(String),
    #[error("xpub encryption key provider failed: {0}")]
    Provider(String),
}

/// A master key held directly, rather than fetched from a cloud KMS.
///
/// The self-hosted option: one secret, generated once (e.g. `openssl rand
/// -base64 32`) and handed to the deployment as `XPUB_ENCRYPTION_KEY`. No
/// AWS account, no extra service - this is the whole provider.
pub struct StaticKeyProvider(SymmetricKey);

impl StaticKeyProvider {
    pub fn new(key: SymmetricKey) -> Self {
        Self(key)
    }

    pub fn from_base64(encoded: &str) -> Result<Self, XpubKeyError> {
        use base64::Engine;
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| XpubKeyError::Provider(format!("invalid base64: {e}")))?;
        let key = SymmetricKey::from_slice(&bytes).ok_or_else(|| {
            XpubKeyError::Provider(format!(
                "XPUB_ENCRYPTION_KEY must decode to exactly 32 bytes, got {}",
                bytes.len()
            ))
        });
        // `SymmetricKey::from_slice` copies into its own `Zeroize`-protected
        // storage; this `Vec` is the leftover copy drop alone would not wipe.
        bytes.zeroize();
        Ok(Self(key?))
    }
}

#[async_trait::async_trait]
impl XpubKeyProvider for StaticKeyProvider {
    async fn master_key(&self) -> Result<SymmetricKey, XpubKeyError> {
        Ok(self.0.clone())
    }
}

static PROVIDER: OnceLock<Arc<dyn XpubKeyProvider>> = OnceLock::new();

/// Install the key provider. Call exactly once, as early as possible in a
/// binary's startup - before any route or job can touch a wallet - in every
/// binary that can read or write one (today: `server` and `migrate_postgres`,
/// both in the `server` crate). Nothing below falls back to plaintext if this
/// was never called; every call just fails.
pub fn init(provider: Arc<dyn XpubKeyProvider>) {
    let _ = PROVIDER.set(provider);
}

/// Test-only: a fixed, published, non-secret key. Idempotent, so every test
/// in a process can call it. Never call this from a real binary's startup -
/// only from a test harness.
pub fn init_insecure_test_default() {
    let _ = PROVIDER.set(Arc::new(StaticKeyProvider::new(SymmetricKey([7u8; 32]))));
}

async fn stretched_key() -> Result<StretchedKey, XpubKeyError> {
    let provider = PROVIDER.get().ok_or_else(|| {
        XpubKeyError::NotConfigured(
            "no xpub key provider installed - call xpub_encryption::init() at startup".to_string(),
        )
    })?;
    let master = provider.master_key().await?;
    stretch_master_key(&master).map_err(|e| XpubKeyError::Provider(e.to_string()))
}

/// Encrypt a plaintext xpub for storage, and compute the deterministic
/// fingerprint `postgres/wallet.rs` uses in its place for every uniqueness
/// check and advisory lock.
///
/// Returned together because both need the same stretched key, and an xpub
/// is never encrypted without also needing its fingerprint - splitting this
/// into two calls would just mean deriving that key twice.
pub async fn encrypt_and_fingerprint(
    plaintext: &str,
) -> Result<(EncryptedBlob, Vec<u8>), XpubKeyError> {
    let key = stretched_key().await?;
    let blob = crypto::symmetric::encrypt(plaintext.as_bytes(), &key)
        .map_err(|e| XpubKeyError::Provider(e.to_string()))?;
    let fingerprint = fingerprint_with(&key, plaintext);
    Ok((blob, fingerprint))
}

/// The fingerprint alone, for a lookup that already knows the plaintext but
/// has nothing to encrypt (e.g. "does any other account hold this key").
pub async fn fingerprint(plaintext: &str) -> Result<Vec<u8>, XpubKeyError> {
    let key = stretched_key().await?;
    Ok(fingerprint_with(&key, plaintext))
}

/// Keyed on `mac_key`, not a bare hash: a bare `SHA256(xpub)` would let
/// anyone holding a database dump brute-force-confirm a *guessed* xpub
/// against every row, even without the master key. HMAC makes that
/// confirmation itself require the key the fingerprint is already protected
/// by - the fingerprint reveals nothing that encryption did not already
/// decide to reveal.
fn fingerprint_with(key: &StretchedKey, plaintext: &str) -> Vec<u8> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&key.mac_key).expect("HMAC-SHA256 accepts any key length");
    mac.update(b"xpub-fingerprint:");
    mac.update(plaintext.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Decrypt a stored xpub back to plaintext.
pub async fn decrypt(blob: &EncryptedBlob) -> Result<String, XpubKeyError> {
    let key = stretched_key().await?;
    let bytes = crypto::symmetric::decrypt(blob, &key)
        .map_err(|e| XpubKeyError::Provider(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| XpubKeyError::Provider(e.to_string()))
}

impl From<XpubKeyError> for crate::RepositoryError {
    fn from(e: XpubKeyError) -> Self {
        // Not `Conflict`: nothing the caller sent is wrong, and there is
        // nothing they can change about the request to fix it. `Database`
        // is the closest existing category for "the backing system failed
        // in a way the request had no control over."
        crate::RepositoryError::Database(format!("xpub encryption: {e}"))
    }
}

/// Picks and constructs the key provider from the environment. Required,
/// fail-closed, the same way `SENTRY_DSN` is: a missing provider must never
/// be mistaken for a working one, and unlike SMTP or the signup relay there
/// is no safe no-op here - "no provider configured" cannot mean "store the
/// xpub in the clear," so there is no `Option` return, only success or a
/// startup error naming exactly what to set.
///
/// `XPUB_ENCRYPTION_PROVIDER` selects which one:
/// - `static` - `XPUB_ENCRYPTION_KEY`, 32 random bytes as base64. The whole
///   deployment's key custody story, for anyone not on AWS.
/// - `kms` - `XPUB_KMS_KEY_ID` + `XPUB_KMS_ENCRYPTED_DEK`. See [`kms`] for
///   how the latter is generated. Only available when this crate is built
///   with the `kms` feature.
pub async fn provider_from_env() -> anyhow::Result<Arc<dyn XpubKeyProvider>> {
    let kind = required_env_var("XPUB_ENCRYPTION_PROVIDER").map_err(|_| {
        anyhow::anyhow!(
            "XPUB_ENCRYPTION_PROVIDER is required (\"static\" or \"kms\") - merchant xpubs \
             are encrypted at rest and there is no plaintext fallback"
        )
    })?;

    match kind.as_str() {
        "static" => {
            let key = required_env_var("XPUB_ENCRYPTION_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "XPUB_ENCRYPTION_PROVIDER=static needs XPUB_ENCRYPTION_KEY - 32 random \
                     bytes, base64 (e.g. `openssl rand -base64 32`)"
                )
            })?;
            Ok(Arc::new(StaticKeyProvider::from_base64(&key)?))
        }
        "kms" => provider_from_env_kms().await,
        other => anyhow::bail!("XPUB_ENCRYPTION_PROVIDER={other:?} is not \"static\" or \"kms\""),
    }
}

/// `std::env::var`, treating an empty string the same as unset.
///
/// Compose forwards a variable's absence from `.env` as an empty string
/// (`${VAR:-}`), not as the variable being unset, so `std::env::var` alone
/// only catches a name that was never named in compose at all - the far more
/// likely operator mistake, naming it but never setting it in `.env`, would
/// otherwise reach `KmsKeyProvider::new` as `Ok("")` and fail deep inside an
/// AWS SDK call instead of with the clear message below. Same fix
/// `SignupServiceConfig::from_lookup` already applies to its own env vars.
fn required_env_var(name: &str) -> Result<String, std::env::VarError> {
    std::env::var(name).and_then(|v| {
        if v.is_empty() {
            Err(std::env::VarError::NotPresent)
        } else {
            Ok(v)
        }
    })
}

#[cfg(feature = "kms")]
async fn provider_from_env_kms() -> anyhow::Result<Arc<dyn XpubKeyProvider>> {
    let key_id = required_env_var("XPUB_KMS_KEY_ID")
        .map_err(|_| anyhow::anyhow!("XPUB_ENCRYPTION_PROVIDER=kms needs XPUB_KMS_KEY_ID"))?;
    let encrypted_dek = required_env_var("XPUB_KMS_ENCRYPTED_DEK").map_err(|_| {
        anyhow::anyhow!(
            "XPUB_ENCRYPTION_PROVIDER=kms needs XPUB_KMS_ENCRYPTED_DEK - see \
             xpub_encryption::kms for how to generate it"
        )
    })?;
    Ok(Arc::new(
        kms::KmsKeyProvider::new(&key_id, &encrypted_dek).await?,
    ))
}

#[cfg(not(feature = "kms"))]
async fn provider_from_env_kms() -> anyhow::Result<Arc<dyn XpubKeyProvider>> {
    anyhow::bail!(
        "XPUB_ENCRYPTION_PROVIDER=kms requires data-service's \"kms\" cargo feature, which \
         this binary was not built with"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> Arc<dyn XpubKeyProvider> {
        Arc::new(StaticKeyProvider::new(SymmetricKey([42u8; 32])))
    }

    async fn stretched_key_for_test() -> StretchedKey {
        stretch_master_key(&test_provider().master_key().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn encrypt_then_decrypt_round_trips() {
        init(test_provider());
        let xpub = "xpub6CLVyWpL638492QotBgyB52snVwhRR6coWgShHB7taMnt9TwTbYtXoh9RCs8d5H1W6ib2Y6qtCobNfwxhqKHnM7fL4tqQ2TRnvcYdgzVDn7";
        let (blob, _fp) = encrypt_and_fingerprint(xpub).await.unwrap();
        assert_ne!(blob.ciphertext, xpub.as_bytes());
        let decrypted = decrypt(&blob).await.unwrap();
        assert_eq!(decrypted, xpub);
    }

    #[tokio::test]
    async fn fingerprint_is_deterministic_for_the_same_key_and_plaintext() {
        let key = stretched_key_for_test().await;
        let a = fingerprint_with(&key, "same-xpub");
        let b = fingerprint_with(&key, "same-xpub");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn fingerprint_differs_for_different_plaintext() {
        let key = stretched_key_for_test().await;
        let a = fingerprint_with(&key, "xpub-a");
        let b = fingerprint_with(&key, "xpub-b");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn encryption_uses_a_random_iv_each_time() {
        init(test_provider());
        let xpub = "xpub6same-key-twice";
        let (blob1, fp1) = encrypt_and_fingerprint(xpub).await.unwrap();
        let (blob2, fp2) = encrypt_and_fingerprint(xpub).await.unwrap();
        // Same plaintext, different ciphertext - this is exactly why every
        // uniqueness check in postgres/wallet.rs keys on the fingerprint
        // instead of the encrypted column.
        assert_ne!(blob1.ciphertext, blob2.ciphertext);
        // But the fingerprint, which has no IV, agrees.
        assert_eq!(fp1, fp2);
    }
}
