use fix_off_by_one::chunk_count;

#[test]
fn exact_multiple() {
    assert_eq!(chunk_count(10, 5), 2);
}

#[test]
fn rounds_up_for_partial_final_chunk() {
    assert_eq!(chunk_count(11, 5), 3);
}

#[test]
fn single_leftover_element() {
    assert_eq!(chunk_count(1, 5), 1);
}

#[test]
fn size_zero_returns_zero() {
    assert_eq!(chunk_count(10, 0), 0);
}

#[test]
fn zero_len_returns_zero() {
    assert_eq!(chunk_count(0, 5), 0);
}
