//! Unit tests for auth repository implementations.

use chrono::{Duration, Utc};

use auth::{
    Device, DeviceId, DeviceType, Role, Session, SessionId, User, UserId, WalletChallenge,
    WalletCredential, WalletCredentialId,
};
use crypto::{EncryptedBlob, KdfParams};

use super::{db_to_device_type, device_type_to_db};

// =========================================================================
// Test helpers - shared with integration tests
// =========================================================================

pub(super) fn test_kdf_params() -> KdfParams {
    KdfParams {
        algorithm: "argon2id".to_string(),
        memory_kb: 65536,
        iterations: 3,
        parallelism: 1,
        salt: vec![0u8; 16],
    }
}

pub(super) fn test_encrypted_blob() -> EncryptedBlob {
    EncryptedBlob {
        ciphertext: vec![1, 2, 3, 4],
        iv: vec![0u8; 16],
        mac: vec![0u8; 32],
    }
}

pub(crate) fn test_user() -> User {
    User {
        id: UserId::new(),
        email: Some("test@example.com".to_string()),
        kdf_salt_identifier: "test@example.com".to_string(),
        primary_wallet_address: None,
        kdf_params: test_kdf_params(),
        encrypted_symmetric_key: test_encrypted_blob(),
        recovery_verification_hash: "recovery_hash".to_string(),
        created_at: Utc::now(),
        last_login_at: None,
        failed_login_attempts: 0,
        locked_until: None,
        role: Role::User,
    }
}

pub(super) fn test_device(user_id: UserId) -> Device {
    Device {
        id: DeviceId::new(),
        user_id,
        name: "Test Device".to_string(),
        device_type: DeviceType::Browser,
        encrypted_symmetric_key: test_encrypted_blob(),
        kdf_params: test_kdf_params(),
        created_at: Utc::now(),
        last_used_at: None,
        is_active: true,
    }
}

pub(super) fn test_session(user_id: UserId, device_id: DeviceId) -> Session {
    Session {
        id: SessionId::new(),
        user_id,
        device_id,
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::hours(24),
        last_activity_at: Utc::now(),
        ip_address: Some("127.0.0.1".to_string()),
        user_agent: Some("Test Agent".to_string()),
    }
}

pub(super) fn test_wallet(user_id: UserId) -> WalletCredential {
    WalletCredential {
        id: WalletCredentialId::new(),
        user_id,
        address: "0x1234567890123456789012345678901234567890".to_string(),
        name: "Test Wallet".to_string(),
        is_primary: true,
        created_at: Utc::now(),
        last_used_at: None,
        is_active: true,
    }
}

pub(super) fn test_wallet_challenge() -> WalletChallenge {
    WalletChallenge {
        challenge: "abcd1234".to_string(),
        address: "0x1234567890123456789012345678901234567890".to_string(),
        created_at: Utc::now(),
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn test_device_type_conversion() {
    assert_eq!(device_type_to_db(DeviceType::Browser), "browser");
    assert_eq!(device_type_to_db(DeviceType::Desktop), "desktop");
    assert_eq!(device_type_to_db(DeviceType::Mobile), "mobile");
    assert_eq!(device_type_to_db(DeviceType::ApiClient), "api_client");
    assert_eq!(device_type_to_db(DeviceType::Unknown), "unknown");

    assert_eq!(db_to_device_type("browser"), DeviceType::Browser);
    assert_eq!(db_to_device_type("desktop"), DeviceType::Desktop);
    assert_eq!(db_to_device_type("mobile"), DeviceType::Mobile);
    assert_eq!(db_to_device_type("api_client"), DeviceType::ApiClient);
    assert_eq!(db_to_device_type("unknown"), DeviceType::Unknown);
    assert_eq!(db_to_device_type("invalid"), DeviceType::Unknown);
}

#[test]
fn test_user_creation() {
    let user = test_user();
    assert!(user.email.is_some());
    assert_eq!(user.email.as_ref().unwrap(), "test@example.com");
    assert_eq!(user.failed_login_attempts, 0);
    assert!(!user.is_locked());
}

#[test]
fn test_device_creation() {
    let user = test_user();
    let device = test_device(user.id);
    assert_eq!(device.user_id, user.id);
    assert_eq!(device.device_type, DeviceType::Browser);
    assert!(device.is_active);
}

#[test]
fn test_session_creation() {
    let user = test_user();
    let device = test_device(user.id);
    let session = test_session(user.id, device.id);
    assert_eq!(session.user_id, user.id);
    assert_eq!(session.device_id, device.id);
    assert!(!session.is_expired());
}

#[test]
fn test_wallet_creation() {
    let user = test_user();
    let wallet = test_wallet(user.id);
    assert_eq!(wallet.user_id, user.id);
    assert!(wallet.is_primary);
    assert!(wallet.is_active);
}

#[test]
fn test_wallet_challenge_creation() {
    let challenge = test_wallet_challenge();
    assert_eq!(challenge.challenge, "abcd1234");
    assert!(!challenge.address.is_empty());
}
