//! AWS KMS-backed xpub encryption key provider.
//!
//! Envelope encryption: a single 32-byte Data Encryption Key (DEK) is
//! generated once, out of band, with:
//!
//! ```sh
//! aws kms generate-data-key --key-id <your-cmk> --key-spec AES_256 \
//!   --query CiphertextBlob --output text
//! ```
//!
//! Only that command's output - the KMS-wrapped ciphertext, never the DEK
//! itself - is persisted, as `XPUB_KMS_ENCRYPTED_DEK`. At startup this
//! provider calls `kms:Decrypt` exactly once to unwrap it into memory; every
//! xpub encrypt/decrypt after that is a local AES operation, not a KMS call.
//! That keeps KMS off the hot path - a `kms:Decrypt` per invoice would be
//! real added latency and real per-request AWS cost - and means a KMS outage
//! blocks startup rather than every payment already in flight.
//!
//! This is the one AWS-specific piece of xpub encryption in the whole
//! codebase, gated behind the `kms` feature for exactly that reason. The
//! shipped `server`/`mcp-server` binaries enable the feature regardless of
//! deployment target - one binary supports either provider at runtime via
//! `XPUB_ENCRYPTION_PROVIDER` - so a VPS deployment that sets `static`
//! simply never *calls* any of this, not never *links* it. A consumer that
//! wants the AWS SDK genuinely absent from the binary builds `data-service`
//! itself without this feature.

use aws_sdk_kms::primitives::Blob;
use base64::Engine;
use crypto::types::SymmetricKey;
use zeroize::Zeroize;

use super::{XpubKeyError, XpubKeyProvider};

pub struct KmsKeyProvider {
    master_key: SymmetricKey,
}

impl KmsKeyProvider {
    /// `key_id` is the CMK that wrapped the DEK; `encrypted_dek_base64` is
    /// that wrapping's ciphertext, base64-encoded (`XPUB_KMS_ENCRYPTED_DEK`).
    pub async fn new(key_id: &str, encrypted_dek_base64: &str) -> anyhow::Result<Self> {
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(encrypted_dek_base64.trim())
            .map_err(|e| anyhow::anyhow!("XPUB_KMS_ENCRYPTED_DEK is not valid base64: {e}"))?;

        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_kms::Client::new(&sdk_config);

        let output = client
            .decrypt()
            .key_id(key_id)
            .ciphertext_blob(Blob::new(ciphertext))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("KMS decrypt of the xpub DEK failed: {e}"))?;

        let mut plaintext = output
            .plaintext
            .ok_or_else(|| anyhow::anyhow!("KMS decrypt returned no plaintext"))?
            .into_inner();

        let master_key = SymmetricKey::from_slice(&plaintext).ok_or_else(|| {
            anyhow::anyhow!(
                "the unwrapped xpub DEK is {} bytes, expected 32 - was \
                 XPUB_KMS_ENCRYPTED_DEK generated with --key-spec AES_256?",
                plaintext.len()
            )
        });
        // `SymmetricKey::from_slice` copies the bytes into its own
        // `Zeroize`-protected storage; this `Vec` is the leftover copy that
        // drop alone would leave sitting in the heap.
        plaintext.zeroize();

        Ok(Self {
            master_key: master_key?,
        })
    }
}

#[async_trait::async_trait]
impl XpubKeyProvider for KmsKeyProvider {
    async fn master_key(&self) -> Result<SymmetricKey, XpubKeyError> {
        Ok(self.master_key.clone())
    }
}
