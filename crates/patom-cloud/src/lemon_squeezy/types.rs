//! Domain newtypes for the Lemon Squeezy integration (CLAUDE.md §1).
//!
//! Lemon Squeezy ids arrive as opaque strings in webhook / API JSON and land
//! in `TEXT` columns; each is a bounded-`String` newtype with a fallible smart
//! constructor (non-empty, length-capped) — the only way in. The internal
//! `SubscriptionId` is a UUID we mint. `Plan` / `SubscriptionStatus` are
//! `str_enum!`s whose labels are the single source of truth for the column
//! value and the JSON wire.

use patom::types::ParseError;

use super::limits::MAX_LS_ID_BYTES;

/// Emits a bounded-`String` newtype for an opaque Lemon Squeezy id: a fallible
/// `TryFrom<String>` / `TryFrom<&str>` (non-empty, ≤ [`MAX_LS_ID_BYTES`]) plus
/// `as_str`. SQL binding passes `as_str`; reads parse back through `TryFrom`,
/// so the bound holds on every crossing.
macro_rules! ls_string_id {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseError;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                if raw.is_empty() {
                    return Err(ParseError::Empty { field: $field });
                }
                if raw.len() > MAX_LS_ID_BYTES {
                    return Err(ParseError::TooLong {
                        field: $field,
                        max: MAX_LS_ID_BYTES,
                        got: raw.len(),
                    });
                }
                Ok(Self(raw))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseError;
            fn try_from(raw: &str) -> Result<Self, Self::Error> {
                Self::try_from(raw.to_owned())
            }
        }
    };
}

ls_string_id! {
    /// Lemon Squeezy customer id (`customer_id` on orders/subscriptions).
    LsCustomerId, "ls_customer_id"
}
ls_string_id! {
    /// Lemon Squeezy subscription id — the natural key for webhook upserts.
    LsSubscriptionId, "ls_subscription_id"
}
ls_string_id! {
    /// Lemon Squeezy order id (`order_created`).
    LsOrderId, "ls_order_id"
}
ls_string_id! {
    /// Lemon Squeezy variant id — maps to a [`Plan`] via config.
    LsVariantId, "ls_variant_id"
}
ls_string_id! {
    /// Lemon Squeezy store id the checkout is created against.
    LsStoreId, "ls_store_id"
}
ls_string_id! {
    /// Per-event idempotency key for an inbound webhook.
    ///
    /// Ensures a redelivery is applied exactly once. Lemon Squeezy sends no
    /// stable event id, so we derive it from the SHA-256 of the raw body
    /// (redeliveries are byte-identical); see `webhook::body_event_id`.
    LsEventId, "ls_event_id"
}

patom::uuid_newtype! {
    /// Internal id for a `cloud.subscriptions` row.
    pub SubscriptionId
}

patom::str_enum! {
    /// The paid plan an org is on. Labels are the stored `plan` value; the
    /// agent-cap mapping lives in the entitlement impl (#131).
    pub enum Plan {
        Starter    => "starter",
        Growth     => "growth",
        Scale      => "scale",
        Enterprise => "enterprise",
    }
}

patom::str_enum! {
    /// Lemon Squeezy subscription status, stored verbatim. The entitlement
    /// impl decides which statuses still grant the paid cap (active/on_trial,
    /// past_due within grace) and which downgrade.
    pub enum SubscriptionStatus {
        OnTrial   => "on_trial",
        Active    => "active",
        Paused    => "paused",
        PastDue   => "past_due",
        Unpaid    => "unpaid",
        Cancelled => "cancelled",
        Expired   => "expired",
    }
}
