//! Prefixed, time-ordered IDs (`msg_`, `ses_`, `per_`, ...). Generated ascending:
//! 14 hex chars of `(unix_ms << 12) | counter` followed by 14 random base62 chars.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LAST: AtomicU64 = AtomicU64::new(0);
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub fn new_id(prefix: &str) -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let floor = ms << 12;
    // Strictly increasing even within one millisecond or if the clock steps back.
    let ts = LAST
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(if floor > last { floor } else { last + 1 }))
        .map(|prev| if floor > prev { floor } else { prev + 1 })
        .unwrap();
    let rand: String = (0..14).map(|_| BASE62[fastrand::usize(..BASE62.len())] as char).collect();
    format!("{prefix}_{ts:014x}{rand}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_and_ascending() {
        let ids: Vec<String> = (0..1000).map(|_| new_id("msg")).collect();
        assert!(ids.iter().all(|i| i.starts_with("msg_") && i.len() == 4 + 14 + 14));
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids must sort in creation order");
    }
}
