//! Long-connection endpoint handshake.
//!
//! `POST {api_base}/callback/ws/endpoint` with `{AppID, AppSecret}` returns the
//! `wss://…` URL to dial (carrying `device_id` + `service_id` query params) plus
//! a server-issued `ClientConfig` (ping interval, reconnect policy). Transcribed
//! from `larksuite/oapi-sdk-go` (`ws/client.go::getConnURL`).
//!
//! The HTTP call is one timed round-trip; the response parsing is factored into
//! pure functions so the handshake can be unit-tested without a network.

use serde::{Deserialize, Serialize};

use super::error::LarkError;
use super::limits::LARK_HANDSHAKE_TIMEOUT;
use super::pbbp2::{ENDPOINT_EXCEED_CONN_LIMIT, ENDPOINT_OK};
use super::types::{LarkAppId, LarkAppSecret};

/// The endpoint path appended to the API base for the handshake POST.
const ENDPOINT_PATH: &str = "/callback/ws/endpoint";
/// The query parameter on the returned URL that carries the connection's
/// numeric service id (used as `Frame.service` on the ping).
const QUERY_SERVICE_ID: &str = "service_id";

/// Raw `ClientConfig` as the server sends it — all values in seconds / counts.
/// The WS manager applies defaults and the [`super::limits::LARK_RECONNECT_MAX`]
/// cap when turning these into `Duration`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawClientConfig {
    /// Reconnect attempts; the server default is `-1` (unbounded) — we cap it.
    pub reconnect_count: i32,
    /// Backoff between reconnect attempts, in seconds.
    pub reconnect_interval: i32,
    /// First-reconnect jitter ceiling, in seconds.
    pub reconnect_nonce: i32,
    /// Ping cadence, in seconds.
    pub ping_interval: i32,
}

/// A negotiated endpoint: the URL to dial, the parsed `service_id`, and the
/// server's reconnect/ping policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub service_id: i32,
    pub config: RawClientConfig,
}

/// Perform the handshake against `api_base`, returning the endpoint to dial.
pub async fn negotiate(
    http: &reqwest::Client,
    api_base: &str,
    app_id: &LarkAppId,
    secret: &LarkAppSecret,
) -> Result<Endpoint, LarkError> {
    let url = format!("{api_base}{ENDPOINT_PATH}");
    let body = BootstrapRequest {
        app_id: app_id.as_str(),
        app_secret: secret.expose(),
    };
    let send = http.post(&url).header("locale", "zh").json(&body).send();
    let resp = tokio::time::timeout(LARK_HANDSHAKE_TIMEOUT, send)
        .await
        .map_err(|_| LarkError::Handshake("endpoint handshake timed out".to_owned()))??;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        // Non-200: the body is a `{code,msg}` error envelope.
        let msg = serde_json::from_slice::<BootstrapErrorResp>(&bytes)
            .ok()
            .filter(|e| !e.msg.is_empty())
            .map_or_else(|| format!("http status {status}"), |e| e.msg);
        return Err(LarkError::Handshake(msg));
    }
    parse_endpoint(&bytes, app_id)
}

/// Pure response parse: validate the `code`, extract the URL + `service_id` +
/// `ClientConfig`. `app_id` is only used to label the [`LarkError::ConnLimit`].
fn parse_endpoint(body: &[u8], app_id: &LarkAppId) -> Result<Endpoint, LarkError> {
    let resp: EndpointResp = serde_json::from_slice(body)?;
    if resp.code == ENDPOINT_EXCEED_CONN_LIMIT {
        return Err(LarkError::ConnLimit(app_id.clone()));
    }
    if resp.code != ENDPOINT_OK {
        let msg = if resp.msg.is_empty() {
            format!("endpoint code {}", resp.code)
        } else {
            resp.msg
        };
        return Err(LarkError::Handshake(msg));
    }
    let data = resp
        .data
        .ok_or_else(|| LarkError::Handshake("endpoint ok but data is null".to_owned()))?;
    let url = data
        .url
        .filter(|u| !u.is_empty())
        .ok_or_else(|| LarkError::Handshake("endpoint ok but URL is empty".to_owned()))?;
    let service_id = service_id_from_url(&url)?;
    let config = data.client_config.unwrap_or_default();
    Ok(Endpoint {
        url,
        service_id,
        config: RawClientConfig {
            reconnect_count: config.reconnect_count,
            reconnect_interval: config.reconnect_interval,
            reconnect_nonce: config.reconnect_nonce,
            ping_interval: config.ping_interval,
        },
    })
}

