//! Log-formatting helpers shared by the agent loop, the worker, and tools.

/// Bytes from a free-form string to inline in a log line. Long enough to read
/// intent at a glance, short enough that large outputs don't drown the log.
pub const PREVIEW_MAX_BYTES: usize = 200;

/// Single-line, length-bounded preview for log fields.
///
/// Newlines collapse to spaces so the row stays single-line; truncation
/// happens on a UTF-8 boundary and appends `…`. Walks the input lazily so a
/// multi-megabyte string never allocates more than `PREVIEW_MAX_BYTES`.
#[must_use]
pub fn preview(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(PREVIEW_MAX_BYTES));
    let mut truncated = false;
    for c in s.chars() {
        let mapped = if c == '\n' || c == '\r' { ' ' } else { c };
        if out.len() + mapped.len_utf8() > PREVIEW_MAX_BYTES {
            truncated = true;
            break;
        }
        out.push(mapped);
    }
    if truncated {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_newlines_to_spaces() {
        assert_eq!(preview("a\nb\rc"), "a b c");
    }

    #[test]
    fn passes_short_strings_through() {
        assert_eq!(preview("hello"), "hello");
    }

    #[test]
    fn truncates_with_ellipsis() {
        let long = "a".repeat(PREVIEW_MAX_BYTES + 50);
        let out = preview(&long);
        assert!(out.ends_with('…'));
        assert!(out.len() <= PREVIEW_MAX_BYTES + 4);
    }
}
