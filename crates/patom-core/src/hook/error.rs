use thiserror::Error;

#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook denied operation: {0}")]
    Denied(String),

    #[error("hook backend error: {0}")]
    Backend(String),
}
