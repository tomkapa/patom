use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::instrument;

use crate::types::{ParseError, SecretString, ToolName};

use super::super::limits::{
    SEARCH_DEFAULT_COUNT, SEARCH_MAX_COUNT, SEARCH_TIMEOUT, truncate_to_char_boundary,
};
use super::super::traits::{Tool, ToolCallContext, ToolError};

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const SEARCH_QUERY_MAX_BYTES: usize = 400;
/// Hard cap on the upstream error body included in a `ToolError::Upstream`.
///
/// Brave error responses are short JSON payloads; capping at 4 KB stops a runaway
/// upstream from filling tracing fields and tool-result context.
const UPSTREAM_BODY_MAX_BYTES: usize = 4 * 1024;

// §5: the default count must always be a legal `SearchCount`. Pinned at compile time so
// a future bump cannot silently invert the relationship.
const _: () = assert!(SEARCH_DEFAULT_COUNT >= 1);
const _: () = assert!(SEARCH_DEFAULT_COUNT <= SEARCH_MAX_COUNT);

/// Validated search query.
///
/// Trimmed of leading/trailing whitespace and capped at [`SEARCH_QUERY_MAX_BYTES`]. The
/// trim happens at parse time so the rest of the tool sees a single canonical form.
#[derive(Debug, Clone)]
struct SearchQuery(String);

impl SearchQuery {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SearchQuery {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty {
                field: "search_query",
            });
        }
        if trimmed.len() > SEARCH_QUERY_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "search_query",
                max: SEARCH_QUERY_MAX_BYTES,
                got: trimmed.len(),
            });
        }
        Ok(Self(trimmed.to_string()))
    }
}

impl<'de> Deserialize<'de> for SearchQuery {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Validated result count: `1..=SEARCH_MAX_COUNT`. Default is [`SEARCH_DEFAULT_COUNT`].
#[derive(Debug, Clone, Copy)]
struct SearchCount(u8);

impl SearchCount {
    const fn get(self) -> u8 {
        self.0
    }
}

impl Default for SearchCount {
    fn default() -> Self {
        // §6: the relationship `SEARCH_DEFAULT_COUNT in 1..=SEARCH_MAX_COUNT` is pinned at
        // compile time below, so this conversion cannot fail.
        Self(SEARCH_DEFAULT_COUNT)
    }
}

impl TryFrom<u8> for SearchCount {
    type Error = ParseError;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        if n == 0 {
            return Err(ParseError::OutOfRange {
                field: "search_count",
                detail: "must be >= 1",
            });
        }
        if n > SEARCH_MAX_COUNT {
            return Err(ParseError::OutOfRange {
                field: "search_count",
                detail: "exceeds ceiling",
            });
        }
        Ok(Self(n))
    }
}

impl<'de> Deserialize<'de> for SearchCount {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u8::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Brave Search front-end. Behind the trait so the agent never knows which search
/// vendor is in use; swapping vendors is one new file plus one composition-root edit.
#[derive(Debug)]
pub struct WebSearchTool {
    name: ToolName,
    schema: Arc<Value>,
    client: Client,
    api_key: SecretString,
}

impl WebSearchTool {
    pub fn new(client: Client, api_key: SecretString) -> Self {
        Self {
            name: ToolName::try_from("web_search").expect("static name is valid"),
            schema: Arc::new(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query.",
                        "maxLength": SEARCH_QUERY_MAX_BYTES,
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": SEARCH_MAX_COUNT,
                        "description": "Number of results to return (1-10, default 5)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })),
            client,
            api_key,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    query: SearchQuery,
    #[serde(default)]
    count: SearchCount,
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        "Search the public web via the Brave Search API and return the top results \
         (title, url, snippet). Use when the user asks about something requiring \
         up-to-date information."
    }

    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    #[instrument(name = "tool.web_search", skip_all, fields(patom.tool = "web_search"))]
    async fn execute(&self, input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        let Input { query, count } = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("web_search: {e}")))?;

        let count_str = count.get().to_string();
        let response = self
            .client
            .get(BRAVE_ENDPOINT)
            .timeout(SEARCH_TIMEOUT)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", self.api_key.expose())
            .query(&[("q", query.as_str()), ("count", count_str.as_str())])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            // §5: cap the upstream body before surfacing it; a runaway 50 MB error page
            // must not end up on a span attribute or in a tool result.
            let mut body = response.text().await?;
            truncate_to_char_boundary(&mut body, UPSTREAM_BODY_MAX_BYTES);
            return Err(ToolError::Upstream {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: BraveResponse = response.json().await?;
        let results = parsed.web.map(|w| w.results).unwrap_or_default();

        if results.is_empty() {
            return Ok(format!("No results for query: {}", query.as_str()));
        }

        let mut out = String::with_capacity(results.len() * 256);
        for (i, r) in results.iter().enumerate() {
            // §6: writing into a String is infallible — using `write!` keeps clippy happy
            // about avoiding repeated `format!` allocations and asserts that fact.
            let res = writeln!(
                out,
                "{idx}. {title}\n   {url}\n   {desc}\n",
                idx = i + 1,
                title = r.title,
                url = r.url,
                desc = r.description,
            );
            assert!(res.is_ok(), "writing to String never errors");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_rejects_empty_and_whitespace() {
        assert!(serde_json::from_value::<SearchQuery>(json!("")).is_err());
        assert!(serde_json::from_value::<SearchQuery>(json!("   \n")).is_err());
    }

    #[test]
    fn search_query_rejects_oversize() {
        let big = "a".repeat(SEARCH_QUERY_MAX_BYTES + 1);
        assert!(serde_json::from_value::<SearchQuery>(json!(big)).is_err());
    }

    #[test]
    fn search_query_trims_at_boundary() {
        let q: SearchQuery = serde_json::from_value(json!("  hello world  ")).expect("valid");
        assert_eq!(q.as_str(), "hello world");
    }

    #[test]
    fn search_count_rejects_zero_and_overflow() {
        assert!(serde_json::from_value::<SearchCount>(json!(0)).is_err());
        let too_big = u64::from(SEARCH_MAX_COUNT) + 1;
        assert!(serde_json::from_value::<SearchCount>(json!(too_big)).is_err());
    }

    #[test]
    fn search_count_default_matches_constant() {
        assert_eq!(SearchCount::default().get(), SEARCH_DEFAULT_COUNT);
    }
}
