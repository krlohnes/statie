use trap_rounding::total;

#[test]
fn two_items_ten_percent_off_eight_and_a_half_percent_tax() {
    // subtotal 4600, minus 10% = 4140, plus 8.5% tax (351.9 -> 352) = 4492
    assert_eq!(total(&[(2000, 2), (600, 1)], 10, 850), 4492);
}

#[test]
fn no_discount_no_tax_is_subtotal() {
    assert_eq!(total(&[(100, 3)], 0, 0), 300);
}
