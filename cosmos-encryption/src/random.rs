//! 操作系统随机源；整数采用拒绝采样，不使用浮点缩放或取模偏差。
use crate::work;
use rand::{rngs::OsRng, RngCore};
const MAX_INTEGER: f64 = 9_007_199_254_740_991.0;
pub(super) struct Random;
pub(super) fn integer(value: f64, min: f64, max: f64, name: &str) -> Result<i64, String> {
    if !value.is_finite() || value.fract() != 0.0 || !(min..=max).contains(&value) {
        return Err(format!(
            "invalid {name} (expected an integer in {min}..={max})"
        ));
    }
    Ok(value as i64)
}
pub(super) fn fill(bytes: &mut [u8]) -> Result<(), String> {
    OsRng
        .try_fill_bytes(bytes)
        .map_err(|err| format!("OS randomness unavailable: {err}"))
}
fn sample(
    min: i64,
    max: i64,
    mut next: impl FnMut() -> Result<u64, String>,
) -> Result<i64, String> {
    let width = (max - min) as u64 + 1;
    let threshold = width.wrapping_neg() % width;
    loop {
        let value = next()?;
        if value >= threshold {
            return Ok(min + (value % width) as i64);
        }
    }
}
#[cosmos::userdata(name = "encryption.random")]
impl Random {
    async fn bytes(size: f64) -> Result<Vec<u8>, String> {
        let size = integer(size, 0.0, 1024.0 * 1024.0, "random byte count")? as usize;
        work::run(move || {
            move || {
                let mut bytes = vec![0; size];
                fill(&mut bytes)?;
                Ok(bytes)
            }
        })
        .await
    }
    async fn integer(min: f64, max: f64) -> Result<f64, String> {
        let min = integer(min, -MAX_INTEGER, MAX_INTEGER, "random minimum")?;
        let max = integer(max, -MAX_INTEGER, MAX_INTEGER, "random maximum")?;
        if min > max {
            return Err("random minimum exceeds maximum".into());
        }
        work::run(move || {
            move || {
                sample(min, max, || {
                    let mut bytes = [0; 8];
                    fill(&mut bytes)?;
                    Ok(u64::from_le_bytes(bytes))
                })
                .map(|value| value as f64)
            }
        })
        .await
    }
}
#[cfg(test)]
mod tests {
    use super::sample;
    #[test]
    fn rejection_sampling_covers_edges_and_propagates_entropy_errors() {
        let mut values = [0, 5, 6, u64::MAX].into_iter();
        assert_eq!(sample(-5, 4, || Ok(values.next().unwrap())).unwrap(), 1);
        assert_eq!(values.next(), Some(u64::MAX));
        assert_eq!(
            sample(-9_007_199_254_740_991, 9_007_199_254_740_991, || Ok(
                u64::MAX
            ))
            .unwrap(),
            -9_007_199_254_739_968
        );
        assert_eq!(sample(7, 7, || Ok(0)).unwrap(), 7);
        assert_eq!(
            sample(0, 3, || Err("entropy failed".into())),
            Err("entropy failed".into())
        );
    }
}
