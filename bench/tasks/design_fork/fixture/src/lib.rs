/// A rate limiter: allows up to `capacity` calls in a burst, then refills
/// at `refill_per_sec` calls per second.
pub struct RateLimiter {
    capacity: u32,
    refill_per_sec: u32,
}

impl RateLimiter {
    /// Creates a rate limiter that allows a burst of up to `capacity` calls
    /// and refills at `refill_per_sec` calls per second thereafter.
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        RateLimiter {
            capacity,
            refill_per_sec,
        }
    }

    /// Reports whether a call at `now_ms` (milliseconds since some fixed
    /// epoch, monotonically non-decreasing across calls) is allowed. Stub:
    /// always allows, which is wrong -- it never enforces the limit.
    pub fn allow(&mut self, _now_ms: u64) -> bool {
        let _ = (self.capacity, self.refill_per_sec);
        true
    }
}
