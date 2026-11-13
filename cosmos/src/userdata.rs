//! Rust 自定义类型 → Lua userdata 桥接。
//!
//! 类型注册走 #[cosmos::userdata] 属性宏：宏为 impl 块里的每个方法生成
//! extern "C" trampoline（参数转换、panic 捕获、返回值压栈）并生成
//! UserData impl（见 cosmos-macros crate）。
//!
//! 运行时侧：
//! - push::<T> 把 T 包进 RefCell 后 placement-write 进 lua_newuserdata 的槽
//!   （Lua GC 托管，不额外堆分配），metatable 在首次 push 时懒构建
//! - metatable 兼任类型表：__index = 自身（静态构造 + 实例方法共用一张表）、
//!   __gc = drop_in_place 统一生成，经 package.preload 用 require 引入
//!   （local Counter = require("cosmos.Counter")）
//! - 类型校验用 luaL_checkudata（registry 按 type_name 找 metatable 后比对）
//! - 方法借 &mut/& 走 RefCell::try_borrow：Lua 边界上编译期独占性降级为
//!   运行期检查，借用冲突（重入/跨协程交错）转成 lua_error 而非 aliasing UB
//!
//! 线程模型：全部约束在单线程（同 coroutine.rs 的 TLS 通道）；
//! 方法 trampoline 在 lua_resume 的保护帧内执行，lua_error 长跳安全。

use std::any::TypeId;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::ffi::CString;
use std::fmt::Display;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use crate::coroutine::AsyncOutput;
use crate::ffi::*;

/// 宿主/扩展注册入口需要的状态类型（避免功能模块直接依赖 crate::ffi）
pub use crate::ffi::lua_State;

/// #[cosmos::userdata] 宏为每个类型生成该 impl
pub trait UserData: 'static {
    fn type_name() -> &'static str {
        std::any::type_name::<Self>()
    }
    /// Lua 侧 require 引入用的类型名（local Counter = require("cosmos.Counter")），
    /// 宏默认生成类型短名（路径最后一段）
    fn lua_name() -> &'static str {
        std::any::type_name::<Self>()
    }
    fn add_methods(_methods: &mut MethodTable<Self>)
    where
        Self: Sized,
    {
    }
}

pub struct MethodTable<T> {
    // CStr 而非 &str：lua_setfield 按 C 字符串读，&str 字面量不保证 NUL 结尾
    entries: Vec<(&'static std::ffi::CStr, lua_CFunction)>,
    _marker: PhantomData<fn(T)>,
}

impl<T> MethodTable<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub fn add(&mut self, name: &'static std::ffi::CStr, f: lua_CFunction) {
        self.entries.push((name, f));
    }
}

// 已注册类型缓存，键是主线程状态。Lua 5.1 的协程 L 拿不到主状态指针
// （LuaJIT 也没有 lua_mainthread），所以 init 时把主状态以 lightuserdata
// 锚定在 registry（registry 对所有协程可见），之后经 main_state 取回
thread_local! {
    static REGISTERED: RefCell<HashMap<*mut lua_State, HashSet<TypeId>>> =
        RefCell::new(HashMap::new());
}

const MAIN_STATE_KEY: &std::ffi::CStr = c"__cosmos_main_state";

/// VM 是否还活着：lua_close 之后挂着未 drop 的锚定对象（LuaFunction /
/// 挂起的 LuaCoroutine）会因为任务延后回收而在已释放的状态上 luaL_unref。
/// Drop 里查这个标志，已关闭则直接放弃释放（进程即将退出，泄漏无害）
static VM_ALIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn vm_alive() -> bool {
    VM_ALIVE.load(Ordering::Relaxed)
}

