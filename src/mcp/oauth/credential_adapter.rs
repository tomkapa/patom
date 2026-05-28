//! Adapter that lets `rmcp::transport::auth::AuthorizationManager` persist
//! credentials through patom's encrypted, multi-tenant
//! `mcp_server_credentials` table.
//!
//! Rmcp's [`CredentialStore`] trait is *single-keyed* (`load`/`save`/`clear`
//! take no arguments) — each adapter instance binds one `(server_id, org_id)`
//! tuple at construction. The session map in [`super::session`] builds one
//! per `AuthorizationManager`.
//!
//! Mapping is the identity transform on the inner value: patom's
//! [`OAuth2Payload`] wraps `rmcp::transport::auth::StoredCredentials`
//! verbatim. We seal the JSON envelope under the org KEK before INSERT so
//! the credentials at rest are no more readable than any other tenant data.

use std::fmt;

use async_trait::async_trait;
use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};

use crate::auth::OrgId;
use crate::mcp::credentials::{
    CredentialPayload, McpCredentialWrite, OAuth2Payload, SharedMcpCredentialStore,
};
use crate::mcp::types::McpServerId;

/// `(server_id, org_id)`-scoped credential store driving rmcp's
/// `AuthorizationManager`.
pub struct PatomCredentialStore {
    server_id: McpServerId,
    org_id: OrgId,
    inner: SharedMcpCredentialStore,
}

impl PatomCredentialStore {
    #[must_use]
    pub fn new(server_id: McpServerId, org_id: OrgId, inner: SharedMcpCredentialStore) -> Self {
        Self {
            server_id,
            org_id,
            inner,
        }
    }
}

impl fmt::Debug for PatomCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatomCredentialStore")
            .field("server_id", &self.server_id)
            .field("org_id", &self.org_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CredentialStore for PatomCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let record = self
            .inner
            .read(self.server_id, self.org_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("credential read: {e}")))?;
        let Some(record) = record else {
            return Ok(None);
        };
        match record.payload {
            CredentialPayload::Oauth2(p) => Ok(Some(p.stored)),
            CredentialPayload::StaticHeaders { .. } => {
                // The server is configured with a static-bearer header, not
                // OAuth — the AuthorizationManager should never have been
                // asked to load credentials for it. Surface as an internal
                // error so the connect path can clear it cleanly.
                Err(AuthError::InternalError(format!(
                    "server {sid}/{org} has static_headers credentials, not oauth2",
                    sid = self.server_id,
                    org = self.org_id
                )))
            }
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let payload = CredentialPayload::Oauth2(OAuth2Payload {
            stored: credentials,
        });
        self.inner
            .upsert(McpCredentialWrite {
                server_id: self.server_id,
                org_id: self.org_id,
                payload,
            })
            .await
            .map_err(|e| AuthError::InternalError(format!("credential upsert: {e}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.inner
            .delete(self.server_id, self.org_id)
            .await
            .map_err(|e| AuthError::InternalError(format!("credential delete: {e}")))
    }
}
