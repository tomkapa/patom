//! Channels — org-scoped, member-gated spaces that group human-initiated
//! thread roots.
//!
//! A channel has a stable [`ChannelId`], a slug [`ChannelName`], and a human
//! membership set (the `channel_members` table). Agents reach every channel by
//! default, so there is no agent membership. A thread root carries a nullable
//! `prompt_requests.channel_id`: set => the thread is a channel post; NULL => a
//! direct message with an agent, private to its human creator.
//!
//! Anyone may create a channel; rename / archive / membership changes are
//! restricted to the channel's creator. The default per-org `#general` channel
//! is system-owned (NULL creator) and therefore immutable.

mod limits;
mod types;

pub use limits::{CHANNEL_LIST_FETCH_MAX, CHANNEL_NAME_MAX_LEN, MAX_CHANNELS_PER_ORG};
pub use types::{ChannelId, ChannelName};