/// 状态初始化：把主状态指针锚定进 registry，必须早于任何 push 调用
pub unsafe fn init(L: *mut lua_State) {
    lua_pushlightuserdata(L, L as *mut c_void);
    lua_setfield(L, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
    VM_ALIVE.store(true, Ordering::Relaxed);
}

pub unsafe fn main_state(L: *mut lua_State) -> *mut lua_State {
    lua_getfield(L, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
    let p = lua_touserdata(L, -1);
    lua_settop(L, -2);
    if p.is_null() {
        L // 未 init 时退化为调用方状态（主状态直接调用 push 的场景）
    } else {
        p as *mut lua_State
    }
}

// 每个类型的 CString 名字只构建一次（leak 进 'static，类型总数有限）
fn type_cstring<T: 'static>() -> &'static CString {
    thread_local! {
        static NAMES: RefCell<HashMap<TypeId, &'static CString>> = RefCell::new(HashMap::new());
    }
    NAMES.with(|names| {
        *names
            .borrow_mut()
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                Box::leak(Box::new(CString::new(std::any::type_name::<T>()).unwrap()))
            })
    })
}

fn ensure_metatable<T: UserData>(main: *mut lua_State, L: *mut lua_State) {
    let registered = REGISTERED.with(|m| {
        m.borrow()
            .get(&main)
            .map(|s| s.contains(&TypeId::of::<T>()))
            .unwrap_or(false)
    });
    if registered {
        return;
    }
    unsafe {
        let name = type_cstring::<T>();
        luaL_newmetatable(L, name.as_ptr()); // registry[type_name] = mt，同时压栈

        // 静态构造和实例方法共用这一张表：__index 指向自己，
        // obj:method() → mt.method，Counter.new() → 命名空间字段直接索引 mt
        lua_pushvalue(L, -1);
        lua_setfield(L, -2, c"__index".as_ptr());

        let mut methods = MethodTable::new();
        T::add_methods(&mut methods);
        for (mname, f) in methods.entries {
            lua_pushcclosure(L, f, 0);
            lua_setfield(L, -2, mname.as_ptr());
        }

        // __gc 统一生成：drop_in_place 对无 Drop 的类型也是合法空操作，
        // 宏不需要探测 Drop 的存在
        lua_pushcclosure(L, gc_trampoline::<T>, 0);
        lua_setfield(L, -2, c"__gc".as_ptr());

        // 类型表经 require 引入：package.preload["cosmos.Counter"] = 工厂函数。
        // require 约定 preload 条目必须是函数（返回非函数值会被静默丢弃，
        // 见 LuaJIT lib_package.c lj_cf_package_require），工厂返回 mt 自身
        let preload_key = format!("cosmos.{}", T::lua_name());
        lua_getfield(L, LUA_GLOBALSINDEX, c"package".as_ptr()); // [mt, package]
        lua_getfield(L, -1, c"preload".as_ptr()); // [mt, package, preload]
        lua_pushlstring(L, preload_key.as_ptr() as *const c_char, preload_key.len());
        lua_pushvalue(L, -4); // [mt, package, preload, key, mt]
        lua_pushcclosure(L, preload_factory, 1); // mt 副本收为 upvalue → [.., key, factory]
        lua_settable(L, -3); // preload[key] = factory
        lua_settop(L, -3); // 栈恢复

        REGISTERED.with(|m| {
            m.borrow_mut().entry(main).or_default().insert(TypeId::of::<T>());
        });
    }
}

/// 提前注册类型（构建 metatable + 挂 require 入口），
/// 让脚本可以在任何实例产生之前 require("cosmos.Counter")。
/// 不调用也能工作：首次 push 时会懒注册，只是 require 会晚于第一个实例
pub fn register<T: UserData>(L: *mut lua_State) {
    unsafe {
        let main = main_state(L);
        ensure_metatable::<T>(main, L);
    }
}

