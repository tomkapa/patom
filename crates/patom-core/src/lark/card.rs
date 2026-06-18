//! Interactive approval card builders for Lark (issue #214).
//!
//! The *pending* card carries Approve / Deny buttons; each button's `value`
//! holds `{approval_id, decision}`, which Lark echoes back verbatim in the
//! `card.action.trigger` callback (`event.action.value`). The *resolved* card
//! replaces the buttons with a one-line outcome — returned inline in the HTTP
//! response to the click, so Lark swaps the card in place.

use serde_json::{Value, json};

use crate::approvals::{ActionSummary, ApprovalId, Decision};

/// The interactive prompt: a summary line plus Approve / Deny buttons whose
/// `value` round-trips `{approval_id, decision}` through the callback.
#[must_use]
pub fn pending_card(approval_id: ApprovalId, action: &ActionSummary) -> Value {
    let id = approval_id.as_uuid().to_string();
    json!({
        "config": { "wide_screen_mode": true, "update_multi": true },
        "elements": [
            {
                "tag": "div",
                "text": {
                    "tag": "lark_md",
                    "content": format!("🔔 **Approval needed**\n{}", action.as_str()),
                }
            },
            {
                "tag": "action",
                "actions": [
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": "Approve" },
                        "type": "primary",
                        "value": { "approval_id": id, "decision": Decision::Approved.tag().to_string() }
                    },
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": "Deny" },
                        "type": "danger",
                        "value": { "approval_id": id, "decision": Decision::Denied.tag().to_string() }
                    }
                ]
            }
        ]
    })
}

/// The resolved card (buttons removed) shown after a decision.
#[must_use]
pub fn resolved_card(decision: Decision, decider_name: &str, action: &ActionSummary) -> Value {
    let headline = match decision {
        Decision::Approved => format!("✅ **Approved** by {decider_name}"),
        Decision::Denied => format!("🚫 **Denied** by {decider_name}"),
    };
    json!({
        "config": { "wide_screen_mode": true },
        "elements": [
            {
                "tag": "div",
                "text": {
                    "tag": "lark_md",
                    "content": format!("{headline}\n{}", action.as_str()),
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action() -> ActionSummary {
        ActionSummary::try_from("Refund $40 to customer #12").expect("summary")
    }

    #[test]
    fn pending_card_buttons_carry_approval_id_and_decision() {
        let id = ApprovalId::new();
        let card = pending_card(id, &action());
        let actions = &card["elements"][1]["actions"];
        assert_eq!(actions[0]["value"]["approval_id"], id.as_uuid().to_string());
        assert_eq!(actions[0]["value"]["decision"], "a");
        assert_eq!(actions[1]["value"]["decision"], "d");
        assert_eq!(actions[1]["type"], "danger");
    }

    #[test]
    fn resolved_card_shows_decider_and_has_no_buttons() {
        let card = resolved_card(Decision::Approved, "Ali", &action());
        assert!(
            card["elements"]
                .as_array()
                .expect("elements")
                .iter()
                .all(|e| e["tag"] != "action"),
            "resolved card strips the action row"
        );
        let content = card["elements"][0]["text"]["content"]
            .as_str()
            .expect("content");
        assert!(content.contains("Approved"));
        assert!(content.contains("Ali"));
    }
}
