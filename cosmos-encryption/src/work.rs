//! 共享计算预算与增量状态；Lua 参数只在宿主线程准备，工作线程只接收拥有型 Rust 数据。
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, Notify, Semaphore};

// 预算跨 Machine / Runtime 共享；取消后运行中的原生计算继续占用许可，直到真正结束。
static QUEUE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(super) async fn run<R, J>(prepare: impl FnOnce() -> J) -> Result<R, String>
where
    R: Send + 'static,
    J: FnOnce() -> Result<R, String> + Send + 'static,
{
    let queued = QUEUE
        .get_or_init(|| Arc::new(Semaphore::new(32)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "crypto worker queue is full")?;
    let active = ACTIVE
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "crypto workers are closed")?;
    let job = prepare();
    let task = tokio::task::spawn_blocking(move || {
        let (_queued, _active) = (queued, active);
        job()
    });
    struct Abort(tokio::task::AbortHandle);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(task.abort_handle());
    task.await
        .map_err(|err| format!("crypto worker failed: {err}"))?
}

pub(super) struct State<T>(Rc<Inner<T>>);
struct Inner<T> {
    value: Mutex<Option<T>>,
    closed: Cell<bool>,
    stopped: Notify,
}
impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Send + 'static> State<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Rc::new(Inner {
            value: Mutex::new(Some(value)),
            closed: Cell::new(false),
            stopped: Notify::new(),
        }))
    }
    pub(super) fn close(&self) {
        self.0.closed.set(true);
        self.0.stopped.notify_waiters();
        if let Ok(mut value) = self.0.value.try_lock() {
            value.take();
        }
    }
    pub(super) fn is_closed(&self) -> bool {
        self.0.closed.get()
    }
    async fn stopped(&self) {
        let notification = self.0.stopped.notified();
        if !self.is_closed() {
            notification.await;
        }
    }
    pub(super) async fn apply<R, J>(&self, prepare: impl FnOnce(T) -> J) -> Result<R, String>
    where
        R: Send + 'static,
        J: FnOnce() -> Result<(Option<T>, R), String> + Send + 'static,
    {
        let mut value = tokio::select! {
            biased;
            _ = self.stopped() => return Err("closed".into()),
            value = self.0.value.lock() => value,
        };
        let state = value.take().ok_or("closed")?;
        struct Lease<'a, T> {
            inner: &'a Inner<T>,
            armed: bool,
        }
        impl<T> Drop for Lease<'_, T> {
            fn drop(&mut self) {
                if self.armed {
                    self.inner.closed.set(true);
                    self.inner.stopped.notify_waiters();
                }
            }
        }
        let mut lease = Lease {
            inner: &self.0,
            armed: true,
        };
        let (next, output) = tokio::select! {
            biased;
            _ = self.stopped() => return Err("closed".into()),
            result = run(|| prepare(state)) => result?,
        };
        *value = next;
        if value.is_none() {
            self.close();
        }
        lease.armed = false;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn queued_cancellation_preserves_state_and_active_cancellation_releases_it() {
        let _serial = SERIAL.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            for cancel_active in [false, true] {
                let dropped = Arc::new(AtomicUsize::new(0));
                let state = State::new(Probe(dropped.clone()));
                let (started, ready) = tokio::sync::oneshot::channel();
                let (release, finish) = std::sync::mpsc::channel();
                let active = tokio::task::spawn_local({
                    let state = state.clone();
                    async move {
                        state
                            .apply(move |value| {
                                move || {
                                    started.send(()).unwrap();
                                    finish.recv_timeout(Duration::from_secs(10)).unwrap();
                                    Ok((Some(value), ()))
                                }
                            })
                            .await
                    }
                });
                tokio::time::timeout(Duration::from_secs(10), ready)
                    .await
                    .unwrap()
                    .unwrap();
                let queued = tokio::task::spawn_local({
                    let state = state.clone();
                    async move { state.apply(|value| move || Ok((Some(value), ()))).await }
                });
                tokio::task::yield_now().await;
                if cancel_active {
                    active.abort();
                    assert!(active.await.unwrap_err().is_cancelled());
                    assert!(state.is_closed());
                    assert_eq!(queued.await.unwrap().unwrap_err(), "closed");
                    // 任务已取消，但工作线程仍拥有状态，不能提前归还预算或释放在用数据。
                    assert_eq!(dropped.load(Ordering::SeqCst), 0);
                    release.send(()).unwrap();
                } else {
                    queued.abort();
                    assert!(queued.await.unwrap_err().is_cancelled());
                    assert!(!state.is_closed());
                    release.send(()).unwrap();
                    active.await.unwrap().unwrap();
                    assert!(!state.is_closed());
                    state.close();
                }
                tokio::time::timeout(Duration::from_secs(10), async {
                    while dropped.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(dropped.load(Ordering::SeqCst), 1);
            }
        });
    }
    #[test]
    fn worker_concurrency_is_bounded() {
        let _serial = SERIAL.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let mut jobs = Vec::new();
            for _ in 0..12 {
                let active = active.clone();
                let peak = peak.clone();
                jobs.push(tokio::spawn(run(move || {
                    move || {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(2));
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    }
                })));
            }
            for job in jobs {
                job.await.unwrap().unwrap();
            }
            assert!(peak.load(Ordering::SeqCst) <= 2);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        });
    }
    #[test]
    fn cancelled_running_jobs_keep_their_budget_and_queue_overflow_is_rejected() {
        use std::future::Future;
        use std::task::Poll;
        let _serial = SERIAL.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut releases = Vec::new();
            let mut callers = Vec::new();
            for _ in 0..2 {
                let (started, ready) = tokio::sync::oneshot::channel();
                let (release, finish) = std::sync::mpsc::channel();
                releases.push(release);
                callers.push(tokio::spawn(run(move || {
                    move || {
                        started.send(()).unwrap();
                        finish.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    }
                })));
                tokio::time::timeout(Duration::from_secs(10), ready)
                    .await
                    .unwrap()
                    .unwrap();
            }
            for caller in callers {
                caller.abort();
                assert!(caller.await.unwrap_err().is_cancelled());
            }
            let mut queued = Vec::new();
            for _ in 0..30 {
                let mut future = Box::pin(run(|| || Ok(())));
                std::future::poll_fn(|cx| {
                    assert!(
                        future.as_mut().poll(cx).is_pending(),
                        "cancelled job returned its budget early"
                    );
                    Poll::Ready(())
                })
                .await;
                queued.push(future);
            }
            assert_eq!(
                run(|| || Ok(())).await.unwrap_err(),
                "crypto worker queue is full"
            );
            drop(queued);
            for release in releases {
                release.send(()).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), run(|| || Ok(())))
                .await
                .unwrap()
                .unwrap();
        });
    }
}
