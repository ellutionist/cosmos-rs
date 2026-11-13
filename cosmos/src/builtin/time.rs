//! 内建时间模块：协程休眠与当前系统时间，通过 cosmos.time 或 require("cosmos.time") 访问。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) struct Time;

#[cosmos::userdata(name = "time")]
impl Time {
    /// 毫秒休眠；参数转换完成校验后再挂起协程，成功不返回值。
    async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    /// 当前 Unix 时间戳，单位为毫秒，随系统时间调整。
    fn now_ms() -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_millis() as i64,
            Err(error) => -(error.duration().as_millis() as i64),
        }
    }

    /// 当前 Unix 时间戳，单位为微秒，随系统时间调整。
    fn now_us() -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_micros() as i64,
            Err(error) => -(error.duration().as_micros() as i64),
        }
    }
}
