pub mod cart;
pub mod discount;
pub mod tax;

/// Total in cents: subtotal, then the percentage discount, then tax.
pub fn total(items: &[(u32, u32)], discount_percent: u32, tax_bp: u32) -> u32 {
    let sub = cart::subtotal(items);
    let discounted = discount::apply_discount(sub, discount_percent);
    tax::add_tax(discounted, tax_bp)
}
