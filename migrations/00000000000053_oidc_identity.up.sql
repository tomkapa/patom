-- Pluggable OIDC identity (ADR-0011).
--
-- Generalises identity from Google-only `(provider, subject)` to the
-- standards-compliant OIDC key `(issuer, subject)`. Google becomes one
-- preset whose issuer is `https://accounts.google.com`; any other
-- compliant IdP (Okta/Entra/Keycloak/Ping) is config, not schema.
--
-- Also adds the id_token `nonce` to the short-lived login-state row so
-- the callback can verify the nonce the authorize step minted.
--
-- Pre-launch: the `oauth_login_states.nonce` column is added NOT NULL
-- with no backfill. Any in-flight login-state rows (10-min TTL) must be
-- cleared before applying — they are ephemeral by construction. See
-- `feedback_no_backcompat`.

-- ───────────────────────────────────────────────────────────────────────────
-- user_identities: re-key identity from the Google-only (provider,
-- subject) to the standards-compliant OIDC (issuer, subject).
--
-- The legacy PK must be demoted, not merely supplemented: generic logins
-- all carry provider = 'oidc', so a per-(provider, subject) PK would
-- collide whenever two different issuers happen to share a `sub` value —
-- exactly the cross-IdP collision ADR-0011 sets out to prevent. The
-- (oidc_issuer, oidc_subject) pair becomes the new PK. The legacy
-- `subject` column is retained (NOT NULL, mirrors `oidc_subject`) for
-- the backfilled Google rows; `provider` is retained and its CHECK
-- relaxed to admit 'oidc'.
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE user_identities
    ADD COLUMN oidc_issuer  TEXT CHECK (oidc_issuer  IS NULL OR octet_length(oidc_issuer)  BETWEEN 1 AND 255),
    ADD COLUMN oidc_subject TEXT CHECK (oidc_subject IS NULL OR octet_length(oidc_subject) BETWEEN 1 AND 255);

-- Backfill existing Google rows: their issuer is Google's, their OIDC
-- subject is the `sub` claim already stored in `subject`.
UPDATE user_identities
   SET oidc_issuer  = 'https://accounts.google.com',
       oidc_subject = subject
 WHERE provider = 'google';

-- Every row now carries the pair; enforce presence and promote it to PK.
ALTER TABLE user_identities
    ALTER COLUMN oidc_issuer  SET NOT NULL,
    ALTER COLUMN oidc_subject SET NOT NULL;
ALTER TABLE user_identities DROP CONSTRAINT user_identities_pkey;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_pkey PRIMARY KEY (oidc_issuer, oidc_subject);

-- Relax the provider allow-list to admit generic OIDC logins.
ALTER TABLE user_identities DROP CONSTRAINT user_identities_provider_check;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_provider_check CHECK (provider IN ('google', 'oidc'));

-- ───────────────────────────────────────────────────────────────────────────
-- oauth_login_states: carry the id_token nonce through the round-trip.
-- ───────────────────────────────────────────────────────────────────────────

-- The `nonce` column is NOT NULL with no default, so adding it would abort
-- if any in-flight login-state row exists. Those rows are ephemeral (10-min
-- TTL, mid-consent round-trips) and carry no durable value, so clear them
-- first — the alternative (nullable → backfill → SET NOT NULL) would only
-- invent throwaway nonces for logins that are about to be abandoned.
DELETE FROM oauth_login_states;

ALTER TABLE oauth_login_states
    ADD COLUMN nonce TEXT NOT NULL CHECK (octet_length(nonce) BETWEEN 1 AND 128);
