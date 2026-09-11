/// Interest owed for one month, in whole cents.
///
/// `annual_rate_bp` is the nominal annual rate in basis points (750 =
/// 7.5%). The documented rule is: divide the annual rate by 12 to get
/// the monthly rate, multiply by the balance, and round the result half
/// up to the nearest cent -- all as a single division so the rounding
/// only happens once, at the end.
pub fn monthly_interest(balance_cents: i64, annual_rate_bp: i64) -> i64 {
    // Guard against a negative balance (the last payment of a schedule
    // can overshoot by a cent or two): interest never runs backwards.
    let balance_cents = balance_cents.max(0);
    (balance_cents * annual_rate_bp + 60_000) / 120_000
}
