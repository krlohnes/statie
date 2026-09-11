use trap_interest::{schedule, Payment};

// $1,000.00 principal, 7.5% APR, 6 months, a fixed $170.33 payment.
const PRINCIPAL_CENTS: i64 = 100_000;
const ANNUAL_RATE_BP: i64 = 750;
const MONTHS: u32 = 6;
const PAYMENT_CENTS: i64 = 17_033;

#[test]
fn six_month_schedule_matches_reference() {
    let got = schedule(PRINCIPAL_CENTS, ANNUAL_RATE_BP, MONTHS, PAYMENT_CENTS);
    let want = vec![
        Payment { interest_cents: 625, principal_cents: 16_408, balance_cents: 83_592 },
        Payment { interest_cents: 522, principal_cents: 16_511, balance_cents: 67_081 },
        Payment { interest_cents: 419, principal_cents: 16_614, balance_cents: 50_467 },
        Payment { interest_cents: 315, principal_cents: 16_718, balance_cents: 33_749 },
        Payment { interest_cents: 211, principal_cents: 16_822, balance_cents: 16_927 },
        Payment { interest_cents: 106, principal_cents: 16_927, balance_cents: 0 },
    ];
    assert_eq!(got, want, "\nexpected: {want:#?}\nactual:   {got:#?}");
}

#[test]
fn six_month_schedule_pays_off_to_zero() {
    let got = schedule(PRINCIPAL_CENTS, ANNUAL_RATE_BP, MONTHS, PAYMENT_CENTS);
    let final_balance = got.last().expect("schedule should have at least one row").balance_cents;
    assert_eq!(final_balance, 0, "schedule should pay off to exactly zero cents, got {final_balance}");
}
