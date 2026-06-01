-- Reverse ADR-0011's OIDC identity columns.
--
-- Safe pre-launch / before any non-Google issuer row exists: a row with
-- `provider = 'oidc'` has no home once the CHECK is restored to
-- ('google'), so this rollback assumes only Google rows are present.

ALTER TABLE oauth_login_states DROP COLUMN nonce;

ALTER TABLE user_identities DROP CONSTRAINT user_identities_provider_check;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_provider_check CHECK (provider IN ('google'));

ALTER TABLE user_identities DROP CONSTRAINT user_identities_pkey;
ALTER TABLE user_identities
    ADD CONSTRAINT user_identities_pkey PRIMARY KEY (provider, subject);

ALTER TABLE user_identities
    DROP COLUMN oidc_subject,
    DROP COLUMN oidc_issuer;
