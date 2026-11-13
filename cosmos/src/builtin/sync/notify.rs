//! 通知：notify_one 最多暂存一次信号，notify_all 只唤醒当前等待者。

use std::cell::Cell;
use std::rc::Rc;

use tokio::sync::Notify;

use super::Timeout;

#[derive(Clone)]
pub(super) struct Notification(Rc<Inner>);

struct Inner {
    notify: Notify,
    closed: Cell<bool>,
}

#[cosmos::userdata(name = "sync.notify")]
impl Notification {
    fn new() -> Self {
        Self(Rc::new(Inner {
            notify: Notify::new(),
            closed: Cell::new(false),
        }))
    }
    async fn wait(self, timeout: Timeout) -> Result<bool, &'static str> {
        timeout
            .wait(async {
                let notified = self.0.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.0.closed.get() {
                    return Err("closed");
                }
                notified.await;
                if self.0.closed.get() {
                    Err("closed")
                } else {
                    Ok(true)
                }
            })
            .await
    }

    fn notify_one(&self) -> Result<bool, &'static str> {
        if self.0.closed.get() {
            return Err("closed");
        }
        self.0.notify.notify_one();
        Ok(true)
    }

    fn notify_all(&self) -> Result<bool, &'static str> {
        if self.0.closed.get() {
            return Err("closed");
        }
        self.0.notify.notify_waiters();
        Ok(true)
    }

    fn close(&self) {
        self.0.closed.set(true);
        self.0.notify.notify_waiters();
    }
}
