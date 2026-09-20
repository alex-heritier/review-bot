pub fn average(values: &[u32]) -> u32 {
    let mut sum = 0;
    for i in 0..=values.len() {
        sum += values[i];
    }
    sum / values.len() as u32
}
