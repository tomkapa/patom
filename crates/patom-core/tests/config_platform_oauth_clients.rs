//! Boundary test for `parse_platform_oauth_clients` — the env-var seam that
//! replaces the per-vendor `google_*` / `github_*` fields in `AuthSettings`.
//!
//! The function is a pure parser: it takes an `(env-var key, value)` iterator
//! and emits a map keyed by the lowercased middle of `PATOM_<MID>_CLIENT_ID`
//! / `PATOM_<MID>_CLIENT_SECRET` pairs. Catalog-id mapping (`-` ↔ `_`,
//! membership checks) is a separate concern handled by the resolver.

#![allow(clippy::expect_used)]

use patom::config::parse_platform_oauth_clients;

#[test]
fn reads_paired_env_vars() {
    let vars = vec![
        (
            "PATOM_GOOGLE_CLIENT_ID".to_string(),
            "google-id".to_string(),
        ),
        (
            "PATOM_GOOGLE_CLIENT_SECRET".to_string(),
            "google-secret".to_string(),
        ),
        ("PATOM_GITHUB_CLIENT_ID".to_string(), "gh-id".to_string()),
        (
            "PATOM_GITHUB_CLIENT_SECRET".to_string(),
            "gh-secret".to_string(),
        ),
    ];

    let out = parse_platform_oauth_clients(vars);

    assert_eq!(out.len(), 2);
    let g = out.get("google").expect("google entry present");
    assert_eq!(g.client_id.expose(), "google-id");
    assert_eq!(g.client_secret.expose(), "google-secret");
    let gh = out.get("github").expect("github entry present");
    assert_eq!(gh.client_id.expose(), "gh-id");
    assert_eq!(gh.client_secret.expose(), "gh-secret");
}

#[test]
fn unpaired_client_id_is_skipped() {
    // CLIENT_ID without a matching CLIENT_SECRET is a misconfiguration; the
    // parser drops the half-pair rather than producing a partial entry that
    // would explode at the token exchange.
    let vars = vec![("PATOM_GOOGLE_CLIENT_ID".to_string(), "id-only".to_string())];
    assert!(parse_platform_oauth_clients(vars).is_empty());
}

#[test]
fn unpaired_client_secret_is_skipped() {
    let vars = vec![(
        "PATOM_GOOGLE_CLIENT_SECRET".to_string(),
        "secret-only".to_string(),
    )];
    assert!(parse_platform_oauth_clients(vars).is_empty());
}

#[test]
fn non_patom_prefixed_vars_are_ignored() {
    // `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` are the existing
    // Login-with-Google envs — they must not bleed into the MCP map.
    let vars = vec![
        ("GOOGLE_CLIENT_ID".to_string(), "login-id".to_string()),
        (
            "GOOGLE_CLIENT_SECRET".to_string(),
            "login-secret".to_string(),
        ),
    ];
    assert!(parse_platform_oauth_clients(vars).is_empty());
}

#[test]
fn empty_value_is_rejected_at_boundary() {
    // SecretString rejects empty values; the pair must be dropped silently
    // rather than crash boot.
    let vars = vec![
        ("PATOM_GOOGLE_CLIENT_ID".to_string(), String::new()),
        (
            "PATOM_GOOGLE_CLIENT_SECRET".to_string(),
            "secret".to_string(),
        ),
    ];
    assert!(parse_platform_oauth_clients(vars).is_empty());
}

#[test]
fn underscore_in_middle_is_preserved_after_lowercase() {
    // Per the env-var convention, catalog ids with `-` are spelled with `_`
    // in env-var middles. The parser keeps the middle as-is (lowercased);
    // the resolver does the `-` ↔ `_` round-trip when looking up.
    let vars = vec![
        (
            "PATOM_MICROSOFT_365_CLIENT_ID".to_string(),
            "ms-id".to_string(),
        ),
        (
            "PATOM_MICROSOFT_365_CLIENT_SECRET".to_string(),
            "ms-secret".to_string(),
        ),
    ];
    let out = parse_platform_oauth_clients(vars);
    assert_eq!(out.len(), 1);
    let c = out.get("microsoft_365").expect("middle preserved");
    assert_eq!(c.client_id.expose(), "ms-id");
}

#[test]
fn empty_middle_is_rejected() {
    // `PATOM__CLIENT_ID` would yield an empty middle — reject.
    let vars = vec![
        ("PATOM__CLIENT_ID".to_string(), "id".to_string()),
        ("PATOM__CLIENT_SECRET".to_string(), "secret".to_string()),
    ];
    assert!(parse_platform_oauth_clients(vars).is_empty());
}
