/// A rate limiter: allows up to `capacity` calls in a burst, then refills
/// at `refill_per_sec` calls per second.
///
/// Token-bucket implementation: `tokens_milli` tracks available tokens in
/// thousandths (to avoid integer-division drift when computing how many
/// tokens `elapsed_ms` is worth), capped at `capacity * 1000`, and
/// `last_ms` is the timestamp `tokens_milli` was last brought up to date.
pub struct RateLimiter {
    capacity: u32,
    refill_per_sec: u32,
    tokens_milli: u64,
    last_ms: u64,
}

impl RateLimiter {
    /// Creates a rate limiter that allows a burst of up to `capacity` calls
    /// and refills at `refill_per_sec` calls per second thereafter. Starts
    /// full (a fresh limiter allows an immediate burst up to `capacity`).
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        RateLimiter {
            capacity,
            refill_per_sec,
            tokens_milli: u64::from(capacity) * 1000,
            last_ms: 0,
        }
    }

    /// Reports whether a call at `now_ms` (milliseconds since some fixed
    /// epoch, monotonically non-decreasing across calls) is allowed.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        let elapsed_ms = now_ms.saturating_sub(self.last_ms);
        self.last_ms = now_ms;

        // Exact integer arithmetic: elapsed_ms * refill_per_sec * 1000 /
        // 1000 tokens-in-thousandths refilled, done as one division at the
        // end so intermediate rounding never accumulates across calls.
        let refilled = elapsed_ms * u64::from(self.refill_per_sec);
        let cap_milli = u64::from(self.capacity) * 1000;
        self.tokens_milli = (self.tokens_milli + refilled).min(cap_milli);

        if self.tokens_milli >= 1000 {
            self.tokens_milli -= 1000;
            true
        } else {
            false
        }
    }
}
