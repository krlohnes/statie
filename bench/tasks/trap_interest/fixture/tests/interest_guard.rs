use trap_interest::interest::monthly_interest;

// A schedule's last payment can overshoot the remaining balance by a
// cent or two (see tests/schedule.rs), which hands a negative balance
// into the next call. Interest on a negative balance must clamp to
// zero rather than going negative itself.
#[test]
fn negative_balance_clamps_interest_to_zero() {
    assert_eq!(monthly_interest(-2, 750), 0);
    assert_eq!(monthly_interest(-500, 1200), 0);
}
