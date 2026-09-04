use mylib::stats::{stddev, variance};

fn approx_eq(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
}

#[test]
fn variance_of_constant_values_is_zero() {
    approx_eq(variance(&[5.0, 5.0, 5.0]), 0.0);
}

#[test]
fn variance_of_a_known_set() {
    // Population variance of [2, 4, 4, 4, 5, 5, 7, 9] is 4.0.
    let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
    approx_eq(variance(&xs), 4.0);
}

#[test]
fn variance_of_empty_slice_is_zero() {
    approx_eq(variance(&[]), 0.0);
}

#[test]
fn stddev_of_a_known_set() {
    let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
    approx_eq(stddev(&xs), 2.0);
}

#[test]
fn stddev_of_empty_slice_is_zero() {
    approx_eq(stddev(&[]), 0.0);
}
