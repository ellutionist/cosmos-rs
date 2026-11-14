//! 统一的 Lua 轻线程：执行机制与生命周期控制分别保存在私有组成部分中。

mod control;
mod execution;

use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::ffi::lua_State;
use crate::userdata::FromLua;

use control::Control;
use execution::{Execution, Step};

pub(crate) use execution::{yield_with, PendingFuture};

const CANCELLED: &str = "thread: cancelled";
// 限制连续立即完成的异步操作，防止单次轮询独占宿主线程。
const RESUME_BUDGET: usize = 256;

/// 一个 Lua 协程的共享句柄；执行权只能取得一次，完成结果可反复等待。
/// 主脚本、回调与后台任务使用同一种执行机制，Rc 保证线程亲和性。
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<cosmos::LightThread>();
/// ```
#[derive(Clone)]
pub struct LightThread {
    inner: Rc<Inner>,
}

struct Inner {
    execution: RefCell<Option<Execution>>,
    control: Control,
}

impl LightThread {
    /// 接管已准备好的 Lua 协程，初始函数和参数已在协程栈上。
    ///
    /// # Safety
    /// main 与 lua_state 属于同一存活 VM，main 栈顶为该协程对象。
    /// 执行及其 Future 必须留在当前线程；宿主关闭 VM 前必须先释放执行资源。
    pub(crate) unsafe fn new(main: *mut lua_State, lua_state: *mut lua_State) -> Self {
        Self {
            inner: Rc::new(Inner {
                execution: RefCell::new(Some(Execution::new(main, lua_state))),
                control: Control::default(),
            }),
        }
    }

    /// 构造 Future 时就取得唯一执行权，尚未轮询即被丢弃也会取消并清理。
    /// 直接执行与后台调度共用此入口，结果读取先于 registry 锚点释放。
    pub(crate) fn run<R: for<'lua> FromLua<'lua>>(
        &self,
    ) -> impl Future<Output = Result<R, String>> {
        let execution = self.inner.execution.borrow_mut().take();
        let guard = execution.map(|execution| RunGuard {
            execution: Some(execution),
            inner: self.inner.clone(),
        });
        async move {
            let Some(mut guard) = guard else {
                return Err("light thread has already started".to_string());
            };
            let result = poll_fn(|cx| guard.poll(cx))
                .await
                .and_then(|()| guard.execution.as_ref().unwrap().read_result::<R>());
            guard.finish(result.as_ref().map(|_| ()).map_err(Clone::clone));
            result
        }
    }

    pub(crate) fn spawn(&self) {
        let run = self.run::<()>();
        drop(tokio::task::spawn_local(async move {
            if let Err(error) = run.await {
                if error != CANCELLED {
                    eprintln!("cosmos.thread: {error}");
                }
            }
        }));
    }

    pub(crate) async fn completion(&self) -> Result<(), String> {
        self.inner.control.wait().await
    }

    pub(crate) fn cancel(&self) {
        self.inner.control.cancel();
    }

    /// 等待任一任务；所有等待只观察结果，未选中的任务仍可再次等待。
    pub(crate) async fn wait_any(threads: &[Self]) -> (usize, Result<(), String>) {
        assert!(!threads.is_empty(), "at least one light thread is required");
        let mut waits: Vec<_> = threads
            .iter()
            .map(|thread| thread.inner.control.wait())
            .collect();
        poll_fn(|cx| {
            for (index, wait) in waits.iter_mut().enumerate() {
                if let Poll::Ready(result) = Pin::new(wait).poll(cx) {
                    return Poll::Ready((index + 1, result));
                }
            }
            Poll::Pending
        })
        .await
    }
}

/// 执行期间独占状态机，不跨 lua_resume 持有共享状态的 RefCell 借用。
struct RunGuard {
    execution: Option<Execution>,
    inner: Rc<Inner>,
}

impl RunGuard {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), String>> {
        self.inner.control.register_driver(cx.waker());
        for _ in 0..RESUME_BUDGET {
            if self.inner.control.is_cancelled() {
                return Poll::Ready(Err(CANCELLED.to_string()));
            }
            match self.execution.as_mut().unwrap().poll_step(cx) {
                Poll::Ready(Ok(Step::Yielded)) => continue,
                result => {
                    return if self.inner.control.is_cancelled() {
                        Poll::Ready(Err(CANCELLED.to_string()))
                    } else {
                        result.map(|result| result.map(|_| ()))
                    };
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn finish(&mut self, result: Result<(), String>) {
        // 先释放挂起操作中的 userdata 借用与协程锚点，再通知等待者。
        drop(self.execution.take());
        self.inner.control.finish(result);
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.execution.is_some() {
            self.finish(Err(CANCELLED.to_string()));
        }
    }
}

#[cfg(test)]
mod tests;
