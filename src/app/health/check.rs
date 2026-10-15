//! Check states, as the health event reports them (protocol:
//! `health_event.md`, "Checks").

use std::fmt;

/// State of one health check.
///
/// `Unknown` means the check could not be evaluated. It never means
/// "probably fine".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckState {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl CheckState {
    /// The value used in the event's `check` tags.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::Ok => "ok",
            CheckState::Warn => "warn",
            CheckState::Fail => "fail",
            CheckState::Unknown => "unknown",
        }
    }
}

impl fmt::Display for CheckState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_states_use_the_protocol_spelling() {
        let all = [
            (CheckState::Ok, "ok"),
            (CheckState::Warn, "warn"),
            (CheckState::Fail, "fail"),
            (CheckState::Unknown, "unknown"),
        ];
        for (state, expected) in all {
            assert_eq!(state.as_str(), expected);
            assert_eq!(state.to_string(), expected);
        }
    }
}
