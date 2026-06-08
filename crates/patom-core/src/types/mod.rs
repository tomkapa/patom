//! Domain primitives — the only types that cross module boundaries.
//!
//! Per CLAUDE.md §1: every value carrying an invariant (logical or business) is a newtype
//! with a `TryFrom` smart constructor. Bare `String` / `u32` / `Uuid` are reserved for values
//! that genuinely have none.

mod agent_reply;
mod avatar_url;
mod error;
mod limits;
mod macros;
mod model_id;
mod participant;
mod prompt;
mod secret;
mod tool_name;

pub use agent_reply::AgentReply;
pub use avatar_url::{AVATAR_URL_MAX_BYTES, AvatarUrl};
pub use error::ParseError;
pub use limits::{MAX_OUTPUT_TOKENS_CAP, MAX_TURNS_CAP, MaxOutputTokens, MaxTurns, TurnIndex};
pub use model_id::ModelId;
pub use participant::{MessageSender, MessageSenderKind, Participant, ParticipantKind};
pub use prompt::{PROMPT_MAX_BYTES, Prompt};
pub use secret::SecretString;
pub use tool_name::{TOOL_NAME_MAX_LEN, ToolName};
