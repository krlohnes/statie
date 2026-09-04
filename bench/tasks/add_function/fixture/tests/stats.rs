use add_function::{mean, median};

#[test]
fn mean_of_a_few_numbers() {
    assert_eq!(mean(&[1.0, 2.0, 3.0]), 2.0);
}

#[test]
fn median_of_odd_length() {
    assert_eq!(median(&[5.0, 1.0, 3.0]), 3.0);
}

#[test]
fn median_of_even_length() {
    assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
}

#[test]
fn median_of_empty_slice_is_zero() {
    assert_eq!(median(&[]), 0.0);
}

#[test]
fn median_does_not_mutate_input_order() {
    let xs = [4.0, 1.0, 2.0];
    let _ = median(&xs);
    assert_eq!(xs, [4.0, 1.0, 2.0]);
}
