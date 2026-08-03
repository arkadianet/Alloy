/// Moves `amount` from the balance at `from` to the balance at `to`.
///
/// Callers must pass two distinct indices that are both in range.
pub fn transfer(balances: &mut [i64], from: usize, to: usize, amount: i64) {
    let source = &mut balances[from];
    let destination = &mut balances[to];
    *source -= amount;
    *destination += amount;
}
