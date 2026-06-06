use std::io;
use std::net::SocketAddr;

use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::auth::AuthError;
use crate::config::SettingsError;
use crate::provider::ProviderError;
use crate::types::ParseError;

/// Cross-cutting application errors raised during startup / composition. Library APIs
/// have their own error types per CLAUDE.md §12 — this is the binary-edge type that
/// `main` propagates through `anyhow`.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration: {0}")]
    Config(#[from] SettingsError),

    #[error("invalid value: {0}")]
    Parse(#[from] ParseError),

    #[error("provider init: {0}")]
    Provider(#[from] ProviderError),

    #[error("http client init: {0}")]
    Http(#[from] reqwest::Error),

    #[error("http bind {http_addr}: {source}")]
    Bind {
        http_addr: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("postgres connect: {source}")]
    DbConnect {
        #[source]
        source: sqlx::Error,
    },

    #[error("postgres migrate: {source}")]
    Migrate {
        #[source]
        source: sqlx::migrate::MigrateError,
    },

    #[error("agent store: {0}")]
    AgentStore(#[from] AgentStoreError),

    #[error("auth: {0}")]
    Auth(#[from] AuthError),

    #[error("mailer init: {0}")]
    Mailer(#[from] crate::orgs::MailError),

    #[error("misconfigured: {0}")]
    Misconfigured(String),
}
