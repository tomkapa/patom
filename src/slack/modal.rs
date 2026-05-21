//! Block Kit JSON builder for the `/relay` compose modal.
//!
//! The modal is the entry point for tenant-scoped agent selection: a
//! user invokes `/relay`, Slack POSTs the slash command to
//! [`super::interactions`], the handler resolves the tenant's roster
//! and builds the JSON returned here. Slack opens the modal client-side
//! and POSTs the user's selection back as `view_submission`.
//!
//! This module is pure — no async, no IO, no DB. It exists as its own
//! file so the JSON shape can be exhaustively snapshot-tested without
//! pulling the rest of the Slack adapter into the test compilation
//! unit.

use serde_json::{Value, json};

use crate::agents::{AgentId, AgentName};

use super::limits::{MAX_AGENTS_IN_PICKER, SLACK_SLASH_PROMPT_MAX_CHARS};

/// Stable identifier Slack echoes back in the `view_submission`
/// envelope. Must match the dispatcher branch in [`super::interactions`].
pub const COMPOSE_CALLBACK_ID: &str = "relay_compose";

/// `block_id` on the agent select block. The submit handler reads
/// `view.state.values["agent"]["pick"].selected_option.value`.
pub const AGENT_BLOCK_ID: &str = "agent";
/// `action_id` on the agent select element.
pub const AGENT_ACTION_ID: &str = "pick";
/// `block_id` on the prompt text input.
pub const PROMPT_BLOCK_ID: &str = "prompt";
/// `action_id` on the prompt text input element.
pub const PROMPT_ACTION_ID: &str = "text";

/// Build the modal that the `/relay` slash command returns inline.
///
/// `agents` is the tenant's full roster as returned by
/// [`crate::agents::AgentStore::list_for_org`]; it is truncated here to
/// [`MAX_AGENTS_IN_PICKER`] entries (Slack `static_select` hard cap).
/// The caller has already alphabetised by `lower(name)`.
///
/// `private_metadata` is the opaque routing payload Slack echoes back
/// on `view_submission`. The caller serialises a JSON object containing
/// `team_id`, `channel_id`, `user_id` and the conversation's invocation
/// context here; this builder treats it as an opaque string.
///
/// When `agents` is empty the modal degrades to an empty-state shape
/// with no submit button — the user sees a "no agents configured"
/// notice and dismisses.
#[must_use]
pub fn build_compose_modal(agents: &[(AgentId, AgentName)], private_metadata: &str) -> Value {
    assert!(
        private_metadata.len() <= super::limits::MAX_PRIVATE_METADATA_BYTES,
        "private_metadata exceeds Slack's bound"
    );
    if agents.is_empty() {
        return empty_state_modal(private_metadata);
    }
    let trimmed_overflow = agents.len() > MAX_AGENTS_IN_PICKER;
    let visible = &agents[..agents.len().min(MAX_AGENTS_IN_PICKER)];
    let options: Vec<Value> = visible
        .iter()
        .map(|(id, name)| {
            json!({
                "text": { "type": "plain_text", "text": name.as_str() },
                "value": id.as_uuid().to_string(),
            })
        })
        .collect();

    let mut blocks = Vec::with_capacity(4);
    blocks.push(json!({
        "type": "input",
        "block_id": AGENT_BLOCK_ID,
        "label": { "type": "plain_text", "text": "Agent" },
        "element": {
            "type": "static_select",
            "action_id": AGENT_ACTION_ID,
            "placeholder": { "type": "plain_text", "text": "Select an agent" },
            "options": options,
        }
    }));
    blocks.push(json!({
        "type": "input",
        "block_id": PROMPT_BLOCK_ID,
        "label": { "type": "plain_text", "text": "Prompt" },
        "element": {
            "type": "plain_text_input",
            "action_id": PROMPT_ACTION_ID,
            "multiline": true,
            "max_length": SLACK_SLASH_PROMPT_MAX_CHARS,
        }
    }));
    if trimmed_overflow {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": format!(
                    "Showing the first {MAX_AGENTS_IN_PICKER} agents. \
                     Don't see yours? Mention `@RelayBot <agent-name>` instead."
                ),
            }],
        }));
    }

    json!({
        "type": "modal",
        "callback_id": COMPOSE_CALLBACK_ID,
        "private_metadata": private_metadata,
        "title": { "type": "plain_text", "text": "Relay" },
        "submit": { "type": "plain_text", "text": "Send" },
        "close": { "type": "plain_text", "text": "Cancel" },
        "blocks": blocks,
    })
}