/// 构造 userdata 并把 T 压上 Lua 栈（栈顶 +1 个值）。
/// L 可以是协程线程，metatable 注册统一记在主状态名下。
///
/// 值包在 RefCell 里存进 userdata：方法拿 &mut/& 时走 try_borrow 运行期检查，
/// 借用冲突（重入/跨协程交错）变成 lua_error 而不是 aliasing UB——
/// Rust 的编译期独占性在 Lua 边界降级为运行期检查
pub fn push<T: UserData>(L: *mut lua_State, value: T) {
    unsafe {
        let main = main_state(L);
        ensure_metatable::<T>(main, L);
        // LuaJIT userdata 对齐保证是 8 字节，更高对齐的类型直接断言拒绝
        debug_assert!(std::mem::align_of::<RefCell<T>>() <= 8);
        let p = lua_newuserdata(L, std::mem::size_of::<RefCell<T>>()) as *mut RefCell<T>;
        ptr::write(p, RefCell::new(value));
        // 5.1 的 luaL_getmetatable 是宏（registry 上的 lua_getfield），直接用 lua_getfield
        lua_getfield(L, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());
        lua_setmetatable(L, -2);
    }
}

/// 把 C 函数包成带一个 userdata upvalue 的闭包（压栈 1 个值）。
/// 用于"读端返回迭代器闭包"这类需要把 Rust 状态挂成 upvalue 的场景。
pub unsafe fn push_closure<T: UserData>(L: *mut lua_State, func: lua_CFunction, upvalue: T) {
    push::<T>(L, upvalue);
    lua_pushcclosure(L, func, 1);
}

/// 方法 trampoline 里取 &mut T（经 RefCell::try_borrow_mut）。
/// luaL_checkudata 校验 metatable，不符直接 lua_error；借用冲突同样 lua_error。
///
/// 返回 RefMut<'static, T> 的 'static 由 transmute 延展：RefCell 以 placement-write
/// 常驻 Lua GC 内存、地址稳定且调用期间被栈锚定不会回收，活期覆盖任何使用场景，
/// 因此把 try_borrow 借出的短生命周期延长为 'static 是安全的。
/// 独占性由 RefCell 的 borrow 标志在运行期保证，不再依赖 &'static mut 的类型承诺
pub unsafe fn check<T: UserData>(L: *mut lua_State, idx: c_int) -> RefMut<'static, T> {
    let p = luaL_checkudata(L, idx, type_cstring::<T>().as_ptr()) as *mut RefCell<T>;
    match (*p).try_borrow_mut() {
        Ok(r) => std::mem::transmute(r),
        Err(_) => raise(L, "attempt to mutably borrow userdata that is already borrowed"),
    }
}

pub unsafe fn check_shared<T: UserData>(L: *mut lua_State, idx: c_int) -> Ref<'static, T> {
    let p = luaL_checkudata(L, idx, type_cstring::<T>().as_ptr()) as *mut RefCell<T>;
    match (*p).try_borrow() {
        Ok(r) => std::mem::transmute(r),
        Err(_) => raise(L, "attempt to borrow userdata that is mutably borrowed"),
    }
}

unsafe extern "C" fn gc_trampoline<T: UserData>(L: *mut lua_State) -> c_int {
    let p = lua_touserdata(L, 1) as *mut RefCell<T>;
    if !p.is_null() {
        // Drop 里 panic 不能穿过 extern "C" 帧，吞掉
        let _ = catch_unwind(AssertUnwindSafe(|| ptr::drop_in_place(p)));
    }
    0
}

/// require 的 preload 工厂：upvalue 1 就是 metatable，直接返回。
/// require 会用返回值填充 package.loaded[name]
unsafe extern "C" fn preload_factory(L: *mut lua_State) -> c_int {
    lua_pushvalue(L, LUA_GLOBALSINDEX - 1); // lua_upvalueindex(1)
    1
}

/// 方法 trampoline 外壳：捕获 panic、把 Err 转成 lua_error。
/// 只允许在 lua_resume 的保护帧内调用（即从 Lua 方法路径进入），
/// 否则 lua_error 没有 setjmp 目标会 PANIC
pub fn trampoline<F>(L: *mut lua_State, f: F) -> c_int
where
    F: FnOnce(*mut lua_State) -> Result<i32, String>,
{
    match catch_unwind(AssertUnwindSafe(|| f(L))) {
        Ok(Ok(n)) => n,
        Ok(Err(msg)) => unsafe { raise(L, &msg) },
        Err(p) => {
            let detail = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            unsafe { raise(L, &format!("rust panic: {detail}")) }
        }
    }
}

