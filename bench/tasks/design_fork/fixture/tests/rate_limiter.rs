use design_fork::RateLimiter;

/// A burst of exactly `capacity` calls at the same instant must all be
/// allowed, and the next one (still at that instant, no time passed) must
/// be denied.
#[test]
fn allows_a_burst_up_to_capacity_then_denies() {
    let mut limiter = RateLimiter::new(3, 1);
    assert!(limiter.allow(0), "1st call within capacity must be allowed");
    assert!(limiter.allow(0), "2nd call within capacity must be allowed");
    assert!(limiter.allow(0), "3rd call within capacity must be allowed");
    assert!(
        !limiter.allow(0),
        "4th call at the same instant must be denied: capacity is 3"
    );
}

/// After exhausting the burst, waiting long enough for the refill rate to
/// grant one more token must allow exactly one more call.
#[test]
fn refills_over_time_after_burst_is_exhausted() {
    let mut limiter = RateLimiter::new(2, 1); // 1 call per second refill.
    assert!(limiter.allow(0));
    assert!(limiter.allow(0));
    assert!(!limiter.allow(0), "capacity exhausted, must deny");

    // 1000ms later, refill_per_sec=1 must have granted at least one more.
    assert!(
        limiter.allow(1_000),
        "1 second later, one call per second must have refilled at least one slot"
    );
}

/// A single call, well under capacity, must never be denied merely by
/// asking again long before any refill is due -- denial only happens once
/// the burst is actually exhausted.
#[test]
fn a_lone_call_far_from_capacity_is_always_allowed() {
    let mut limiter = RateLimiter::new(5, 1);
    for t in [0u64, 1, 2, 3, 4] {
        assert!(
            limiter.allow(t),
            "call at t={t}ms, well under capacity 5, must be allowed"
        );
    }
}

/// Waiting long enough to refill past capacity must never let more than
/// `capacity` calls through in a row -- refilling does not accumulate
/// beyond the burst size.
#[test]
fn refill_never_exceeds_capacity() {
    let mut limiter = RateLimiter::new(2, 1);
    // Wait a very long time before the first call: refill must cap at
    // capacity (2), not grant an unbounded number of calls.
    let now = 60_000; // 60 seconds, i.e. 60 refills' worth at 1/sec.
    assert!(limiter.allow(now));
    assert!(limiter.allow(now));
    assert!(
        !limiter.allow(now),
        "refill must cap at capacity even after a long idle period"
    );
}

/// Calling at exactly the sustained rate (one call every `1000 /
/// refill_per_sec` ms) must never be denied, run over many iterations --
/// this is the "no negative drift" check: an implementation that
/// accumulates rounding error over repeated calls would eventually start
/// wrongly denying calls that are exactly on schedule.
#[test]
fn sustained_rate_calls_never_drift_into_denial() {
    let refill_per_sec = 5u32;
    let mut limiter = RateLimiter::new(1, refill_per_sec);
    let interval_ms = 1_000 / u64::from(refill_per_sec);

    assert!(limiter.allow(0), "first call must be allowed");

    for i in 1..2_000u64 {
        let now = i * interval_ms;
        assert!(
            limiter.allow(now),
            "call #{i} at t={now}ms, exactly on the sustained schedule, must be allowed \
             (no negative drift)"
        );
    }
}

/// A call that arrives faster than the sustained rate (i.e. before the
/// next token would have refilled) must be denied, even after many prior
/// calls have already succeeded -- this is the flip side of the drift
/// check: an implementation that drifts the other way (granting extra
/// tokens it hasn't earned yet) would wrongly allow this.
#[test]
fn a_call_faster_than_the_sustained_rate_is_denied() {
    let refill_per_sec = 2u32;
    let mut limiter = RateLimiter::new(1, refill_per_sec);
    let interval_ms = 1_000 / u64::from(refill_per_sec);

    assert!(limiter.allow(0));
    for i in 1..50u64 {
        let now = i * interval_ms;
        assert!(limiter.allow(now), "call #{i} on schedule must be allowed");
    }

    // One more call 1ms after the last allowed call, well before the next
    // token is due, must be denied.
    let last_now = 49 * interval_ms;
    assert!(
        !limiter.allow(last_now + 1),
        "a call 1ms after the last allowed one, before the next refill is due, must be denied"
    );
}
