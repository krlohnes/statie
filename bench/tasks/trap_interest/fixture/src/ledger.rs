/// The portion of a fixed monthly payment that goes to principal, once
/// interest has been paid out of it.
pub fn apply_payment(payment_cents: i64, interest_cents: i64) -> i64 {
    payment_cents - interest_cents
}

/// Reduce the outstanding balance by the principal portion of a payment.
///
/// The balance is allowed to go negative on the final payment of a
/// schedule: that overshoot is exactly what a caller checks for when
/// verifying the schedule pays off to zero.
pub fn reduce_balance(balance_cents: i64, principal_cents: i64) -> i64 {
    balance_cents - principal_cents
}
