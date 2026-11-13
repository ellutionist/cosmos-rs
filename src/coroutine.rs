use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use libc::{c_char, c_int};

use crate::ffi::*;
use crate::stack;

/// 异步操作的输出：完成后压入 Lua 栈的值。
/// 用值枚举替代 Box<dyn IntoLua>：输出收敛到 Lua 值域这个闭集，
/// 完成时从 future 里 move 出来，零分配；
/// 多返回值将来加 Multi 变体（SmallVec，≤4 个仍零分配）
#[allow(dead_code)] // 变体随 op 库逐步启用，目前只有 Nil 被构造
pub enum Output {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    String(String),
}

impl Output {
    /// 把值压入 Lua 栈，返回压入个数
    pub fn into_lua(self, L: *mut lua_State) -> i32 {
        match self {
            Output::Nil => 0,
            Output::Boolean(b) => {
                unsafe { lua_pushboolean(L, b as c_int) };
                1
            }
            Output::Number(n) => {
                unsafe { lua_pushnumber(L, n) };
                1
            }
            Output::Integer(i) => {
                unsafe { lua_pushinteger(L, i as lua_Integer) };
                1
            }
            Output::String(s) => {
                unsafe { lua_pushlstring(L, s.as_ptr() as *const c_char, s.len()) };
                1
            }
        }
    }
}

/// 协程挂起期间等待的异步操作
pub type PendingFuture = Pin<Box<dyn Future<Output = Output> + Send>>;

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
        // 被取消（future 被 drop）时协程正挂起，同样适用
        unsafe { luaL_unref(self.thread, LUA_REGISTRYINDEX, self.registry_ref) };
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
                Phase::Awaiting(fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(out) => out.into_lua(this.thread),
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
