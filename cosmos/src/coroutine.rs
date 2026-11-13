use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use libc::c_int;

use crate::ffi::*;
use crate::stack;
use crate::userdata::PushLua;

/// 擦除异步操作的具体类型，在完成的当次轮询中直接压栈。
/// 输出仍是原生 Rust 值，只要求实现 PushLua，无需单独装箱返回值。
pub trait PendingOp {
    /// Pending 时不压栈；Ready 返回压入的值数，交给下一次 lua_resume。
    /// 与 PushLua 相同，转换过程中不得调用 lua_error。
    fn poll_push(self: Pin<&mut Self>, cx: &mut Context<'_>, L: *mut lua_State) -> Poll<i32>;
}

impl<F: Future> PendingOp for F
where
    F::Output: PushLua,
{
    fn poll_push(self: Pin<&mut Self>, cx: &mut Context<'_>, L: *mut lua_State) -> Poll<i32> {
        self.poll(cx).map(|value| value.push_lua(L))
    }
}

/// 协程挂起期间等待的异步操作。
/// 无 Send 约束：LuaCoroutine 本身 !Send（线程亲和），整个链路不会跨线程，
/// 放宽后 op 可以捕获 !Send 的数据（如带 Rc 的用户对象）
pub type PendingFuture = Pin<Box<dyn PendingOp>>;

// yield 通道：C 函数把 PendingFuture 塞进 TLS 槽再 lua_yield(L, 0)，
// 同一线程上的 poll 侧从槽里取回（见 yield_with / LuaCoroutine::poll）。
//
// 相比 lightuserdata + Box::into_raw 的堆指针通道，每次 yield 少一次分配，
// 代价是绑定单线程 runtime：写槽和取槽必须发生在同一条线程上。
// lua_State 本身线程亲和，LuaCoroutine 的 !Send（PhantomData<Rc<()>>）
// 在类型层面保证 future 不会跨线程迁移。
thread_local! {
    static PENDING_OP: RefCell<Option<PendingFuture>> = const { RefCell::new(None) };
}

/// C 函数侧协议：op 进槽 + yield 0 个值（结果等 resume 时再由 poll 侧压栈）。
/// lua_yield 的返回值必须作为 C 函数返回值交给 VM
/// （5.1 语义：常规 yield 路径返回 -1 即 LUA_YIELD，不 longjmp）
pub fn yield_with(L: *mut lua_State, fut: PendingFuture) -> c_int {
    PENDING_OP.with(|slot| {
        debug_assert!(slot.borrow().is_none(), "previous pending op was not taken");
        *slot.borrow_mut() = Some(fut);
    });
    unsafe { lua_yield(L, 0) }
}

/// 单次 poll 内最多恢复协程的次数，防止 Lua 侧连续 yield 饿死调度器
const RESUME_BUDGET: u32 = 256;

/// 协程 future 的两个阶段，决定本轮 resume 的 nargs 从哪来：
/// - Fresh：协程从未启动，初始参数在栈上，nargs = gettop - 1
/// - Awaiting(fut)：挂起的异步操作，完成后其结果压栈，nargs = 压栈个数
enum Phase {
    Fresh,
    Awaiting(PendingFuture),
}

/// 每个 Lua 协程绑定一个 future：
/// poll 按 Phase 推进——Fresh 直接 resume，Awaiting 先 poll 异步操作、
/// 完成后把结果压栈再 lua_resume 续跑协程，直到协程结束返回 Ready。
/// 调度完全交给 tokio。
pub struct LuaCoroutine {
    thread: *mut lua_State,
    phase: Phase,
    registry_ref: i32,
    // lua_State 线程亲和 + TLS 通道要求 future 留在 yield 发生的那条线程上：
    // Rc 使本类型 !Send，多线程 runtime 下会在编译期被 spawn 拒绝，
    // 而不是运行期取错槽
    _not_send: PhantomData<Rc<()>>,
}

impl LuaCoroutine {
    /// 把 lua_newthread 出来的协程锚定到 registry（luaL_ref 会从栈上取走它），
    /// 防止挂起期间被 GC 回收。
    /// 初始参数在 new 之前压到协程栈上（位于 chunk 函数之上），
    /// 首次 resume 会按 gettop - 1 自动传参（对应脚本里的 `...`）
    pub unsafe fn new(g: *mut lua_State, thread: *mut lua_State) -> Self {
        Self {
            thread,
            phase: Phase::Fresh,
            registry_ref: luaL_ref(g, LUA_REGISTRYINDEX),
            _not_send: PhantomData,
        }
    }
}

impl Drop for LuaCoroutine {
    fn drop(&mut self) {
        // LuaJIT 的 lua_close 只接受主线程，协程线程靠 GC 回收——摘掉锚点即可。
        // 被取消（future 被 drop）时协程正挂起，同样适用。
        // VM 已关闭则跳过：任务可能在 lua_close 之后才被 runtime 回收
        if crate::userdata::vm_alive() {
            unsafe { luaL_unref(self.thread, LUA_REGISTRYINDEX, self.registry_ref) };
        }
    }
}

impl Future for LuaCoroutine {
    type Output = Result<(), String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut budget = RESUME_BUDGET;
        loop {
            let nargs: i32 = match &mut this.phase {
                // 协程从未启动：栈上是 [chunk 函数, 初始参数...]，参数个数 = gettop - 1
                Phase::Fresh => unsafe { lua_gettop(this.thread) - 1 },
                // 关键接线：内层 future 拿到的是外层 cx 的 waker，
                // 事件（timer/epoll）到来时唤醒的就是本 future 所在的 task。
                // Pending 时 future 留在枚举里原地保存（堆上，pinning 不变）
                Phase::Awaiting(fut) => match fut.as_mut().poll_push(cx, this.thread) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(nargs) => nargs,
                },
            };
            match unsafe { lua_resume(this.thread, nargs) } {
                LUA_YIELD => {
                    // C 函数已把 pending future 塞进 TLS 槽（yield_with），栈上无值可取
                    let Some(fut) = PENDING_OP.with(|slot| slot.borrow_mut().take()) else {
                        return Poll::Ready(Err(
                            "coroutine yielded without a pending future".to_string()
                        ));
                    };
                    this.phase = Phase::Awaiting(fut);
                    if budget == 0 {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    budget -= 1;
                    // loop 回顶部：同一轮 poll 里接上新的异步操作，sleep 链不经过调度器
                }
                0 => return Poll::Ready(Ok(())),
                LUA_ERRRUN | LUA_ERRSYNTAX | LUA_ERRMEM | LUA_ERRERR => {
                    return Poll::Ready(Err(unsafe { stack::err_message(this.thread) }));
                }
                status => {
                    return Poll::Ready(Err(format!("lua_resume: unexpected status {status}")))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::userdata::{FromLua, Nil};
    use std::cell::Cell;
    use std::task::Waker;

    #[test]
    fn pending_operation_pushes_only_when_ready_and_releases_borrow() {
        unsafe {
            let L = luaL_newstate();
            assert!(!L.is_null());
            let state = Rc::new(RefCell::new(7i64));
            let owner = state.clone();
            let mut op: PendingFuture = Box::pin(async move {
                let guard = owner.borrow_mut();
                let mut pending = true;
                std::future::poll_fn(|cx| {
                    if pending {
                        pending = false;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
                (*guard, vec![0, 255, 1])
            });
            let mut cx = Context::from_waker(Waker::noop());
            assert_eq!(op.as_mut().poll_push(&mut cx, L), Poll::Pending);
            assert_eq!(lua_gettop(L), 0);
            assert!(state.try_borrow_mut().is_err());

            assert_eq!(op.as_mut().poll_push(&mut cx, L), Poll::Ready(2));
            assert!(state.try_borrow_mut().is_ok());
            assert_eq!(lua_gettop(L), 2);
            assert_eq!(i64::from_lua(L, 1).unwrap(), 7);
            assert_eq!(Vec::<u8>::from_lua(L, 2).unwrap(), [0, 255, 1]);
            drop(op);
            assert_eq!(Rc::strong_count(&state), 1);
            lua_close(L);
        }
    }

    #[test]
    fn async_return_counts_distinguish_unit_none_and_errors() {
        unsafe {
            let L = luaL_newstate();
            assert!(!L.is_null());
            let mut cx = Context::from_waker(Waker::noop());
            let mut operations: Vec<PendingFuture> = vec![
                Box::pin(async {}),
                Box::pin(async { None::<i64> }),
                Box::pin(async { Err::<(), _>("失败") }),
            ];
            for (op, expected) in operations.iter_mut().zip([0, 1, 2]) {
                lua_settop(L, 0);
                assert_eq!(op.as_mut().poll_push(&mut cx, L), Poll::Ready(expected));
                assert_eq!(lua_gettop(L), expected);
                if expected > 0 {
                    assert_eq!(lua_type(L, 1), LUA_TNIL);
                }
            }
            assert_eq!(String::from_lua(L, 2).unwrap(), "失败");
            drop(operations);
            lua_close(L);
        }
    }

    #[test]
    fn cancelling_pending_operation_drops_captured_values_without_pushing() {
        unsafe {
            let L = luaL_newstate();
            assert!(!L.is_null());
            let dropped = Rc::new(Cell::new(false));
            struct DropFlag(Rc<Cell<bool>>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.set(true);
                }
            }
            let flag = DropFlag(dropped.clone());
            let mut op: PendingFuture = Box::pin(async move {
                std::future::pending::<()>().await;
                drop(flag);
                Nil
            });
            let mut cx = Context::from_waker(Waker::noop());
            assert_eq!(op.as_mut().poll_push(&mut cx, L), Poll::Pending);
            drop(op);
            assert!(dropped.get());
            assert_eq!(lua_gettop(L), 0);
            lua_close(L);
        }
    }
}
