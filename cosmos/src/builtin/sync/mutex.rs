//! 非重入互斥锁；with_lock 将锁的生命周期绑定到回调 Future。

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::userdata::{LuaFunction, LuaValue};

use super::semaphore::Permit;
use super::Timeout;

#[derive(Clone)]
pub(super) struct Mutex(Arc<Semaphore>);

#[cosmos::userdata(name = "sync.mutex")]
impl Mutex {
    fn new() -> Self {
        Self(Arc::new(Semaphore::new(1)))
    }
    async fn lock(self, timeout: Timeout) -> Result<Permit, &'static str> {
        timeout
            .wait(async {
                self.0
                    .acquire_owned()
                    .await
                    .map(Permit::new)
                    .map_err(|_| "closed")
            })
            .await
    }

    /// 超时仅作用于获取锁；回调可 yield，返回第一个 Lua 值，报错或取消时立即释放锁。
    async fn with_lock(
        self,
        callback: LuaFunction,
        timeout: Timeout,
    ) -> Result<Option<LuaValue>, String> {
        let _guard = timeout
            .wait(async { self.0.acquire_owned().await.map_err(|_| "closed") })
            .await
            .map_err(str::to_owned)?;
        callback.call::<_, Option<LuaValue>>(()).await
    }
}
