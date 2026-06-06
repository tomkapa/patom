//! Declarative macros that emit the boilerplate halves of CLAUDE.md §1's
//! "newtype every invariant" rule for two recurring shapes:
//!
//! * [`uuid_newtype!`] — opaque UUID-backed identifiers (Postgres `UUID`
//!   column + JSON wire). Emits `Type` / `Encode` / `Decode` / `PgHasArrayType`
//!   for sqlx and `Serialize` / `Deserialize` for serde, plus the standard
//!   ergonomics (`Debug` / `Display` / `Default` / `From<Uuid>` / `as_uuid`).
//!   The hand-written half — fallible smart constructors, parsing — stays at
//!   the call site.
//!
//! * [`str_enum!`] — flat (no-payload) enums where each variant has a stable
//!   `&'static str` label that is the single source of truth for the column
//!   `CHECK` constraint, the JSON wire format, and any tracing attribute.
//!   Adding a variant is a one-line edit; the contract enforces itself.
//!
//! Both macros are intentionally narrow: payloaded enums (e.g. JSON-envelope
//! variants like `FailureReason`) and length-bounded `String` newtypes stay
//! hand-written — the macro would obscure the variation that matters.

/// Emits a `Uuid`-backed newtype with the full sqlx + serde + ergonomics suite
/// that every opaque identifier in this crate needs.
///
/// ```ignore
/// crate::uuid_newtype! {
///     /// Opaque session identifier.
///     pub SessionId
/// }
///
/// // Custom parsing stays explicit at the call site:
/// impl TryFrom<&str> for SessionId {
///     type Error = ParseError;
///     fn try_from(raw: &str) -> Result<Self, Self::Error> { ... }
/// }
/// ```
#[macro_export]
macro_rules! uuid_newtype {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(::uuid::Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self { Self(::uuid::Uuid::new_v4()) }

            #[must_use]
            pub fn as_uuid(self) -> ::uuid::Uuid { self.0 }
        }

        impl ::std::default::Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl ::std::convert::From<::uuid::Uuid> for $name {
            fn from(raw: ::uuid::Uuid) -> Self { Self(raw) }
        }

        impl ::sqlx::Type<::sqlx::Postgres> for $name {
            fn type_info() -> ::sqlx::postgres::PgTypeInfo {
                <::uuid::Uuid as ::sqlx::Type<::sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &::sqlx::postgres::PgTypeInfo) -> bool {
                <::uuid::Uuid as ::sqlx::Type<::sqlx::Postgres>>::compatible(ty)
            }
        }

        impl ::sqlx::postgres::PgHasArrayType for $name {
            fn array_type_info() -> ::sqlx::postgres::PgTypeInfo {
                <::uuid::Uuid as ::sqlx::postgres::PgHasArrayType>::array_type_info()
            }
        }

        impl<'r> ::sqlx::Decode<'r, ::sqlx::Postgres> for $name {
            fn decode(
                value: ::sqlx::postgres::PgValueRef<'r>,
            ) -> ::std::result::Result<Self, ::sqlx::error::BoxDynError> {
                <::uuid::Uuid as ::sqlx::Decode<'r, ::sqlx::Postgres>>::decode(value).map(Self)
            }
        }

        impl<'q> ::sqlx::Encode<'q, ::sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut ::sqlx::postgres::PgArgumentBuffer,
            ) -> ::std::result::Result<::sqlx::encode::IsNull, ::sqlx::error::BoxDynError> {
                <::uuid::Uuid as ::sqlx::Encode<'q, ::sqlx::Postgres>>::encode_by_ref(&self.0, buf)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                ::uuid::Uuid::deserialize(deserializer).map(Self)
            }
        }
    };
}

/// Declares a flat enum where each variant has a stable `&'static str` label.
///
/// The label list is the single source of truth: `as_str`, `parse`, `ALL`,
/// `sqlx::{Type, Encode, Decode}`, and `serde::{Serialize, Deserialize}` are
/// all derived from it, so the column `CHECK` constraint, JSON wire format,
/// and tracing attributes cannot drift.
///
/// ```ignore
/// crate::str_enum! {
///     /// Lifecycle state of a prompt request row.
///     pub enum RequestStatus {
///         Pending    => "pending",
///         Processing => "processing",
///         Done       => "done",
///         Failed     => "failed",
///     }
/// }
/// ```
#[macro_export]
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident => $label:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every variant — drives [`Self::parse`], the sqlx `Decode` impl,
            /// and serde `Deserialize`, so the inverse of [`Self::as_str`]
            /// cannot drift.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Stable wire/storage label. The corresponding column `CHECK`
            /// constraint and JSON wire format are keyed off these strings.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)+
                }
            }

            /// Inverse of [`Self::as_str`], driven from the same source.
            #[must_use]
            pub fn parse(raw: &str) -> ::std::option::Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_str() == raw)
            }
        }

        impl ::sqlx::Type<::sqlx::Postgres> for $name {
            fn type_info() -> ::sqlx::postgres::PgTypeInfo {
                <&str as ::sqlx::Type<::sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &::sqlx::postgres::PgTypeInfo) -> bool {
                <&str as ::sqlx::Type<::sqlx::Postgres>>::compatible(ty)
            }
        }

        impl<'r> ::sqlx::Decode<'r, ::sqlx::Postgres> for $name {
            fn decode(
                value: ::sqlx::postgres::PgValueRef<'r>,
            ) -> ::std::result::Result<Self, ::sqlx::error::BoxDynError> {
                let raw = <&str as ::sqlx::Decode<'r, ::sqlx::Postgres>>::decode(value)?;
                // §6: schema CHECK constraint forbids any value outside the
                // label set; observing one means schema and code disagree.
                Self::parse(raw).ok_or_else(|| {
                    format!("invariant: unknown {} {raw:?}", stringify!($name)).into()
                })
            }
        }

        impl<'q> ::sqlx::Encode<'q, ::sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut ::sqlx::postgres::PgArgumentBuffer,
            ) -> ::std::result::Result<::sqlx::encode::IsNull, ::sqlx::error::BoxDynError> {
                <&str as ::sqlx::Encode<'q, ::sqlx::Postgres>>::encode(self.as_str(), buf)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                // `String` rather than `&str` so deserializers that
                // cannot borrow (e.g. `serde_json::Value`) still work.
                let raw = <::std::string::String as ::serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(raw.as_str()).ok_or_else(|| {
                    <D::Error as ::serde::de::Error>::custom(
                        format!("unknown {} {raw:?}", stringify!($name))
                    )
                })
            }
        }
    };
}
