//! 公平信号量；配额由 permit 持有，显式 release 或 GC 时归还。

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore as TokioSemaphore};

use super::{Count, Timeout};

#[derive(Clone)]
pub(super) struct Semaphore(Arc<TokioSemaphore>);

#[cosmos::userdata(name = "sync.semaphore")]
impl Semaphore {
    fn new(permits: Count) -> Self {
        Self(Arc::new(TokioSemaphore::new(permits.0)))
    }

    async fn acquire(self, timeout: Timeout) -> Result<Permit, &'static str> {
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

    fn available(&self) -> i64 {
        self.0.available_permits() as i64
    }

    fn close(&self) {
        self.0.close();
    }
}

pub(super) struct Permit(Option<OwnedSemaphorePermit>);

impl Permit {
    pub(super) fn new(permit: OwnedSemaphorePermit) -> Self {
        Self(Some(permit))
    }
}

#[cosmos::userdata(name = "sync.permit")]
impl Permit {
    /// 幂等释放；不依赖持有者仍使用原来的协程或同步对象。
    fn release(&mut self) {
        self.0.take();
    }
}
