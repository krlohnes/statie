/// Sum of unit price (cents) times quantity.
pub fn subtotal(items: &[(u32, u32)]) -> u32 {
    items.iter().map(|(price, qty)| price * qty).sum()
}
