//! 完成结果、取消请求与等待者通知；不接触 Lua 或 Tokio 任务。

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
pub(super) struct Control {
    cancelled: Cell<bool>,
    result: RefCell<Option<Result<(), String>>>,
    driver: RefCell<Option<Waker>>,
    waiters: RefCell<Vec<Option<Waker>>>,
}

impl Control {
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }

    pub(super) fn register_driver(&self, waker: &Waker) {
        let mut driver = self.driver.borrow_mut();
        if !driver.as_ref().is_some_and(|old| old.will_wake(waker)) {
            *driver = Some(waker.clone());
        }
    }

    pub(super) fn cancel(&self) {
        if self.result.borrow().is_some() {
            return;
        }
        self.cancelled.set(true);
        let driver = self.driver.borrow_mut().take();
        if let Some(driver) = driver {
            driver.wake();
        }
    }

    pub(super) fn finish(&self, result: Result<(), String>) {
        let mut stored = self.result.borrow_mut();
        debug_assert!(stored.is_none(), "a light thread can only complete once");
        *stored = Some(result);
        drop(stored);
        let driver = self.driver.borrow_mut().take();
        drop(driver);
        // 唤醒前释放共享状态借用，允许等待者重入查询或取消其他任务。
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters.into_iter().flatten() {
            waiter.wake();
        }
    }

    pub(super) fn wait(&self) -> Wait<'_> {
        Wait {
            control: self,
            slot: None,
        }
    }
}

/// 每次等待独立注册；取消等待只移除自己的通知，不消耗任务的最终结果。
pub(super) struct Wait<'a> {
    control: &'a Control,
    slot: Option<usize>,
}

impl Future for Wait<'_> {
    type Output = Result<(), String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.control.result.borrow().as_ref() {
            return Poll::Ready(result.clone());
        }
        let mut waiters = this.control.waiters.borrow_mut();
        let slot = *this.slot.get_or_insert_with(|| {
            if let Some(slot) = waiters.iter().position(Option::is_none) {
                slot
            } else {
                waiters.push(None);
                waiters.len() - 1
            }
        });
        if !waiters[slot]
            .as_ref()
            .is_some_and(|old| old.will_wake(cx.waker()))
        {
            waiters[slot] = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        let waiter = self.slot.and_then(|slot| {
            self.control
                .waiters
                .borrow_mut()
                .get_mut(slot)
                .and_then(Option::take)
        });
        drop(waiter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::Wake;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn completion_broadcasts_and_dropped_waiters_do_not_steal_notifications() {
        let control = Control::default();
        let observer1 = Arc::new(WakeCount::default());
        let observer2 = Arc::new(WakeCount::default());
        let abandoned_observer = Arc::new(WakeCount::default());
        let waker1 = Waker::from(observer1.clone());
        let waker2 = Waker::from(observer2.clone());
        let abandoned_waker = Waker::from(abandoned_observer.clone());
        let mut wait1 = control.wait();
        let mut wait2 = control.wait();
        let mut abandoned = control.wait();
        assert!(Pin::new(&mut wait1)
            .poll(&mut Context::from_waker(&waker1))
            .is_pending());
        assert!(Pin::new(&mut wait2)
            .poll(&mut Context::from_waker(&waker2))
            .is_pending());
        assert!(Pin::new(&mut abandoned)
            .poll(&mut Context::from_waker(&abandoned_waker))
            .is_pending());
        drop(abandoned);
        control.register_driver(&waker1);
        control.cancel();
        assert_eq!(observer1.0.load(Ordering::Relaxed), 1);
        // 取消请求不等于完成，执行资源尚未释放时不能提前通知 join。
        assert_eq!(observer2.0.load(Ordering::Relaxed), 0);
        control.finish(Err("cancelled".to_string()));
        assert_eq!(observer1.0.load(Ordering::Relaxed), 2);
        assert_eq!(observer2.0.load(Ordering::Relaxed), 1);
        assert_eq!(abandoned_observer.0.load(Ordering::Relaxed), 0);
        let mut context = Context::from_waker(Waker::noop());
        for mut wait in [wait1, wait2, control.wait()] {
            assert_eq!(
                Pin::new(&mut wait).poll(&mut context),
                Poll::Ready(Err("cancelled".to_string()))
            );
        }
    }
}
