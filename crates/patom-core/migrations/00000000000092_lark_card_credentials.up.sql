-- Lark card-action callback credentials (issue #214).
--
-- The `card.action.trigger` callback for an interactive approval card arrives
-- over a NEW HTTPS route (the pbbp2 long-connection cannot carry callback
-- subscriptions), so — unlike the long-connection event stream, which is
-- authenticated by the bot's tenant_access_token — the inbound callback must be
-- verified with the app's Encrypt Key (request signature) and Verification
-- Token (body token). Both are per-app secrets the admin copies from the Lark
-- developer console, so they are sealed by OrgEncryptor exactly like
-- `app_secret` (ciphertext + 12-byte nonce + key_version).
--
-- NULLABLE: a long-connection-only app (no card callbacks) never sets them; the
-- card-action route 404s for an app whose credentials are absent (fail-closed).
-- `card_key_version` is shared by both blobs (sealed together at the same org
-- KEK version).

ALTER TABLE lark_apps
    ADD COLUMN card_encrypt_key_ciphertext         BYTEA NULL,
    ADD COLUMN card_encrypt_key_nonce              BYTEA NULL
        CHECK (card_encrypt_key_nonce IS NULL
               OR octet_length(card_encrypt_key_nonce) = 12),
    ADD COLUMN card_verification_token_ciphertext  BYTEA NULL,
    ADD COLUMN card_verification_token_nonce       BYTEA NULL
        CHECK (card_verification_token_nonce IS NULL
               OR octet_length(card_verification_token_nonce) = 12),
    ADD COLUMN card_key_version                     SMALLINT NULL,
    -- All-or-nothing: a partial state (e.g. ciphertext without nonce, or one
    -- secret but not the other) would persist undecryptable credentials and
    -- break the card-action verify path. The store enforces this in code; the
    -- DB CHECK is the structural backstop (§5/§6). Dropping any of these columns
    -- (the down migration) drops this constraint with it.
    ADD CONSTRAINT lark_apps_card_creds_all_or_nothing CHECK (
        (card_encrypt_key_ciphertext IS NULL
         AND card_encrypt_key_nonce IS NULL
         AND card_verification_token_ciphertext IS NULL
         AND card_verification_token_nonce IS NULL
         AND card_key_version IS NULL)
        OR
        (card_encrypt_key_ciphertext IS NOT NULL
         AND card_encrypt_key_nonce IS NOT NULL
         AND card_verification_token_ciphertext IS NOT NULL
         AND card_verification_token_nonce IS NOT NULL
         AND card_key_version IS NOT NULL)
    );
