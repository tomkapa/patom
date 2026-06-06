//! Lemon Squeezy billing integration (issue #131).
//!
//! Lemon Squeezy is the Merchant of Record: it owns card handling and global
//! tax, so Patom stores only ids + subscription status (PCI SAQ-A). This module
//! holds the domain newtypes, the one boundary error type, and the
//! subscription store; the webhook, checkout, client, and billing-backed
//! entitlement impl build on top.

pub mod error;
pub mod limits;
pub mod pg_store;
pub mod store;
pub mod types;

pub use error::LemonSqueezyError;
pub use pg_store::PgSubscriptionStore;
pub use store::{NewSubscription, SubscriptionRecord, SubscriptionStore};
pub use types::{
    LsCustomerId, LsEventId, LsOrderId, LsSubscriptionId, LsVariantId, Plan, SubscriptionId,
    SubscriptionStatus,
};
