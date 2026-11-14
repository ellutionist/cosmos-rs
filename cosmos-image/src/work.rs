//! Lua 参数只在本地准备；许可由原生作业持有，取消不会提前归还计算额度。
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
static QUEUE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();
pub(super) async fn run<R: Send + 'static, J: FnOnce() -> Result<R, String> + Send + 'static>(
    prepare: impl FnOnce() -> J,
) -> Result<R, String> {
    let queued = QUEUE
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "image worker queue is full")?;
    let active = ACTIVE
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image workers are closed")?;
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
        .map_err(|err| format!("image worker failed: {err}"))?
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::{future::Future, task::Poll, time::Duration};
    #[test]
    fn cancelled_jobs_keep_native_budget_and_waiting_queue_is_bounded() {
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
                    move || {
                        ready.send(()).unwrap();
                        finish.recv_timeout(Duration::from_secs(10)).unwrap();
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
                let mut task = Box::pin(run(|| || Ok(())));
                std::future::poll_fn(|cx| {
                    assert!(
                        task.as_mut().poll(cx).is_pending(),
                        "cancelled image task returned its budget early"
                    );
                    Poll::Ready(())
                })
                .await;
                waiting.push(task);
            }
            assert_eq!(
                run(|| || Ok(())).await.unwrap_err(),
                "image worker queue is full"
            );
            drop(waiting);
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
