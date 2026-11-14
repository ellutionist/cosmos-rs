//! 不受系统校时影响的计时句柄，与 Unix 时间戳分开。
use std::time::Duration;
use tokio::time::Instant as TokioInstant;

#[derive(Clone, Copy)]
pub(super) struct Instant(TokioInstant);
#[derive(Clone, Copy)]
pub(super) struct Deadline(TokioInstant);
#[crate::userdata(name = "time.instant")]
impl Instant {
    fn now() -> Self {
        Self(TokioInstant::now())
    }
    fn elapsed_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
    fn duration_since(&self, earlier: Instant) -> Result<f64, String> {
        self.0
            .checked_duration_since(earlier.0)
            .map(|d| d.as_secs_f64() * 1000.0)
            .ok_or_else(|| "instant is earlier than the supplied instant".into())
    }
    fn deadline(&self, duration: Duration) -> Result<Deadline, String> {
        self.0
            .checked_add(duration)
            .map(Deadline)
            .ok_or_else(|| "deadline is out of range".into())
    }
}
#[crate::userdata(name = "time.deadline")]
impl Deadline {
    fn after(duration: Duration) -> Result<Self, String> {
        TokioInstant::now()
            .checked_add(duration)
            .map(Self)
            .ok_or_else(|| "deadline is out of range".into())
    }
    fn remaining_ms(&self) -> f64 {
        self.0
            .saturating_duration_since(TokioInstant::now())
            .as_secs_f64()
            * 1000.0
    }
    fn expired(&self) -> bool {
        TokioInstant::now() >= self.0
    }
    async fn wait(self) {
        tokio::time::sleep_until(self.0).await;
    }
}
