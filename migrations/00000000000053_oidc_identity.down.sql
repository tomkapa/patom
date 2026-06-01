-- Reverse ADR-0011's OIDC identity columns.

ALTER TABLE oauth_login_states DROP COLUMN nonce;

-- The forward writer stamps every fresh identity with provider='oidc'
-- (Google-preset logins included), so the legacy Google-only shape must be
-- restored before the CHECK/PK are re-tightened — otherwise the rollback
-- aborts the moment the new code has been used once. Google-issuer rows
-- revert to provider='google' (the legacy `subject` column already mirrors
-- the sub). Any non-Google issuer row has no representation in the pre-OIDC
-- schema, so rolling the feature back necessarily drops it.
UPDATE user_identities
   SET provider = 'google'
 WHERE provider = 'oidc'
   AND oidc_issuer = 'https://accounts.google.com';
DELETE FROM user_identities WHERE provider = 'oidc';

ALTER TABLE user_identities DROP CONSTRAINT user_identities_provider_check;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_provider_check CHECK (provider IN ('google'));

ALTER TABLE user_identities DROP CONSTRAINT user_identities_pkey;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_pkey PRIMARY KEY (provider, subject);

ALTER TABLE user_identities
    DROP COLUMN oidc_subject,
    DROP COLUMN oidc_issuer;