pub unsafe fn raise(L: *mut lua_State, msg: &str) -> ! {
    lua_pushlstring(L, msg.as_ptr() as *const c_char, msg.len());
    lua_error(L); // longjmp，不返回
    unreachable!()
}

/// lua_close 前调用，清掉该状态的注册缓存（否则线程本地 map 会残留死键）
pub fn cleanup(L: *mut lua_State) {
    unsafe {
        let main = main_state(L);
        REGISTERED.with(|m| {
            m.borrow_mut().remove(&main);
        });
    }
    VM_ALIVE.store(false, Ordering::Relaxed);
}

/// 方法参数：从 Lua 栈取值
pub trait FromLua: Sized {
    /// # Safety
    /// L 是当前执行中的状态，idx 在有效栈范围内
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String>;
}

impl FromLua for f64 {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tonumber(L, idx))
    }
}

impl FromLua for i64 {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tointeger(L, idx) as i64)
    }
}

impl FromLua for u16 {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        let v = lua_tointeger(L, idx);
        if v < 0 || v > u16::MAX as lua_Integer {
            return Err(format!("argument {idx}: number out of range for u16"));
        }
        Ok(v as u16)
    }
}

impl FromLua for bool {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TBOOLEAN {
            return Err(format!("argument {idx}: expected boolean"));
        }
        Ok(lua_toboolean(L, idx) != 0)
    }
}

/// 原始字节（二进制安全）：Lua 字符串不做 UTF-8 校验
impl FromLua for Vec<u8> {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(L, idx, &mut len);
        Ok(std::slice::from_raw_parts(p as *const u8, len).to_vec())
    }
}

/// Lua table（string → string）↔ Rust 有序键值表。
/// 非 string 的键/值跳过；遍历只发生在桥接层。
#[derive(Clone, Default)]
pub struct LuaStrMap(pub Vec<(String, String)>);

impl FromLua for LuaStrMap {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TTABLE {
            return Err(format!("argument {idx}: expected table"));
        }
        let mut out = Vec::new();
        lua_pushnil(L);
        while lua_next(L, idx) != 0 {
            if lua_type(L, -2) == LUA_TSTRING && lua_type(L, -1) == LUA_TSTRING {
                let key = <String as FromLua>::from_lua(L, -2)?;
                let value = <String as FromLua>::from_lua(L, -1)?;
                out.push((key, value));
            }
            lua_settop(L, -2); // 弹掉 value，保留 key 供下一次 lua_next
        }
        Ok(LuaStrMap(out))
    }
}

impl PushLua for LuaStrMap {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe {
            lua_createtable(L, 0, self.0.len() as c_int);
            for (k, v) in self.0 {
                k.push_lua(L);
                v.push_lua(L);
                lua_settable(L, -3);
            }
        }
        1
    }
}

impl FromLua for String {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(L, idx, &mut len);
        Ok(String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned())
    }
}

/// 零拷贝借用：同步方法专用。借用 Lua 字符串的数据，不复制、不分配。
/// 只能用于同步方法（方法体在 lua_resume 内执行完，字符串被栈锚定不会被 GC）；
/// 异步方法不得使用——future 跨 yield 后栈帧已丢弃，借用会悬垂（宏会拒绝）。
/// 异步方法要用字符串请用 LuaStr（按长度拷贝或锚定）
impl<'a> FromLua for &'a str {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(L, idx, &mut len);
        let bytes = std::slice::from_raw_parts(p as *const u8, len);
        std::str::from_utf8(bytes).map_err(|_| format!("argument {idx}: invalid UTF-8 string"))
    }
}

/// 长度阈值：超过则走 registry 锚定而非拷贝。
/// 拷贝成本 ≈ malloc(10~50ns) + memcpy(~0.1ns/字节)，锚定成本 ≈ 10 次表操作
/// （300~800ns 固定）；二者在约 4KB 处交叉，短串拷贝更省
pub const STR_ANCHOR_THRESHOLD: usize = 4096;

