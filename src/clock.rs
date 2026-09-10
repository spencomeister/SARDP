//! A process-local monotonic microsecond clock (spec 2.1: "時刻: マイクロ秒、
//! 送信者の単調時計基準"). Each side of a connection only needs its own
//! consistent monotonic reference; TimeSync (spec 2.9) is what correlates
//! readings across sides, not a shared epoch.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds elapsed since this process's first call to `now_us()`.
pub fn now_us() -> u64 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_monotonically_nondecreasing() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
    }
}
