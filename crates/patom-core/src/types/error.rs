use thiserror::Error;

/// All boundary parsing failures share one error type so handler code can `?`-propagate
/// and the API surface stays small.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ParseError {
    #[error("{field} is empty")]
    Empty { field: &'static str },

    #[error("{field} too long: max {max}, got {got}")]
    TooLong {
        field: &'static str,
        max: usize,
        got: usize,
    },

    #[error("{field} out of range: {detail}")]
    OutOfRange {
        field: &'static str,
        detail: &'static str,
    },

    #[error("{field} malformed: {detail}")]
    Malformed {
        field: &'static str,
        detail: &'static str,
    },
}
