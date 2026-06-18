-- Reverse of 00000000000092_lark_card_credentials.up.sql.
ALTER TABLE lark_apps
    DROP COLUMN IF EXISTS card_encrypt_key_ciphertext,
    DROP COLUMN IF EXISTS card_encrypt_key_nonce,
    DROP COLUMN IF EXISTS card_verification_token_ciphertext,
    DROP COLUMN IF EXISTS card_verification_token_nonce,
    DROP COLUMN IF EXISTS card_key_version;
