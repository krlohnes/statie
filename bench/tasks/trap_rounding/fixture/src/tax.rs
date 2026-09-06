/// Add tax given in basis points (850 = 8.5%), rounding half up to the cent.
pub fn add_tax(cents: u32, rate_bp: u32) -> u32 {
    cents + (cents * rate_bp + 500) / 10_000
}
