//! Reconnect delay: doubles from `base` up to `max`, resets after a session
//! that actually reached Hello.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        assert!(base <= max, "backoff base must not exceed max");
        Backoff { base, max, next: base }
    }

    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }

    pub fn reset(&mut self) {
        self.next = self.base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_then_caps() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(800));
        let got: Vec<u128> = (0..6).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(got, vec![100, 200, 400, 800, 800, 800]);
    }

    #[test]
    fn reset_returns_to_base() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(800));
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
