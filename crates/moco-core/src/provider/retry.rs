//! Retry policy: exponential from 2 s capped at 10 s, at most 10 retries,
//! jittered; a provider retry-after raises the wait (capped at 15 min); timeouts get at most 3 retries.
use super::{ProviderError, TransportKind};
use std::time::Duration;

pub const MAX_RETRIES: u32 = 10;
pub const MAX_TIMEOUT_RETRIES: u32 = 3;
const BASE: Duration = Duration::from_secs(2);
const CAP: Duration = Duration::from_secs(10);
const RETRY_AFTER_CAP: Duration = Duration::from_secs(15 * 60);

/// Wait before retry number `attempt` (1-based), or `None` if the error should surface.
/// `jitter` in [0,1) scales the backoff down by up to 25%.
pub fn delay(attempt: u32, err: &ProviderError, jitter: f64) -> Option<Duration> {
    if !err.retryable() || attempt == 0 || attempt > MAX_RETRIES {
        return None;
    }
    if matches!(err, ProviderError::Transport { kind: TransportKind::Timeout, .. }) && attempt > MAX_TIMEOUT_RETRIES {
        return None;
    }
    let backoff = (BASE * 2u32.saturating_pow(attempt - 1)).min(CAP).mul_f64(1.0 - 0.25 * jitter.clamp(0.0, 1.0));
    let after = match err {
        ProviderError::RateLimit { retry_after_ms: Some(ms), .. } | ProviderError::Internal { retry_after_ms: Some(ms), .. } => {
            Some(Duration::from_millis(*ms).min(RETRY_AFTER_CAP))
        }
        _ => None,
    };
    Some(after.map_or(backoff, |a| a.max(backoff)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(ms: Option<u64>) -> ProviderError {
        ProviderError::RateLimit { retry_after_ms: ms, message: "x".into() }
    }
    fn secs(d: Option<Duration>) -> Option<f64> {
        d.map(|d| d.as_secs_f64())
    }

    #[test]
    fn exponential_backoff_capped() {
        let e = ProviderError::Internal { retry_after_ms: None, message: "x".into() };
        let got: Vec<_> = (1..=6).map(|a| secs(delay(a, &e, 0.0)).unwrap()).collect();
        assert_eq!(got, [2.0, 4.0, 8.0, 10.0, 10.0, 10.0]);
        assert_eq!(secs(delay(1, &e, 1.0)), Some(1.5), "full jitter shaves 25%");
    }

    #[test]
    fn retry_limits() {
        assert!(delay(10, &rate(None), 0.0).is_some());
        assert!(delay(11, &rate(None), 0.0).is_none());
        let timeout = ProviderError::Transport { kind: TransportKind::Timeout, message: "t".into() };
        assert!(delay(3, &timeout, 0.0).is_some());
        assert!(delay(4, &timeout, 0.0).is_none());
        let reset = ProviderError::Transport { kind: TransportKind::Other, message: "reset".into() };
        assert!(delay(4, &reset, 0.0).is_some(), "only timeouts are limited to 3");
    }

    #[test]
    fn retry_after_raises_and_caps() {
        assert_eq!(secs(delay(1, &rate(Some(30_000)), 0.0)), Some(30.0));
        assert_eq!(secs(delay(1, &rate(Some(500)), 0.0)), Some(2.0), "never below backoff");
        assert_eq!(secs(delay(1, &rate(Some(99_999_999)), 0.0)), Some(900.0), "15 minute cap");
    }

    #[test]
    fn non_retryable_errors_surface() {
        for e in [
            ProviderError::Auth("a".into()),
            ProviderError::InvalidRequest("i".into()),
            ProviderError::ContextOverflow("c".into()),
            ProviderError::ContentFilter("f".into()),
            ProviderError::Interrupted,
        ] {
            assert!(delay(1, &e, 0.0).is_none(), "{e:?}");
        }
    }
}
