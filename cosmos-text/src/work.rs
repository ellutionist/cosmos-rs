//! 许可一直由原生作业持有；取消仅在搜索/转换步之间协作检查。
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use tokio::sync::Semaphore;
static QUEUE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();
pub(super) fn check(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("text operation cancelled".into())
    } else {
        Ok(())
    }
}
pub(super) async fn run<
    R: Send + 'static,
    J: FnOnce(&AtomicBool) -> Result<R, String> + Send + 'static,
>(
    prepare: impl FnOnce() -> J,
) -> Result<R, String> {
    let queued = QUEUE
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "text worker queue is full")?;
    let active = ACTIVE
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "text workers are closed")?;
    let job = prepare();
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let task = tokio::task::spawn_blocking(move || {
        let (_queued, _active) = (queued, active);
        check(&flag)?;
        job(&flag)
    });
    struct Abort(Arc<AtomicBool>, tokio::task::AbortHandle);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
            self.1.abort();
        }
    }
    let _abort = Abort(cancelled, task.abort_handle());
    task.await
        .map_err(|err| format!("text worker failed: {err}"))?
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::{future::Future, task::Poll, time::Duration};
    #[test]
    fn cancelled_native_jobs_keep_budget_and_queue_admission_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut releases = Vec::new();
            let mut jobs = Vec::new();
            for _ in 0..2 {
                let (ready, started) = tokio::sync::oneshot::channel();
                let (release, finish) = std::sync::mpsc::channel();
                releases.push(release);
                jobs.push(tokio::spawn(run(move || {
                    move |cancel| {
                        ready.send(()).unwrap();
                        finish.recv_timeout(Duration::from_secs(10)).unwrap();
                        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
                        Ok(())
                    }
                })));
                tokio::time::timeout(Duration::from_secs(10), started)
                    .await
                    .unwrap()
                    .unwrap();
            }
            for job in jobs {
                job.abort();
                assert!(job.await.unwrap_err().is_cancelled());
            }
            let mut waiting = Vec::new();
            for _ in 0..14 {
                let mut task = Box::pin(run(|| |_| Ok(())));
                std::future::poll_fn(|cx| {
                    assert!(
                        task.as_mut().poll(cx).is_pending(),
                        "cancelled text job released its native budget early"
                    );
                    Poll::Ready(())
                })
                .await;
                waiting.push(task);
            }
            assert_eq!(
                run(|| |_| Ok(())).await.unwrap_err(),
                "text worker queue is full"
            );
            drop(waiting);
            for release in releases {
                release.send(()).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), run(|| |_| Ok(())))
                .await
                .unwrap()
                .unwrap();
        });
    }
}
