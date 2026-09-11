pub mod interest;
pub mod ledger;

/// One month's line in an amortisation schedule, all amounts in whole cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payment {
    pub interest_cents: i64,
    pub principal_cents: i64,
    pub balance_cents: i64,
}

/// Build the amortisation schedule for a fixed monthly payment.
///
/// `principal_cents` is the starting balance, `annual_rate_bp` is the
/// nominal annual rate in basis points (750 = 7.5%), and `payment_cents`
/// is the fixed amount paid each month (the last month is not special-
/// cased: whatever balance remains after `months` payments is the final
/// `balance_cents`, which the caller can check against zero).
pub fn schedule(
    principal_cents: i64,
    annual_rate_bp: i64,
    months: u32,
    payment_cents: i64,
) -> Vec<Payment> {
    let mut balance = principal_cents;
    let mut rows = Vec::with_capacity(months as usize);
    for _ in 0..months {
        let interest = interest::monthly_interest(balance, annual_rate_bp);
        let principal_part = ledger::apply_payment(payment_cents, interest);
        balance = ledger::reduce_balance(balance, principal_part);
        rows.push(Payment {
            interest_cents: interest,
            principal_cents: principal_part,
            balance_cents: balance,
        });
    }
    rows
}