/// Extract the numeric `service_id` query parameter from the dial URL.
fn service_id_from_url(raw: &str) -> Result<i32, LarkError> {
    let parsed = url::Url::parse(raw)
        .map_err(|e| LarkError::Handshake(format!("invalid endpoint URL: {e}")))?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == QUERY_SERVICE_ID)
        .and_then(|(_, v)| v.parse::<i32>().ok())
        .ok_or_else(|| LarkError::Handshake("endpoint URL missing service_id".to_owned()))
}

#[derive(Serialize)]
struct BootstrapRequest<'a> {
    #[serde(rename = "AppID")]
    app_id: &'a str,
    #[serde(rename = "AppSecret")]
    app_secret: &'a str,
}

#[derive(Deserialize)]
struct BootstrapErrorResp {
    #[serde(default)]
    #[allow(dead_code)] // surfaced via `msg`; kept for wire completeness.
    code: i32,
    #[serde(default)]
    msg: String,
}

#[derive(Deserialize)]
struct EndpointResp {
    code: i32,
    #[serde(default)]
    msg: String,
    data: Option<EndpointData>,
}

#[derive(Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: Option<String>,
    #[serde(rename = "ClientConfig")]
    client_config: Option<WireClientConfig>,
}

#[derive(Deserialize, Default)]
struct WireClientConfig {
    #[serde(default, rename = "ReconnectCount")]
    reconnect_count: i32,
    #[serde(default, rename = "ReconnectInterval")]
    reconnect_interval: i32,
    #[serde(default, rename = "ReconnectNonce")]
    reconnect_nonce: i32,
    #[serde(default, rename = "PingInterval")]
    ping_interval: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> LarkAppId {
        LarkAppId::try_from("cli_test").expect("valid")
    }

    #[test]
    fn parses_ok_endpoint_with_config() {
        let body = br#"{
            "code": 0,
            "msg": "success",
            "data": {
                "URL": "wss://gw.larksuite.com/ws?device_id=dev1&service_id=7",
                "ClientConfig": {
                    "ReconnectCount": -1,
                    "ReconnectInterval": 120,
                    "ReconnectNonce": 30,
                    "PingInterval": 120
                }
            }
        }"#;
        let ep = parse_endpoint(body, &app()).expect("ok endpoint");
        assert_eq!(ep.service_id, 7);
        assert_eq!(ep.config.reconnect_count, -1);
        assert_eq!(ep.config.ping_interval, 120);
        assert_eq!(ep.config.reconnect_nonce, 30);
        assert!(ep.url.starts_with("wss://"));
    }

    #[test]
    fn missing_client_config_defaults_to_zeros() {
        let body = br#"{"code":0,"data":{"URL":"wss://x/ws?service_id=3"}}"#;
        let ep = parse_endpoint(body, &app()).expect("ok");
        assert_eq!(ep.service_id, 3);
        assert_eq!(ep.config, RawClientConfig::default_zero());
    }

    #[test]
    fn non_ok_code_is_handshake_error() {
        let body = br#"{"code":1,"msg":"system busy"}"#;
        let err = parse_endpoint(body, &app()).expect_err("busy");
        assert!(matches!(err, LarkError::Handshake(m) if m == "system busy"));
    }

    #[test]
    fn exceed_conn_limit_maps_to_conn_limit() {
        let body = br#"{"code":1000040350,"msg":"too many"}"#;
        let err = parse_endpoint(body, &app()).expect_err("limit");
        assert!(matches!(err, LarkError::ConnLimit(_)));
    }

    #[test]
    fn ok_with_null_data_is_error() {
        let body = br#"{"code":0,"data":null}"#;
        assert!(parse_endpoint(body, &app()).is_err());
    }

    #[test]
    fn url_without_service_id_is_error() {
        let err = service_id_from_url("wss://gw/ws?device_id=d").expect_err("no service id");
        assert!(matches!(err, LarkError::Handshake(_)));
    }

    impl RawClientConfig {
        fn default_zero() -> Self {
            Self {
                reconnect_count: 0,
                reconnect_interval: 0,
                reconnect_nonce: 0,
                ping_interval: 0,
            }
        }
    }
}
