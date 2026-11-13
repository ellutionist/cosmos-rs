//! 协程同步原语：有界 channel、通知、信号量和互斥锁。
//! 等待方法使用拥有型共享句柄，不跨 await 持有 Lua userdata 的借用守卫。

mod channel;
mod mutex;
mod notify;
mod semaphore;

use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use crate::ffi::*;
use crate::userdata::Value;
use crate::userdata::{register, FromLua};
use channel::Channel;
use mutex::Mutex;
use notify::Notification;
use semaphore::{Permit, Semaphore};

/// channel 容量和信号量配额使用相同的严格正整数转换，拒绝截断小数。
struct Count(usize);

impl<'lua> FromLua<'lua> for Count {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let count = value.read::<f64>()?;
        let max = (u32::MAX as usize).min(tokio::sync::Semaphore::MAX_PERMITS);
        if !count.is_finite() || count.fract() != 0.0 || !(1.0..=max as f64).contains(&count) {
            return Err(value.expected(&format!("integer in 1..={max}")));
        }
        Ok(Self(count as usize))
    }
}

/// 所有等待接口的末参为可选毫秒超时；缺省/nil 表示无限等待，0 表示只尝试立即完成。
struct Timeout(Option<Duration>);

impl<'lua> FromLua<'lua> for Timeout {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        Ok(Self(value.read::<Option<Duration>>()?))
    }
}

impl Timeout {
    async fn wait<T>(
        self,
        future: impl Future<Output = Result<T, &'static str>>,
    ) -> Result<T, &'static str> {
        match self.0 {
            Some(duration) if duration.is_zero() => {
                // 零超时只 poll 一次，不把机会让给其他任务，也不依赖计时器精度。
                tokio::pin!(future);
                std::future::poll_fn(|cx| {
                    Poll::Ready(match future.as_mut().poll(cx) {
                        Poll::Ready(result) => result,
                        Poll::Pending => Err("timeout"),
                    })
                })
                .await
            }
            Some(duration) => tokio::time::timeout(duration, future)
                .await
                .map_err(|_| "timeout")?,
            None => future.await,
        }
    }
}

/// 注册同步原语，父模块 cosmos.sync 由各类型的声明路径自动组装。
///
/// # Safety
/// lua_state 必须属于完成 userdata::init 的存活 VM，且由当前线程独占操作。
pub(super) unsafe fn register_types(lua_state: *mut lua_State) {
    register::<Channel>(lua_state);
    register::<Notification>(lua_state);
    register::<Semaphore>(lua_state);
    register::<Mutex>(lua_state);
    register::<Permit>(lua_state);
}