fn empty_state_modal(private_metadata: &str) -> Value {
    json!({
        "type": "modal",
        "callback_id": COMPOSE_CALLBACK_ID,
        "private_metadata": private_metadata,
        "title": { "type": "plain_text", "text": "Relay" },
        "close": { "type": "plain_text", "text": "Close" },
        "blocks": [{
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": "*No agents are configured for this workspace yet.* \
                         Ask your administrator to create one in Relay, \
                         then re-run `/relay`.",
            },
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> (AgentId, AgentName) {
        (
            AgentId::new(),
            AgentName::try_from(name).expect("valid name"),
        )
    }

    #[test]
    fn empty_list_returns_empty_state_with_no_submit_button() {
        let v = build_compose_modal(&[], "{}");
        assert_eq!(v["type"], "modal");
        assert_eq!(v["callback_id"], COMPOSE_CALLBACK_ID);
        // No submit button in the empty state — the user can only close.
        assert!(v.get("submit").is_none(), "got: {v}");
        let blocks = v["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "section");
    }

    #[test]
    fn single_agent_renders_picker_and_prompt() {
        let alice = agent("alice");
        let v = build_compose_modal(std::slice::from_ref(&alice), "{\"team_id\":\"T\"}");
        assert_eq!(v["type"], "modal");
        assert_eq!(v["callback_id"], COMPOSE_CALLBACK_ID);
        assert_eq!(v["private_metadata"], "{\"team_id\":\"T\"}");
        assert_eq!(v["submit"]["text"], "Send");

        let blocks = v["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);

        let agent_block = &blocks[0];
        assert_eq!(agent_block["block_id"], AGENT_BLOCK_ID);
        assert_eq!(agent_block["element"]["action_id"], AGENT_ACTION_ID);
        let options = agent_block["element"]["options"]
            .as_array()
            .expect("options");
        assert_eq!(options.len(), 1);
        assert_eq!(options[0]["text"]["text"], "alice");
        assert_eq!(options[0]["value"], alice.0.as_uuid().to_string());

        let prompt_block = &blocks[1];
        assert_eq!(prompt_block["block_id"], PROMPT_BLOCK_ID);
        assert_eq!(prompt_block["element"]["action_id"], PROMPT_ACTION_ID);
        assert_eq!(prompt_block["element"]["multiline"], true);
        assert_eq!(
            prompt_block["element"]["max_length"],
            SLACK_SLASH_PROMPT_MAX_CHARS
        );
    }

    #[test]
    fn over_cap_truncates_and_adds_overflow_hint() {
        // Build MAX + 5 agents; the modal must surface exactly MAX in the
        // select and one trailing context block warning about the cap.
        let mut roster: Vec<(AgentId, AgentName)> = Vec::new();
        for i in 0..(MAX_AGENTS_IN_PICKER + 5) {
            roster.push(agent(&format!("agent-{i:03}")));
        }
        let v = build_compose_modal(&roster, "{}");
        let blocks = v["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 3, "agent + prompt + overflow-hint context");
        let options = blocks[0]["element"]["options"].as_array().expect("options");
        assert_eq!(options.len(), MAX_AGENTS_IN_PICKER);
        assert_eq!(blocks[2]["type"], "context");
        let hint = blocks[2]["elements"][0]["text"]
            .as_str()
            .expect("hint text");
        let lower = hint.to_ascii_lowercase();
        assert!(lower.contains("first"), "got: {hint}");
        assert!(lower.contains("mention"), "got: {hint}");
    }

    #[test]
    fn at_cap_omits_overflow_hint() {
        let mut roster: Vec<(AgentId, AgentName)> = Vec::new();
        for i in 0..MAX_AGENTS_IN_PICKER {
            roster.push(agent(&format!("agent-{i:03}")));
        }
        let v = build_compose_modal(&roster, "{}");
        let blocks = v["blocks"].as_array().expect("blocks");
        // Exactly two blocks: select + prompt, no context.
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    #[should_panic(expected = "private_metadata exceeds Slack's bound")]
    fn private_metadata_over_bound_panics() {
        let pm = "x".repeat(super::super::limits::MAX_PRIVATE_METADATA_BYTES + 1);
        let _ = build_compose_modal(&[], &pm);
    }
}
