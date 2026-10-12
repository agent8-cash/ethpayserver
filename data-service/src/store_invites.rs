//! Store invites: membership by consent of the address owner.
//!
//! An invite is keyed on an email address, not a user id. Creating one reads
//! no account, so the requester learns nothing about whether the address is
//! registered; the membership appears only when whoever holds the mailed
//! token redeems it.

use async_trait::async_trait;
use auth::{StoreId, StoreRoleId, UserId};
use chrono::{DateTime, Utc};
use types::RepositoryResult;
use uuid::Uuid;

/// What redeeming a token did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteAcceptance {
    /// The caller is now a member of `store_id` with `role_id`.
    Accepted {
        store_id: StoreId,
        role_id: StoreRoleId,
    },
    /// The caller already belongs to the store. The invite is left unspent and
    /// the existing role is untouched - an invite must never demote anyone.
    AlreadyMember,
    /// Missing, expired or already used; not distinguished, so a guesser
    /// learns nothing about how close a token was.
    Invalid,
}

#[async_trait]
pub trait StoreInviteWriter: Send + Sync {
    /// Create a pending invite for `email`, replacing any pending one for the
    /// same store and address. Returns the token to mail to that address.
    async fn create_store_invite(
        &self,
        store_id: StoreId,
        email: &str,
        role_id: StoreRoleId,
        invited_by: UserId,
        expires_at: DateTime<Utc>,
    ) -> RepositoryResult<Uuid>;

    /// Redeem a token for `user_id`, atomically: the invite is spent and the
    /// membership created together, or neither happens.
    async fn accept_store_invite(
        &self,
        token: Uuid,
        user_id: UserId,
    ) -> RepositoryResult<InviteAcceptance>;
}
