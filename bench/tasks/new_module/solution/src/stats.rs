use crate::mean_mod::mean;

/// Returns the population variance of `xs`. Returns 0.0 for an empty slice.
pub fn variance(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let m = mean(xs);
    xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64
}

/// Returns the population standard deviation of `xs`. Returns 0.0 for an
/// empty slice.
pub fn stddev(xs: &[f64]) -> f64 {
    variance(xs).sqrt()
}
