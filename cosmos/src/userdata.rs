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
//! - 类型校验比较 userdata 的 metatable，类型或借用失败只返回 Err
//! - 方法借 &mut/& 走 RefCell::try_borrow：Lua 边界上编译期独占性降级为
//!   运行期检查，借用冲突（重入/跨协程交错）转成 lua_error 而非 aliasing UB
//!
//! 线程模型：全部约束在单线程（同 coroutine.rs 的 TLS 通道）；
//! 参数转换与借用失败只返回 Err，最外层 trampoline 在 Rust 局部值释放后才调用 lua_error。

use std::any::TypeId;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt::Display;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ffi::*;

/// #[cosmos::userdata] 宏为每个类型生成该 impl
pub trait UserData: 'static {
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
        lua_settop(L, -4); // 弹出 mt、package、preload，恢复入口栈高度

        REGISTERED.with(|m| {
            m.borrow_mut()
                .entry(main)
                .or_default()
                .insert(TypeId::of::<T>());
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

/// 校验 userdata 的类型，不抛出 Lua 错误；调用方可以先正常释放 Rust 局部值。
unsafe fn userdata_ptr<T: UserData>(
    L: *mut lua_State,
    idx: c_int,
) -> Result<*mut RefCell<T>, String> {
    let top = lua_gettop(L);
    let matches = if lua_type(L, idx) == LUA_TUSERDATA && lua_getmetatable(L, idx) != 0 {
        lua_getfield(L, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());
        lua_rawequal(L, -1, -2) != 0
    } else {
        false
    };
    lua_settop(L, top);
    if !matches {
        return Err(format!(
            "argument {idx}: expected {} userdata",
            T::lua_name()
        ));
    }
    Ok(lua_touserdata(L, idx) as *mut RefCell<T>)
}

/// 守卫的地址由 Lua userdata 保持稳定；生命周期由调用栈或协程的 registry 锚点保证。
/// 借用跨 await 时留在同一线程，类型或借用冲突均返回 Err，由最外层 trampoline 处理。
pub unsafe fn try_check<T: UserData>(
    L: *mut lua_State,
    idx: c_int,
) -> Result<RefMut<'static, T>, String> {
    let p = userdata_ptr::<T>(L, idx)?;
    let guard = (*p)
        .try_borrow_mut()
        .map_err(|_| "attempt to mutably borrow userdata that is already borrowed".to_string())?;
    Ok(std::mem::transmute::<RefMut<'_, T>, RefMut<'static, T>>(
        guard,
    ))
}

/// 共享借用与独占借用共用类型校验及错误协议。
/// 守卫借用 Lua 托管的内存，只能用于受调用栈/协程锚点保护的调用期间，不能作为回调的拥有型结果。
pub unsafe fn try_check_shared<T: UserData>(
    L: *mut lua_State,
    idx: c_int,
) -> Result<Ref<'static, T>, String> {
    let p = userdata_ptr::<T>(L, idx)?;
    let guard = (*p)
        .try_borrow()
        .map_err(|_| "attempt to borrow userdata that is mutably borrowed".to_string())?;
    Ok(std::mem::transmute::<Ref<'_, T>, Ref<'static, T>>(guard))
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
    let error = match catch_unwind(AssertUnwindSafe(|| f(L))) {
        Ok(Ok(n)) => return n,
        Ok(Err(msg)) => msg,
        Err(p) => {
            let detail = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            format!("rust panic: {detail}")
        }
    };
    unsafe {
        lua_pushlstring(L, error.as_ptr() as *const c_char, error.len());
    }
    // lua_error 使用 longjmp，不会运行 Rust 析构；错误字符串也要在跳转前释放。
    drop(error);
    unsafe { lua_error(L) }
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
    /// L 属于仍存活的 VM；idx 是有效栈索引，或用于读取缺省参数/返回值的空槽。
    /// 转换失败必须返回 Err，不得调用 lua_error；成功或失败都应恢复入口栈高度。
    /// 返回值需自行保活所依赖的 Lua 引用，不能返回仅靠当前调用栈保护的借用。
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
        let idx = crate::stack::abs_index(L, idx);
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
            Ok(LuaStr::Owned(String::from_utf8_lossy(bytes).into_owned()))
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
    /// 在独立协程中执行回调，参数和结果均通过桥接 trait 转换。
    /// 协程锚点覆盖整个等待与结果读取过程，取消时随 Future 一起回收。
    /// 宿主必须先回收任务，再关闭所属 VM。
    pub async fn call<A: PushLua, R: FromLua>(&self, args: A) -> Result<R, String> {
        let (thread, mut co) = unsafe {
            let thread = lua_newthread(self.main);
            lua_rawgeti(thread, LUA_REGISTRYINDEX, self.registry_ref);
            args.push_lua(thread);
            (
                thread,
                crate::coroutine::LuaCoroutine::new(self.main, thread),
            )
        };
        (&mut co).await?;
        let result = unsafe { R::from_lua(thread, 1) };
        drop(co);
        result
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
/// - 异步 op 输出：PendingOp 在 Future 完成的当次轮询中调用本 trait，
///   返回值无需单独装箱，同步与异步复用同一组转换
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

impl FromLua for Nil {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        match lua_type(L, idx) {
            LUA_TNONE | LUA_TNIL => Ok(Nil),
            _ => Err(format!("argument {idx}: expected nil")),
        }
    }
}

/// 无返回值的回调忽略 Lua 返回内容，供 WebSocket handler 等场景使用。
impl FromLua for () {
    unsafe fn from_lua(_L: *mut lua_State, _idx: c_int) -> Result<Self, String> {
        Ok(())
    }
}

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
/// userdata 返回：直接构造压栈，无需为返回值单独装箱
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
/// 同步与异步方法共用这一份语义
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

#[cfg(test)]
mod derive_tests {
    use super::*;
    use crate::ffi::*;

    fn with_state(f: impl FnOnce(*mut lua_State)) {
        unsafe {
            let L = luaL_newstate();
            assert!(!L.is_null(), "luaL_newstate failed");
            luaL_openlibs(L);
            init(L);
            f(L);
            cleanup(L);
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

    #[derive(cosmos::PushLua)]
    enum Outcome<T: PushLua> {
        Empty,
        Data(T),
        Closed {
            L: Nil,
            reason: &'static str,
            partial: T,
        },
    }

    #[test]
    fn enum_push_preserves_count_order_and_binary_data() {
        with_state(|L| unsafe {
            assert_eq!(Outcome::<Vec<u8>>::Empty.push_lua(L), 0);
            assert_eq!(lua_gettop(L), 0);

            assert_eq!(Outcome::Data(vec![0, 255, 1]).push_lua(L), 1);
            assert_eq!(Vec::<u8>::from_lua(L, 1).unwrap(), [0, 255, 1]);
            lua_settop(L, 0);

            let outcome = Outcome::Closed {
                L: Nil,
                reason: "closed",
                partial: vec![255, 0],
            };
            assert_eq!(outcome.push_lua(L), 3);
            assert_eq!(lua_gettop(L), 3);
            assert_eq!(lua_type(L, 1), LUA_TNIL);
            assert_eq!(String::from_lua(L, 2).unwrap(), "closed");
            assert_eq!(Vec::<u8>::from_lua(L, 3).unwrap(), [255, 0]);
        });
    }

    #[derive(cosmos::FromLua)]
    enum CallbackReply<T: FromLua> {
        #[lua(type = "table")]
        Table(T),
        #[lua(type = "string")]
        Body(Vec<u8>),
        #[lua(type = "nil")]
        Empty(Nil, Option<String>),
        #[lua(type = "number")]
        Number { value: i64 },
        #[lua(type = "none")]
        Nothing,
    }

    #[test]
    fn enum_from_lua_reads_typed_values_and_multiple_slots() {
        with_state(|L| unsafe {
            type Reply = CallbackReply<ContainerDefaults>;
            assert!(matches!(Reply::from_lua(L, 1).unwrap(), Reply::Nothing));
            lua_pushinteger(L, 42);
            assert!(matches!(
                Reply::from_lua(L, -1).unwrap(),
                Reply::Number { value: 42 }
            ));

            push_int_table(L, &[("a", 7), ("b", 9)]);
            match Reply::from_lua(L, -1).unwrap() {
                Reply::Table(value) => assert_eq!(value, ContainerDefaults { a: 7, b: 9 }),
                _ => panic!("返回值分支错误"),
            }
            assert_eq!(lua_gettop(L), 2);
            lua_settop(L, 0);

            vec![0u8, 255, 1].push_lua(L);
            match Reply::from_lua(L, 1).unwrap() {
                Reply::Body(bytes) => assert_eq!(bytes, [0, 255, 1]),
                _ => panic!("返回值分支错误"),
            }
            lua_settop(L, 0);
            Nil.push_lua(L);
            "callback error".push_lua(L);
            match Reply::from_lua(L, -2).unwrap() {
                Reply::Empty(_, Some(error)) => assert_eq!(error, "callback error"),
                _ => panic!("返回值分支错误"),
            }
            assert_eq!(lua_gettop(L), 2);
        });
    }

    #[test]
    fn failed_nested_conversion_restores_stack_and_allows_retry() {
        with_state(|L| unsafe {
            lua_createtable(L, 0, 1);
            "bad".push_lua(L);
            lua_setfield(L, -2, c"a".as_ptr());
            assert!(CallbackReply::<ContainerDefaults>::from_lua(L, -1).is_err());
            assert_eq!(lua_gettop(L), 1);

            10i64.push_lua(L);
            lua_setfield(L, -2, c"a".as_ptr());
            let value = ContainerDefaults::from_lua(L, -1).unwrap();
            assert_eq!(value.a, 10);
            assert_eq!(lua_gettop(L), 1);
        });
    }

    #[test]
    fn userdata_conversion_reports_type_and_borrow_errors_without_longjmp() {
        struct Value;
        impl UserData for Value {}
        struct Other;
        impl UserData for Other {}

        with_state(|L| unsafe {
            push(L, Value);
            let guard = try_check::<Value>(L, -1).unwrap();
            assert!(try_check_shared::<Value>(L, 1).is_err());
            assert!(try_check::<Value>(L, 1).is_err());
            assert_eq!(lua_gettop(L), 1);
            drop(guard);
            assert!(try_check::<Value>(L, 1).is_ok());

            push(L, Other);
            assert!(try_check::<Value>(L, -1).is_err());
            assert!(try_check_shared::<Value>(L, -1).is_err());
            assert_eq!(lua_gettop(L), 2);
        });
    }

    #[test]
    fn typed_callback_keeps_arguments_and_results_alive_across_yield() {
        with_state(|L| unsafe {
            crate::libs::open_cosmos(L);
            assert_eq!(
                luaL_loadstring(
                    L,
                    c"return function(value) cosmos.sleep(1); return nil, value end".as_ptr()
                ),
                0
            );
            assert_eq!(lua_pcall(L, 0, 1, 0), 0);
            let callback = LuaFunction::from_lua(L, -1).unwrap();
            lua_settop(L, 0);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            let result = local.block_on(
                &runtime,
                callback.call::<_, CallbackReply<ContainerDefaults>>("yielded"),
            );
            match result.unwrap() {
                CallbackReply::Empty(_, Some(value)) => assert_eq!(value, "yielded"),
                _ => panic!("返回值分支错误"),
            }
            assert_eq!(lua_gettop(L), 0);
            drop(local);
        });
    }
}
