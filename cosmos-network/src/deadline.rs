//! 各网络协议共用的截止时间与毫秒超时转换。

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// Some 时到点返回 Err(())，None 时无限等待，由调用方转换协议错误。
pub(crate) async fn with_deadline<F, T>(deadline: Option<Instant>, fut: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(d) => tokio::time::timeout_at(d, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}

/// 网络接口的毫秒超时：<= 0 表示清除超时。
pub(crate) fn timeout_from_ms(ms: i64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms as u64))
}
