//! Roster sync — shadow-mint silent members so the agent knows *who* everyone
//! is, not just who has posted.
//!
//! On `GUILD_CREATE` (the connect-time member sample) and each
//! `GUILD_MEMBER_ADD`/`UPDATE`, every non-bot member is materialized as a shadow
//! colleague via the directory. Unlike a message, a roster member is not tied to
//! a channel, so we only mint the identity — channel membership is topped up
//! lazily when the member first posts (the bridge's `ensure_channel`).

use tracing::warn;

use crate::auth::OrgId;

use super::bridge::BridgeDeps;
use super::error::DiscordError;
use super::event::{Author, GuildCreate, GuildMemberEvent};
use super::limits::{DISCORD_DISPLAY_NAME_MAX, DISCORD_ROSTER_MAX_MEMBERS};

/// Mint shadows for the members in a `GUILD_CREATE` snapshot (bounded, §5).
pub async fn sync_guild(
    deps: &BridgeDeps,
    org_id: OrgId,
    gc: &GuildCreate,
) -> Result<(), DiscordError> {
    let total = gc.members.len();
    if total > DISCORD_ROSTER_MAX_MEMBERS {
        // No silent caps (§2): say what was dropped from the connect-time sample.
        warn!(
            event = "discord.roster.sample_truncated",
            total,
            cap = DISCORD_ROSTER_MAX_MEMBERS,
        );
    }
    for member in gc.members.iter().take(DISCORD_ROSTER_MAX_MEMBERS) {
        mint_member(deps, org_id, &member.user, member.nick.as_deref()).await?;
    }
    Ok(())
}

/// Mint (or refresh) the shadow for a single member upsert event.
pub async fn sync_member(
    deps: &BridgeDeps,
    org_id: OrgId,
    ev: &GuildMemberEvent,
) -> Result<(), DiscordError> {
    mint_member(deps, org_id, &ev.user, ev.nick.as_deref()).await
}

/// Shadow-mint one member, skipping bots/apps (no human shadow for them).
async fn mint_member(
    deps: &BridgeDeps,
    org_id: OrgId,
    user: &Author,
    nick: Option<&str>,
) -> Result<(), DiscordError> {
    if user.bot {
        return Ok(());
    }
    let display = user
        .display_name(nick)
        .map(|d| d.chars().take(DISCORD_DISPLAY_NAME_MAX).collect::<String>());
    deps.directory
        .resolve_or_mint(org_id, &user.id, display.as_deref())
        .await?;
    Ok(())
}