/// 异步方法的字符串参数：≤阈值拷贝为 Owned，>阈值 registry 锚定借用（Borrowed）。
/// 同步方法请直接用 &str（零拷贝且无需锚定）。
/// 实现 Deref<Target=str>，方法体里当 &str 用（as_bytes/len/format 都行）。
///
/// 为什么异步方法不能裸借实参 &str（必须拷贝或锚定）：
/// 协程挂起时 lua_yield 把栈 top 设回 base，实参所在的 C 函数帧槽位（base 之上）
/// 被丢弃；若该串没有其它强引用，就成为不可达对象被 GC 回收，捕获的 &str 随之
/// 悬垂（deref 即 UB）。强制 GC 实测：同样位于被丢弃槽位上的临时对象，无锚定时
/// GC 后 collected=true、8MB 串 freed≈8MB；luaL_ref 锚定后 collected=false、
/// freed=0——锚定使串成为 registry 里的 GC 根，数据在整个借用期内有效。
///
/// 注意这是"可能悬垂、而非必悬垂"：若串在 Lua 侧仍有残留强引用（如赋给了局部
/// 变量、或留在顶层 chunk 帧的临时寄存器），GC 不会回收它。正因是否回收取决于
/// 调用点写法、无法依赖，才必须保守地拷贝或锚定。
///
/// （附注：Lua 规定字符串不从弱表移除，别用弱值表观测字符串回收；用内存计数，
/// 或对 table/userdata 等显式构造对象做弱表观测。）
pub enum LuaStr {
    Owned(String),
    Borrowed(StrAnchor),
}

/// registry 锚定守卫：持有一个 luaL_ref 引用，drop 时 luaL_unref。
/// 被锚定的字符串是 GC 根，其数据指针在守卫存活期间一直有效，
/// 因此 s 的 'static 生命周期是 sound 的（transmute 延展）
pub struct StrAnchor {
    s: &'static str,
    L: *mut lua_State,
    ref_: c_int,
}

impl Drop for StrAnchor {
    fn drop(&mut self) {
        unsafe { luaL_unref(self.L, LUA_REGISTRYINDEX, self.ref_) };
    }
}

impl std::ops::Deref for LuaStr {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            LuaStr::Owned(s) => s,
            LuaStr::Borrowed(a) => a.s,
        }
    }
}

impl FromLua for LuaStr {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(L, idx, &mut len);
        let bytes = std::slice::from_raw_parts(p as *const u8, len);
        if len <= STR_ANCHOR_THRESHOLD {
            Ok(LuaStr::Owned(
                String::from_utf8_lossy(bytes).into_owned(),
            ))
        } else {
            // 先校验 UTF-8（此时字符串还在栈上、锚定前不会被 GC），再锚定
            let s = std::str::from_utf8(bytes)
                .map_err(|_| format!("argument {idx}: invalid UTF-8 string"))?;
            lua_pushvalue(L, idx);
            let ref_ = luaL_ref(L, LUA_REGISTRYINDEX);
            Ok(LuaStr::Borrowed(StrAnchor {
                s: std::mem::transmute::<&str, &'static str>(s),
                L,
                ref_,
            }))
        }
    }
}

/// 可选参数：实参缺省（栈上无此槽位）或显式 nil 时取 None。
/// 宏不需要特判——Lua 调用少传参数时 lua_type 天然返回 LUA_TNONE。
impl<T: FromLua> FromLua for Option<T> {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        match lua_type(L, idx) {
            LUA_TNONE | LUA_TNIL => Ok(None),
            _ => Ok(Some(T::from_lua(L, idx)?)),
        }
    }
}

/// Lua 函数引用：参数需要在调用返回后继续存活（如 HTTP handler）时使用。
/// 锚定在 registry（luaL_ref），可跨 yield、跨协程使用；Drop 时解除锚定。
/// 借用 VM 的单线程约束与 lua_State 相同。
pub struct LuaFunction {
    main: *mut lua_State,
    registry_ref: c_int,
}

