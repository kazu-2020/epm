pub fn normalize(value: f64) -> Option<f64> {
    if !value.is_finite() {
        None
    } else if value == 0.0 {
        Some(0.0)
    } else {
        Some(value)
    }
}
