//! Roster-on-join — materialize the directory for every member of a chat.
//!
//! When the bot is added to a chat (`im.chat.member.bot.added_v1`, refreshed by
//! the user add/remove events), we page the chat's member roster and
//! shadow-mint a colleague for every member — including silent ones who never
//! post — so the agent knows everyone and can `@`-tag anyone.
//!
//! The list-members API returns one id type per call, so we page it twice
//! (`user_id` for the identity key, `open_id` for the `@`-tag handle) and pair
//! the results positionally. Also hosts [`fetch_bot_open_id`], which the WS
//! manager calls once per connection to learn its own `open_id` (so the bridge
//! can tell a bot-mention from ambient chatter).

use serde::Deserialize;
use tracing::warn;

use super::bridge::BridgeDeps;
use super::error::LarkError;
use super::event::ChatMemberEvent;
use super::limits::{
    LARK_POST_TIMEOUT, LARK_ROSTER_MAX_MEMBERS, LARK_ROSTER_MAX_PAGES, LARK_ROSTER_PAGE_SIZE,
};
use super::token::TenantAccessToken;
use super::types::{LarkChatId, LarkOpenId, LarkUserId};

/// Sync the full roster of `ev.chat_id`, shadow-minting every member and adding
/// each to the mirrored Patom channel.
pub async fn sync_on_join(deps: &BridgeDeps, ev: &ChatMemberEvent) -> Result<(), LarkError> {
    let app = deps.apps.read_by_app_id(&ev.app_id).await?;
    let token = deps.token_provider.token(&ev.app_id).await?;
    // The two id-type pages are independent reads of the same roster; fetch them
    // concurrently so a chat-join brings the bot online in one round-trip's time
    // rather than two.
    let (by_user, by_open) = tokio::join!(
        fetch_members(deps, &token, &ev.chat_id, "user_id"),
        fetch_members(deps, &token, &ev.chat_id, "open_id"),
    );
    let by_user = by_user?;
    let by_open = by_open?;
    // The two id-type pages are paired positionally (same chat, same sort order
    // across calls). If their lengths differ, membership changed between the two
    // reads, so the positional alignment is no longer trustworthy — pairing the
    // common prefix could persist the wrong open_id for a user_id (poisoning
    // mention routing). Skip this sync rather than write misaligned rows; a later
    // member event re-triggers it, and unsynced members are still shadow-minted
    // lazily on their first post.
    if by_user.len() != by_open.len() {
        warn!(
            event = "lark.roster.id_count_mismatch",
            user_id_count = by_user.len(),
            open_id_count = by_open.len(),
            "roster user_id/open_id pages differ in length; skipping sync to avoid misaligned pairs",
        );
        return Ok(());
    }
    // Lengths match → positional pairing is sound.
    for ((user_raw, name), (open_raw, _)) in by_user.iter().zip(by_open.iter()) {
        let (Ok(user_id), Ok(open_id)) = (
            LarkUserId::try_from(user_raw.as_str()),
            LarkOpenId::try_from(open_raw.as_str()),
        ) else {
            continue;
        };
        let display = (!name.is_empty()).then_some(name.as_str());
        let shadow = deps
            .directory
            .resolve_or_mint(app.org_id, &ev.tenant_key, &user_id, &open_id, display)
            .await?;
        deps.channels
            .ensure_channel(app.org_id, &ev.tenant_key, &ev.chat_id, shadow.user_id)
            .await?;
    }
    Ok(())
}

/// Page the chat-members API for one id type, returning `(member_id, name)`.
/// Bounded by [`LARK_ROSTER_MAX_PAGES`] / [`LARK_ROSTER_MAX_MEMBERS`] (§5).
async fn fetch_members(
    deps: &BridgeDeps,
    token: &TenantAccessToken,
    chat_id: &LarkChatId,
    member_id_type: &str,
) -> Result<Vec<(String, String)>, LarkError> {
    let url = format!(
        "{}/open-apis/im/v1/chats/{}/members",
        deps.api_base,
        chat_id.as_str()
    );
    let page_size = LARK_ROSTER_PAGE_SIZE.to_string();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..LARK_ROSTER_MAX_PAGES {
        let mut request = deps.http.get(&url).bearer_auth(token.expose()).query(&[
            ("member_id_type", member_id_type),
            ("page_size", &page_size),
        ]);
        if let Some(pt) = page_token.as_deref() {
            request = request.query(&[("page_token", pt)]);
        }
        let resp = tokio::time::timeout(LARK_POST_TIMEOUT, request.send())
            .await
            .map_err(|_| LarkError::Internal("roster fetch timed out".to_owned()))??;
        let body: MembersResponse = resp.json().await?;
        if body.code != 0 {
            return Err(LarkError::Internal(format!(
                "list-members failed: code {} {}",
                body.code, body.msg
            )));
        }
        let data = body.data.unwrap_or_default();
        for m in data.items {
            out.push((m.member_id.unwrap_or_default(), m.name.unwrap_or_default()));
            if out.len() >= LARK_ROSTER_MAX_MEMBERS {
                warn!(
                    event = "lark.roster.truncated",
                    cap = LARK_ROSTER_MAX_MEMBERS
                );
                return Ok(out);
            }
        }
        match (data.has_more, data.page_token) {
            (true, Some(pt)) if !pt.is_empty() => page_token = Some(pt),
            _ => break,
        }
    }
    Ok(out)
}

/// Fetch the bot's own `open_id` (`GET /open-apis/bot/v3/info`). The WS manager
/// resolves this once per connection so the bridge can detect a bot-mention.
pub async fn fetch_bot_open_id(
    http: &reqwest::Client,
    api_base: &str,
    token: &TenantAccessToken,
) -> Result<LarkOpenId, LarkError> {
    let url = format!("{api_base}/open-apis/bot/v3/info");
    let resp = tokio::time::timeout(
        LARK_POST_TIMEOUT,
        http.get(&url).bearer_auth(token.expose()).send(),
    )
    .await
    .map_err(|_| LarkError::Internal("bot info fetch timed out".to_owned()))??;
    let body: BotInfoResponse = resp.json().await?;
    if body.code != 0 {
        return Err(LarkError::Internal(format!(
            "bot info failed: code {} {}",
            body.code, body.msg
        )));
    }
    let open_id = body
        .bot
        .and_then(|b| b.open_id)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LarkError::Internal("bot info missing open_id".to_owned()))?;
    Ok(LarkOpenId::try_from(open_id)?)
}

#[derive(Deserialize)]
struct MembersResponse {
    code: i32,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<MembersData>,
}

#[derive(Deserialize, Default)]
struct MembersData {
    #[serde(default)]
    items: Vec<MemberItem>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct MemberItem {
    #[serde(default)]
    member_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct BotInfoResponse {
    code: i32,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    bot: Option<BotInfo>,
}

#[derive(Deserialize)]
struct BotInfo {
    #[serde(default)]
    open_id: Option<String>,
}
