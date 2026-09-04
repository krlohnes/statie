/// Returns the number of chunks of size `size` needed to cover `len`
/// elements. Rounds up: a partial final chunk still counts. A `size` of
/// zero cannot divide anything, so it returns 0.
pub fn chunk_count(len: usize, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    (len + size - 1) / size
}
