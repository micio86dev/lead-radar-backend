//! Job state machine and retry policy. Pure functions so the worker loop cannot invent its own
//! arithmetic: exhaustion moves to the dead-letter state instead of looping forever.

use serde::{Deserialize, Serialize};

pub const MAX_BACKOFF_SECONDS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FailureOutcome {
    Retry { attempt: i64, delay_seconds: i64 },
    DeadLetter { attempt: i64 },
}

/// Called after a failure: `attempts` counts the attempt that just failed.
pub fn on_failure(attempts: i64, max_attempts: i64, base_seconds: i64) -> FailureOutcome {
    if attempts >= max_attempts {
        FailureOutcome::DeadLetter { attempt: attempts }
    } else {
        FailureOutcome::Retry {
            attempt: attempts,
            delay_seconds: backoff_seconds(base_seconds, attempts),
        }
    }
}

/// Exponential backoff: base, 2x, 4x, ... capped at one hour.
pub fn backoff_seconds(base_seconds: i64, attempt: i64) -> i64 {
    let base = base_seconds.max(1);
    let exponent = (attempt.max(1) - 1).min(20) as u32;
    base.saturating_mul(2i64.saturating_pow(exponent))
        .min(MAX_BACKOFF_SECONDS)
}

/// A job is claimable when it is queued and due, or running with an expired lease (interrupted by a
/// crash and therefore recoverable).
pub fn is_claimable(status: &str, has_expired_lease: bool) -> bool {
    status == "queued" || (status == "running" && has_expired_lease)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff_seconds(5, 1), 5);
        assert_eq!(backoff_seconds(5, 2), 10);
        assert_eq!(backoff_seconds(5, 3), 20);
        assert_eq!(backoff_seconds(5, 30), MAX_BACKOFF_SECONDS);
        assert_eq!(backoff_seconds(0, 1), 1);
    }

    #[test]
    fn exhaustion_moves_a_job_to_the_dead_letter_state() {
        assert_eq!(
            on_failure(1, 3, 5),
            FailureOutcome::Retry {
                attempt: 1,
                delay_seconds: 5
            }
        );
        assert_eq!(
            on_failure(2, 3, 5),
            FailureOutcome::Retry {
                attempt: 2,
                delay_seconds: 10
            }
        );
        assert_eq!(
            on_failure(3, 3, 5),
            FailureOutcome::DeadLetter { attempt: 3 }
        );
        assert_eq!(
            on_failure(4, 3, 5),
            FailureOutcome::DeadLetter { attempt: 4 }
        );
    }

    #[test]
    fn only_due_or_expired_jobs_are_claimable() {
        assert!(is_claimable("queued", false));
        assert!(is_claimable("running", true));
        assert!(!is_claimable("running", false));
        assert!(!is_claimable("succeeded", true));
        assert!(!is_claimable("dead_letter", true));
    }
}
