//! 文件与目录共享的串行阻塞 I/O 调度，不把 Lua 引用送到工作线程。
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::task::AbortHandle;

use super::io_error;

pub(super) struct Handle<T>(Arc<Inner<T>>);
struct Inner<T> {
    value: Mutex<Option<T>>,
    gate: Arc<Semaphore>,
    closed: AtomicBool,
    stopped: Notify,
    timeout_ns: AtomicU64,
}
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Inner<T> {
    fn stop(&self) {
        self.closed.store(true, Ordering::Release);
        self.gate.close();
        self.stopped.notify_waiters();
    }
}
struct Operation<T> {
    inner: Arc<Inner<T>>,
    task: AbortHandle,
    armed: bool,
}
impl<T> Drop for Operation<T> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.stop();
            self.task.abort();
            if let Ok(mut value) = self.inner.value.try_lock() {
                value.take();
            }
        }
    }
}
impl<T: Send + 'static> Handle<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Arc::new(Inner {
            value: Mutex::new(Some(value)),
            gate: Arc::new(Semaphore::new(1)),
            closed: AtomicBool::new(false),
            stopped: Notify::new(),
            timeout_ns: AtomicU64::new(0),
        }))
    }
    pub(super) fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
    pub(super) fn settimeout(&self, ms: f64) -> Result<bool, String> {
        if !ms.is_finite() || !(0.0..=1e12).contains(&ms) {
            return Err("invalid timeout (ms)".into());
        }
        self.0.timeout_ns.store(
            Duration::from_secs_f64(ms / 1000.0).as_nanos() as u64,
            Ordering::Release,
        );
        Ok(true)
    }
    pub(super) fn timeout(&self) -> Option<Duration> {
        let ns = self.0.timeout_ns.load(Ordering::Acquire);
        (ns != 0).then(|| Duration::from_nanos(ns))
    }
    async fn limited<R>(
        &self,
        future: impl Future<Output = Result<R, String>>,
    ) -> Result<R, String> {
        match self.timeout() {
            None => future.await,
            Some(duration) => tokio::time::timeout(duration, future)
                .await
                .map_err(|_| "timeout".to_string())?,
        }
    }

    pub(super) async fn run<R, F>(&self, operation: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut T) -> io::Result<R> + Send + 'static,
    {
        self.limited(async {
            let stopped = self.0.stopped.notified();
            tokio::pin!(stopped);
            stopped.as_mut().enable();
            if self.is_closed() {
                return Err("closed".into());
            }
            // 等锁不占用阻塞线程；取消排队操作不改变文件状态。
            let permit = self
                .0
                .gate
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "closed".to_string())?;
            let inner = self.0.clone();
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut value = inner
                    .value
                    .lock()
                    .map_err(|_| io::Error::other("I/O state lock poisoned"))?;
                if inner.closed.load(Ordering::Acquire) {
                    value.take();
                    return Err(io::Error::other("closed"));
                }
                let result = operation(value.as_mut().ok_or_else(|| io::Error::other("closed"))?);
                if inner.closed.load(Ordering::Acquire) {
                    value.take();
                }
                result
            });
            let mut guard = Operation {
                inner: self.0.clone(),
                task: task.abort_handle(),
                armed: true,
            };
            let result = tokio::select! {
                biased;
                _ = &mut stopped => return Err("closed".into()),
                result = task => result.map_err(|error| format!("I/O task failed: {error}"))?,
            };
            guard.armed = false;
            result.map_err(io_error)
        })
        .await
    }
    pub(super) async fn close(&self) -> Result<bool, String> {
        self.0.stop();
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            inner
                .value
                .lock()
                .map_err(|_| "I/O state lock poisoned".to_string())?
                .take();
            Ok(true)
        })
        .await
        .map_err(|error| format!("I/O close task failed: {error}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::Handle;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[tokio::test]
    async fn cancelling_started_io_closes_handle_wakes_waiters_and_drops_resource() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let handle = Handle::new(Probe(dropped.clone()));
        let (release, receiver) = std::sync::mpsc::channel();
        let active = {
            let handle = handle.clone();
            let started = started.clone();
            tokio::spawn(async move {
                handle
                    .run(move |_| {
                        started.store(true, Ordering::SeqCst);
                        receiver.recv().unwrap();
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let waiter = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.run(|_| Ok(())).await })
        };
        tokio::task::yield_now().await;
        active.abort();
        assert!(active.await.unwrap_err().is_cancelled());
        assert!(handle.is_closed());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            "closed"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(handle.close().await.unwrap());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelling_queued_io_preserves_handle_and_does_not_run_operation() {
        let handle = Handle::new(0usize);
        let started = Arc::new(AtomicBool::new(false));
        let executed = Arc::new(AtomicBool::new(false));
        let (release, receiver) = std::sync::mpsc::channel();
        let active = {
            let handle = handle.clone();
            let started = started.clone();
            tokio::spawn(async move {
                handle
                    .run(move |value| {
                        started.store(true, Ordering::SeqCst);
                        receiver.recv().unwrap();
                        *value += 1;
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let queued = {
            let handle = handle.clone();
            let executed = executed.clone();
            tokio::spawn(async move {
                handle
                    .run(move |_| {
                        executed.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        queued.abort();
        let _ = queued.await;
        assert!(!handle.is_closed());
        release.send(()).unwrap();
        active.await.unwrap().unwrap();
        assert_eq!(handle.run(|value| Ok(*value)).await.unwrap(), 1);
        assert!(!executed.load(Ordering::SeqCst));
        handle.close().await.unwrap();
    }
}