impl LuaFunction {
    /// 把函数压到 target 栈顶（target 可以是同一 VM 里的任意协程）
    pub unsafe fn push_to(&self, target: *mut lua_State) {
        lua_rawgeti(target, LUA_REGISTRYINDEX, self.registry_ref);
    }

    /// 在 main 上新建协程并调用 self(args)，返回可 await 的协程。
    /// args 走 PushLua（元组即多参数）：栈布局 [函数, args...]，
    /// 这是 handler 类回调的统一入口，功能模块不再自己碰 lua_newthread。
    pub unsafe fn call_in_new_thread<A: PushLua>(&self, args: A) -> crate::coroutine::LuaCoroutine {
        let thread = lua_newthread(self.main); // 线程对象压到 main 栈上
        self.push_to(thread);
        args.push_lua(thread);
        // LuaCoroutine::new 里 luaL_ref(main) 会把线程对象从 main 栈上取走
        crate::coroutine::LuaCoroutine::new(self.main, thread)
    }

    pub fn registry_ref(&self) -> c_int {
        self.registry_ref
    }

    /// 创建时所属的主状态（跨协程调用时 registry 操作的稳妥锚点）
    pub fn main(&self) -> *mut lua_State {
        self.main
    }
}

impl FromLua for LuaFunction {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TFUNCTION {
            return Err(format!("argument {idx}: expected function"));
        }
        let main = main_state(L);
        lua_pushvalue(L, idx);
        let registry_ref = luaL_ref(L, LUA_REGISTRYINDEX);
        Ok(LuaFunction { main, registry_ref })
    }
}

impl Drop for LuaFunction {
    fn drop(&mut self) {
        if vm_alive() {
            unsafe { luaL_unref(self.main, LUA_REGISTRYINDEX, self.registry_ref) };
        }
    }
}

/// 统一的压栈协议：把 Rust 值压上 Lua 栈，返回压入个数。
///
/// 两条路径共用这一个协议：
/// - 同步方法返回：trampoline 内直接压栈，userdata 走 blanket impl 零装箱
/// - 异步 op 输出：边界类型 AsyncOutput（coroutine.rs）也实现本 trait，
///   poll loop 用同一入口压栈，各变体委托回本文件的标量 impl——
///   lua_pushxxx 全代码库只出现一份
///
/// 安全约定：压栈在 lua_resume 之间也合法；实现里不得调用 lua_error
/// （那需要 lua_resume 的保护帧，错误应返回 Err 由 trampoline 转换）
pub trait PushLua {
    fn push_lua(self, L: *mut lua_State) -> i32;
}

impl PushLua for () {
    fn push_lua(self, _L: *mut lua_State) -> i32 {
        0
    }
}
impl PushLua for i64 {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushinteger(L, self as lua_Integer) };
        1
    }
}
impl PushLua for u16 {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushinteger(L, self as lua_Integer) };
        1
    }
}
impl PushLua for f64 {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushnumber(L, self) };
        1
    }
}
impl PushLua for bool {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushboolean(L, self as c_int) };
        1
    }
}
impl PushLua for String {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(L, self.as_ptr() as *const c_char, self.len()) };
        1
    }
}
/// 原始字节 → Lua 字符串（二进制安全，不做 UTF-8 校验）
impl PushLua for Vec<u8> {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(L, self.as_ptr() as *const c_char, self.len()) };
        1
    }
}
/// Lua nil 作为"一个值"的标记：多返回值里占位用
/// （`()` 是 0 个返回值，二者语义不同）
pub struct Nil;

impl PushLua for Nil {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushnil(L) };
        1
    }
}
impl<T: PushLua> PushLua for Option<T> {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        match self {
            Some(v) => v.push_lua(L),
            // None 压一个 nil：作为表字段时与 setfield 配对，
            // 作为多返回值时 `local v = f()` 同样是 nil
            None => {
                unsafe { lua_pushnil(L) };
                1
            }
        }
    }
}
/// userdata 返回：直接构造压栈，零装箱（不走 AsyncOutput::Custom）
impl<T: UserData> PushLua for T {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        push::<T>(L, self);
        1
    }
}

