//! Auth helpers for integration tests.
//!
//! Mints a deterministic `JwtSigner` + a stub `OidcAuth` for the test
//! `AppState`. Seeds users + orgs + memberships via raw SQL inside a
//! privileged tx (the identity tables are not RLS-protected so this is
//! straightforward).

use std::sync::Arc;

use async_trait::async_trait;
use patom::auth::{
    AuthStart, JwtSigner, OidcAuth, OidcNonce, OidcProfile, OrgId, PgUserStore, PkceVerifier,
    Principal, Role, SharedOidcAuth, SharedUserStore, UserId,
};
use patom::clock::{SharedClock, SystemClock};
use patom::types::SecretString;
use sqlx::PgPool;

const TEST_JWT_SECRET: &str = "test-secret-bytes-must-be-at-least-32-bytes-long-yes";

/// Build a `JwtSigner` with a deterministic secret. Tests that mint
/// cookies share this signer with the `AppState` so verification
/// succeeds.
#[must_use]
pub fn test_jwt(clock: SharedClock) -> JwtSigner {
    let secret = SecretString::try_from(TEST_JWT_SECRET.to_owned()).expect("non-empty");
    JwtSigner::new(&secret, clock).expect("test jwt signer")
}

/// A stub OIDC provider for the test `AppState`. Discovery needs network
/// and a live IdP, so the integration suite never drives a real login
/// round-trip; this fake fills the `oauth` slot and errors loudly if a
/// test ever calls it, surfacing the gap rather than hanging on a
/// socket.
#[derive(Debug)]
struct StubOidcAuth;

#[async_trait]
impl OidcAuth for StubOidcAuth {
    fn start(&self) -> Result<AuthStart, patom::auth::AuthError> {
        Err(patom::auth::AuthError::Internal(
            "stub oidc provider: start() not supported in tests",
        ))
    }

    async fn exchange(
        &self,
        _code: &str,
        _verifier: &PkceVerifier,
        _nonce: &OidcNonce,
    ) -> Result<OidcProfile, patom::auth::AuthError> {
        Err(patom::auth::AuthError::Internal(
            "stub oidc provider: exchange() not supported in tests",
        ))
    }
}

/// Build the stub [`SharedOidcAuth`] for the test `AppState`.
#[must_use]
pub fn test_oauth() -> SharedOidcAuth {
    Arc::new(StubOidcAuth)
}

/// Construct the user store backed by the test pool.
#[must_use]
pub fn user_store(pool: PgPool) -> SharedUserStore {
    Arc::new(PgUserStore::new(pool))
}

/// One seeded test principal — user + org + Owner membership.
#[derive(Debug, Clone)]
pub struct SeededPrincipal {
    pub user_id: UserId,
    pub org_id: OrgId,
    pub cookie_value: String,
}

/// Fixed CSRF token used by every seeded test principal. Real users
/// receive a freshly minted token from the OAuth callback / `/me`;
/// tests echo this constant in both the cookie and the header to
/// satisfy the CSRF middleware on non-GET requests.
pub const TEST_CSRF_TOKEN: &str = "test-csrf-token-fixed-value";

impl SeededPrincipal {
    /// Build the `Cookie:` header value to attach to a test request.
    /// Includes both the session JWT cookie and the CSRF cookie so
    /// non-GET requests through the CSRF middleware succeed.
    #[must_use]
    pub fn cookie_header(&self) -> String {
        format!(
            "{}={}; {}={}",
            patom::auth::limits::COOKIE_NAME,
            self.cookie_value,
            patom::auth::limits::CSRF_COOKIE_NAME,
            TEST_CSRF_TOKEN,
        )
    }

    /// The CSRF token value to echo in the `X-CSRF-Token` header on
    /// non-GET requests. Pairs with [`Self::cookie_header`] — the
    /// middleware enforces these two match.
    #[must_use]
    pub fn csrf_header(&self) -> &'static str {
        TEST_CSRF_TOKEN
    }

    /// Build a `Principal` mirroring the JWT claims — useful when a
    /// test wants to call a `begin_as`-style helper directly without
    /// going through the HTTP layer.
    #[must_use]
    pub fn as_principal(&self) -> Principal {
        Principal {
            user_id: self.user_id,
            active_org_id: self.org_id,
            role: Role::Owner,
        }
    }
}

/// Add an existing `user_id` to a freshly-minted org as a plain `member`
/// and return that org's id. For tests exercising multi-org behaviour —
/// a user who belongs to more than one org — without minting a whole new
/// principal.
pub async fn join_second_org(pool: &PgPool, user_id: UserId) -> OrgId {
    let org_id = OrgId::new();
    let now = chrono::Utc::now();
    let slug = format!("second-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query("INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) VALUES ($1, $2, $3, 'en', $4, $4)")
        .bind(org_id)
        .bind("Second Org")
        .bind(&slug)
        .bind(now)
        .execute(pool)
        .await
        .expect("seed second org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("join second org");
    org_id
}

/// Insert a `users` + `organizations` + `org_members` triple into the
/// test schema and mint a JWT cookie for them. The org slug is unique
/// per call via uuid.
pub async fn seed_principal(pool: &PgPool, jwt: &JwtSigner) -> SeededPrincipal {
    let user_id = UserId::new();
    let org_id = OrgId::new();
    let slug = format!(
        "test-org-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let email = format!(
        "user-{}@example.test",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(user_id)
        .bind(&email)
        .bind("Test User")
        .bind(now)
        .execute(pool)
        .await
        .expect("seed user");
    sqlx::query("INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) VALUES ($1, $2, $3, 'en', $4, $4)")
        .bind(org_id)
        .bind("Test Org")
        .bind(&slug)
        .bind(now)
        .execute(pool)
        .await
        .expect("seed org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed org member");

    let cookie_value = jwt.mint(user_id, org_id).expect("mint test jwt");
    SeededPrincipal {
        user_id,
        org_id,
        cookie_value,
    }
}

/// Re-export the default clock used by test setups so callers don't
/// have to import it separately when wiring [`test_jwt`].
#[must_use]
pub fn shared_clock() -> SharedClock {
    SystemClock::shared()
}

/// Mint a [`SeededPrincipal`] for a `(user_id, org_id)` pair already seeded
/// by [`super::pg::seed_tenant`]. Use this when a test needs an HTTP cookie
/// whose `active_org_id` lines up with the seeded `agent_id` — the only org
/// that has a seeded default agent in a freshly-migrated test database.
#[must_use]
pub fn principal_for_default_org(
    user_id: UserId,
    org_id: OrgId,
    jwt: &JwtSigner,
) -> SeededPrincipal {
    let cookie_value = jwt.mint(user_id, org_id).expect("mint test jwt");
    SeededPrincipal {
        user_id,
        org_id,
        cookie_value,
    }
}
