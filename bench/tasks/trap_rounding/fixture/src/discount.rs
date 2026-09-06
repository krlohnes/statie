/// Apply a whole-number percentage discount, truncating to the cent.
pub fn apply_discount(cents: u32, percent: u32) -> u32 {
    cents / 100 * (100 - percent)
}