/// 多返回值：2 元组依次压栈（split 这类"一次返回多个 userdata"的同步方法用）
impl<A: PushLua, B: PushLua> PushLua for (A, B) {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        let (a, b) = self;
        a.push_lua(L) + b.push_lua(L)
    }
}

/// 字符串切片：避免为 &'static str 常量额外分配 String
impl PushLua for &str {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(L, self.as_ptr() as *const c_char, self.len()) };
        1
    }
}

/// Result<T, E> 统一走 nil + 错误串（标准 Lua 形态）：
/// Ok(v) 压 v，Err(e) 压 nil + e.to_string()（两个返回值）。
/// 同步方法经 PushLua、异步方法经 IntoOutput，都命中这一份语义
impl<T: PushLua, E: Display> PushLua for Result<T, E> {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        match self {
            Ok(v) => v.push_lua(L),
            Err(e) => {
                let msg = e.to_string();
                unsafe {
                    lua_pushnil(L);
                    lua_pushlstring(L, msg.as_ptr() as *const c_char, msg.len());
                }
                2
            }
        }
    }
}

/// 值 → 异步边界类型 AsyncOutput：async 方法/op 的返回值要跨 yield 边界，
/// 必须收敛进枚举（标量零分配，开放值域走 Custom 闭包）。
///
/// 与 PushLua 的分工：PushLua 是"值 → 直接压栈"（同步方法，trampoline 内），
/// IntoOutput 是"值 → 边界类型"（异步方法/op，future 完成后由 poll loop 压栈）
pub trait IntoOutput {
    fn into_output(self) -> AsyncOutput;
}

impl IntoOutput for () {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Nil
    }
}
impl IntoOutput for i64 {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Integer(self)
    }
}
impl IntoOutput for f64 {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Number(self)
    }
}
impl IntoOutput for bool {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Boolean(self)
    }
}
impl IntoOutput for String {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::String(self)
    }
}
impl<T: IntoOutput> IntoOutput for Option<T> {
    fn into_output(self) -> AsyncOutput {
        match self {
            Some(v) => v.into_output(),
            None => AsyncOutput::Nil,
        }
    }
}

/// 元组的异步返回：语义与同步的 tuple PushLua 一致（按序压栈、值为返回值个数）。
/// 异步方法的值要跨 yield 边界，只能先装箱进 AsyncOutput::Custom，
/// 由 poll loop 统一压栈——桥接层承担这次装箱，功能层只组合 PushLua 原语
impl<A: PushLua + 'static, B: PushLua + 'static> IntoOutput for (A, B) {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Custom(Box::new(move |L| {
            let (a, b) = self;
            a.push_lua(L) + b.push_lua(L)
        }))
    }
}
/// 返回 userdata 类型本身：装箱进 Custom 闭包（开放值域跨边界，闭包兼做存放+压栈）。
/// 孤儿规则保证下游无法为 String 等已实现 IntoOutput 的类型再实现 UserData，
/// 不会与本 blanket impl 冲突
impl<T: UserData> IntoOutput for T {
    fn into_output(self) -> AsyncOutput {
        AsyncOutput::Custom(Box::new(move |L| {
            push::<T>(L, self);
            1
        }))
    }
}

/// Result<T, E> 的异步形态：Ok 走 IntoOutput，Err 走 NilErr（nil + 错误串）。
/// 与 PushLua 的 Result impl 语义一致，同步/异步统一
impl<T: IntoOutput, E: Display> IntoOutput for Result<T, E> {
    fn into_output(self) -> AsyncOutput {
        match self {
            Ok(v) => v.into_output(),
            Err(e) => AsyncOutput::NilErr(e.to_string()),
        }
    }
}

#[cfg(test)]
mod derive_tests {
    use super::*;
    use crate::ffi::*;

    fn with_state(f: impl FnOnce(*mut lua_State)) {
        unsafe {
            let L = luaL_newstate();
            assert!(!L.is_null(), "luaL_newstate failed");
            f(L);
            lua_close(L);
        }
    }

