-- Org default language + OAuth locale capture.
--
-- Adds a per-org `default_language` (en | vi) read by the agent worker on
-- every turn to render the new `<language>` directive in the system
-- prompt and by the OAuth callback to seed the personal org's value at
-- first sign-up. The seed value comes from Google's userinfo `locale`
-- field with the inbound `Accept-Language` header (captured here at
-- /auth/google/login time) as a fallback.
--
-- Pre-launch: NOT NULL with no DEFAULT (see `feedback_no_backcompat`).
-- Every insert site must pass a Language explicitly — the personal-org
-- creation path in `auth::callback` and any test fixture that mints an
-- organization. Existing dev rows must be wiped before applying.

ALTER TABLE organizations
    ADD COLUMN default_language TEXT NOT NULL
        CHECK (default_language IN ('en', 'vi'));

-- Inbound Accept-Language primary tag, captured at /auth/google/login and
-- replayed in the callback as the locale fallback when Google's userinfo
-- doesn't carry one. Nullable: not every login (e.g. curl probes) sends
-- the header. Length-capped to 32 bytes — primary-tag selection in the
-- handler strips the rest before insert, and the cap matches the app-side
-- assertion in `auth::limits`.
ALTER TABLE oauth_login_states
    ADD COLUMN detected_locale TEXT
        CHECK (detected_locale IS NULL OR octet_length(detected_locale) <= 32);
