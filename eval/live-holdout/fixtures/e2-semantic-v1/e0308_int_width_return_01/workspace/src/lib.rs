/// Sums the byte lengths of every frame in `frame_lengths`.
///
/// The running total is accumulated and reported as a 64-bit count, so that a
/// long run of maximum-size frames can never overflow the reported value.
pub fn total_bytes(frame_lengths: &[u32]) -> u32 {
    let mut total: u64 = 0;
    for &len in frame_lengths {
        total += u64::from(len);
    }
    total
}