    /// 建一张整数/字符串字段的表（栈顶 +1）
    unsafe fn push_int_table(L: *mut lua_State, pairs: &[(&str, lua_Integer)]) {
        lua_createtable(L, 0, pairs.len() as c_int);
        for (k, v) in pairs {
            lua_pushlstring(L, k.as_ptr() as *const c_char, k.len());
            lua_pushinteger(L, *v);
            lua_settable(L, -3);
        }
    }

    fn seven() -> i64 {
        7
    }

    #[derive(cosmos::FromLua, Debug, PartialEq)]
    struct FieldDefaults {
        #[lua(default = 1)]
        a: i64,
        #[lua(default)]
        b: Option<i64>,
        #[lua(default_fn = seven)]
        d: i64,
        c: i64,
    }

    #[test]
    fn field_level_defaults() {
        with_state(|L| unsafe {
            push_int_table(L, &[("c", 3)]);
            let v = FieldDefaults::from_lua(L, 1).expect("read table");
            assert_eq!(
                v,
                FieldDefaults {
                    a: 1,
                    b: None,
                    d: 7,
                    c: 3
                }
            );
        });
    }

    #[test]
    fn all_fields_present() {
        with_state(|L| unsafe {
            push_int_table(L, &[("a", 10), ("b", 20), ("c", 30), ("d", 40)]);
            let v = FieldDefaults::from_lua(L, 1).expect("read table");
            assert_eq!(
                v,
                FieldDefaults {
                    a: 10,
                    b: Some(20),
                    d: 40,
                    c: 30
                }
            );
        });
    }

    #[test]
    fn missing_field_without_default_is_error() {
        with_state(|L| unsafe {
            push_int_table(L, &[("a", 1)]);
            let err = FieldDefaults::from_lua(L, 1).unwrap_err();
            assert!(err.contains("missing field `c`"), "unexpected error: {err}");
        });
    }

    #[derive(cosmos::FromLua, Debug, PartialEq)]
    #[lua(default)]
    struct ContainerDefaults {
        a: i64,
        b: i64,
    }

    impl Default for ContainerDefaults {
        fn default() -> Self {
            Self { a: -1, b: -2 }
        }
    }

    #[test]
    fn container_default_uses_default_impl() {
        with_state(|L| unsafe {
            push_int_table(L, &[("b", 5)]);
            let v = ContainerDefaults::from_lua(L, 1).expect("read table");
            assert_eq!(v, ContainerDefaults { a: -1, b: 5 });
        });
    }

    #[derive(cosmos::FromLua, Debug, PartialEq)]
    #[lua(default_fn = build_defaults)]
    struct FnDefaults {
        a: i64,
        b: i64,
    }

    fn build_defaults() -> FnDefaults {
        FnDefaults { a: 11, b: 22 }
    }

    #[test]
    fn container_default_fn() {
        with_state(|L| unsafe {
            push_int_table(L, &[("b", 5)]);
            let v = FnDefaults::from_lua(L, 1).expect("read table");
            assert_eq!(v, FnDefaults { a: 11, b: 5 });
        });
    }

    #[test]
    fn non_table_is_error() {
        with_state(|L| unsafe {
            lua_pushinteger(L, 1);
            let err = ContainerDefaults::from_lua(L, 1).unwrap_err();
            assert!(err.contains("expected table"), "unexpected error: {err}");
        });
    }

    #[derive(cosmos::PushLua)]
    struct Pushed {
        a: i64,
        b: Option<i64>,
    }

    #[test]
    fn push_lua_generates_table() {
        with_state(|L| unsafe {
            Pushed { a: 42, b: None }.push_lua(L);
            assert_eq!(lua_type(L, -1), LUA_TTABLE);

            lua_getfield(L, -1, c"a".as_ptr());
            assert_eq!(lua_tointeger(L, -1), 42);
            lua_settop(L, -2);

            lua_getfield(L, -1, c"b".as_ptr());
            assert_eq!(lua_type(L, -1), LUA_TNIL);
        });
    }
}
