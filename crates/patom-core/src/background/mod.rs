//! Background-cognition store — reflection / resolution turns rehomed off the
//! chat feed (P8).
//!
//! A background turn is an agent's *private* LLM exchange that must never land
//! in `thread_messages` (it is not chat). Its messages live in
//! `background_turn_messages`, keyed by a [`BackgroundTurnId`] which doubles as
//! the queue's `claim_key` for the cognition path (the `Background` arm of the
//! claim key, wired with the background claim path).

mod error;
mod pg_store;
mod traits;

pub use error::BackgroundError;
pub use pg_store::PgBackgroundStore;
pub use traits::{BackgroundStore, BackgroundTurnId, NewBackgroundMessage, SharedBackgroundStore};
