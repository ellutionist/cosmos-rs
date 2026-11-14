//! Rust 自定义类型 → Lua userdata 桥接。
//!
//! 类型注册走 `#[cosmos::userdata]` 属性宏：宏生成 Rust LuaCallable 适配与 UserData impl。
//! C ABI trampoline、panic 捕获和挂起协议保留在 cosmos 内部；
//! 外部模块只通过 Value/Output/Call 组合转换，不接触 Lua 状态指针或栈操作。
//!
//! 运行时侧：
//! - `push::<T>` 把 T 包进 RefCell 后 placement-write 进 lua_newuserdata 的槽
//!   （Lua GC 托管，不额外堆分配），metatable 在首次 push 时懒构建
//! - metatable 兼任类型表：__index = 自身（静态构造 + 实例方法共用一张表）、
//!   __gc = drop_in_place 统一生成，经 package.preload 用 require 引入
//!   （如 local Channel = require("cosmos.sync.channel")）
//! - 声明路径按点分段，自动创建父模块；类型表同时是父模块字段和 require 返回值
//! - 类型校验比较 userdata 的 metatable，类型或借用失败只返回 Err
//! - 方法借 &mut/& 走 RefCell::try_borrow：Lua 边界上编译期独占性降级为
//!   运行期检查，借用冲突（重入/跨协程交错）转成 lua_error 而非 aliasing UB
//!
//! 线程模型：全部约束在单线程（同 light_thread/execution.rs 的 TLS 通道）；
//! 参数转换与借用失败只返回 Err，最外层 trampoline 在 Rust 局部值释放后才调用 lua_error。

use std::any::TypeId;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt::Display;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use crate::ffi::*;

mod call;
mod context;

pub use call::{Call, LuaCallable, LuaClosure};
pub use context::{Output, TableIdentity, TableReader, TableWriter, Value, ValueKind};

/// `#[cosmos::userdata]` 宏为每个类型生成该 impl
pub trait UserData: 'static {
    /// Lua 侧 require 引入用的相对路径（如 sync.channel），
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

impl<T> Default for MethodTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> MethodTable<T> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub fn add<C: LuaCallable>(&mut self, name: &'static std::ffi::CStr) {
        self.entries.push((name, call::invoke::<C>));
    }
}

// 已注册类型缓存，键是主线程状态。Lua 5.1 的协程 lua_state 拿不到主状态指针
// （LuaJIT 也没有 lua_mainthread），所以 init 时把主状态以 lightuserdata
// 锚定在 registry（registry 对所有协程可见），之后经 main_state 取回
thread_local! {
    static REGISTERED: RefCell<HashMap<*mut lua_State, VmRegistration>> =
        RefCell::new(HashMap::new());
}

const MAIN_STATE_KEY: &std::ffi::CStr = c"__cosmos_main_state";

struct VmRegistration {
    types: HashSet<TypeId>,
    alive: Rc<Cell<bool>>,
}

impl Default for VmRegistration {
    fn default() -> Self {
        Self {
            types: HashSet::new(),
            alive: Rc::new(Cell::new(true)),
        }
    }
}

/// registry 引用统一使用所属 VM 的主状态释放，并持有该次 VM 生命周期的独立标记。
/// 即使后来新 VM 复用了相同地址，旧引用也不会访问新 VM 或已经释放的状态。
pub(crate) struct RegistryRef {
    main: *mut lua_State,
    value: c_int,
    alive: Rc<Cell<bool>>,
}

impl RegistryRef {
    /// 消费栈顶值并锚定；lua_state 必须属于初始化后的存活 VM。
    pub(crate) unsafe fn new(lua_state: *mut lua_State) -> Self {
        let main = main_state(lua_state);
        let alive =
            REGISTERED.with(|states| states.borrow_mut().entry(main).or_default().alive.clone());
        Self {
            main,
            value: luaL_ref(lua_state, LUA_REGISTRYINDEX),
            alive,
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.get()
    }

    unsafe fn push_to(&self, lua_state: *mut lua_State) {
        assert!(self.is_alive(), "Lua VM is closed");
        debug_assert_eq!(
            main_state(lua_state),
            self.main,
            "Lua reference does not belong to the target VM"
        );
        if self.value == LUA_REFNIL {
            lua_pushnil(lua_state);
        } else {
            lua_rawgeti(lua_state, LUA_REGISTRYINDEX, self.value);
        }
    }
}

impl Drop for RegistryRef {
    fn drop(&mut self) {
        if self.is_alive() {
            unsafe { luaL_unref(self.main, LUA_REGISTRYINDEX, self.value) };
        }
    }
}

/// 状态初始化：把主状态指针锚定进 registry，必须早于任何 push 调用
///
/// # Safety
/// lua_state 必须是当前线程上的有效主状态，且所属 VM 在本次初始化期间保持存活。
pub(crate) unsafe fn init(lua_state: *mut lua_State) {
    lua_pushlightuserdata(lua_state, lua_state);
    lua_setfield(lua_state, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
    REGISTERED.with(|states| {
        states.borrow_mut().entry(lua_state).or_default();
    });
}

/// 返回所属 VM 的主状态；未初始化时使用传入状态。
///
/// # Safety
/// lua_state 必须指向当前线程上的有效 Lua 状态，其 registry 可访问。
unsafe fn main_state(lua_state: *mut lua_State) -> *mut lua_State {
    lua_getfield(lua_state, LUA_REGISTRYINDEX, MAIN_STATE_KEY.as_ptr());
    let p = lua_touserdata(lua_state, -1);
    lua_settop(lua_state, -2);
    if p.is_null() {
        lua_state // 未 init 时退化为调用方状态（主状态直接调用 push 的场景）
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

unsafe fn ensure_metatable<T: UserData>(main: *mut lua_State, lua_state: *mut lua_State) {
    let registered = REGISTERED.with(|m| {
        m.borrow()
            .get(&main)
            .map(|state| state.types.contains(&TypeId::of::<T>()))
            .unwrap_or(false)
    });
    if registered {
        return;
    }
    unsafe {
        let top = lua_gettop(lua_state);
        // 命名空间先于类型注册时，直接将已有表补成 metatable，保留子模块和 require 的身份。
        push_module(lua_state, &format!("cosmos.{}", T::lua_name()));
        lua_pushvalue(lua_state, -1);
        lua_setfield(lua_state, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());

        // 静态构造和实例方法共用这一张表：__index 指向自己，
        // obj:method() → mt.method，类型表上的静态方法直接索引 mt
        lua_pushvalue(lua_state, -1);
        lua_setfield(lua_state, -2, c"__index".as_ptr());

        let mut methods = MethodTable::new();
        T::add_methods(&mut methods);
        for (mname, f) in methods.entries {
            lua_pushcclosure(lua_state, f, 0);
            lua_setfield(lua_state, -2, mname.as_ptr());
        }

        // __gc 统一生成：drop_in_place 对无 Drop 的类型也是合法空操作，
        // 宏不需要探测 Drop 的存在
        lua_pushcclosure(lua_state, gc_trampoline::<T>, 0);
        lua_setfield(lua_state, -2, c"__gc".as_ptr());

        lua_settop(lua_state, top);

        REGISTERED.with(|m| {
            m.borrow_mut()
                .entry(main)
                .or_default()
                .types
                .insert(TypeId::of::<T>());
        });
    }
}

/// 提前注册类型（构建 metatable + 挂 require 入口），
/// 让脚本可以在任何实例产生之前通过声明路径 require 对应类型表。
/// 不调用也能工作：首次 push 时会懒注册，只是 require 会晚于第一个实例
///
/// # Safety
/// lua_state 必须属于已初始化、已打开标准库的存活 VM，并由当前线程独占操作。
pub(crate) unsafe fn register<T: UserData>(lua_state: *mut lua_State) {
    let main = main_state(lua_state);
    ensure_metatable::<T>(main, lua_state);
}

/// 按点分路径创建/复用模块表，各级表同时挂到父表和 require 入口，栈顶增加叶子表。
/// 后续注册同一路径的 userdata 时直接补全该表，不替换已有模块或子模块。
///
/// # Safety
/// lua_state 满足 register 的契约，栈可容纳模块表及注册临时值。
/// path 必须由非空、无 NUL 的字段组成；已有路径节点必须是模块表。
pub(crate) unsafe fn push_module(lua_state: *mut lua_State, path: &str) {
    let top = lua_gettop(lua_state);
    lua_pushvalue(lua_state, LUA_GLOBALSINDEX);
    let mut prefix = String::new();
    for part in path.split('.') {
        assert!(!part.is_empty(), "module path must not be empty: {path}");
        let key = CString::new(part).expect("module path must not contain NUL");
        lua_getfield(lua_state, -1, key.as_ptr());
        if lua_type(lua_state, -1) == LUA_TNIL {
            lua_settop(lua_state, -2);
            lua_createtable(lua_state, 0, 0);
            lua_pushvalue(lua_state, -1);
            lua_setfield(lua_state, -3, key.as_ptr());
        }
        assert_eq!(
            lua_type(lua_state, -1),
            LUA_TTABLE,
            "module path is not a table: {path}"
        );
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(part);

        let module = lua_gettop(lua_state);
        lua_getfield(lua_state, LUA_GLOBALSINDEX, c"package".as_ptr());
        lua_getfield(lua_state, -1, c"preload".as_ptr());
        lua_pushlstring(lua_state, prefix.as_ptr() as *const c_char, prefix.len());
        lua_pushvalue(lua_state, module);
        lua_pushcclosure(lua_state, preload_factory, 1);
        lua_settable(lua_state, -3);
        lua_settop(lua_state, module);
        // 只保留当前节点，栈占用不随路径深度增加。
        lua_replace(lua_state, top + 1);
    }
}

/// 构造 userdata 并把 T 压上 Lua 栈（栈顶 +1 个值）。
/// lua_state 可以是协程线程，metatable 注册统一记在主状态名下。
///
/// 值包在 RefCell 里存进 userdata：方法拿 &mut/& 时走 try_borrow 运行期检查，
/// 借用冲突（重入/跨协程交错）变成 lua_error 而不是 aliasing UB——
/// Rust 的编译期独占性在 Lua 边界降级为运行期检查
///
/// # Safety
/// lua_state 必须属于已初始化的存活 VM，栈有足够空间，RefCell<T> 的对齐不得超过 8 字节。
pub(crate) unsafe fn push<T: UserData>(lua_state: *mut lua_State, value: T) {
    unsafe {
        let main = main_state(lua_state);
        ensure_metatable::<T>(main, lua_state);
        // LuaJIT userdata 对齐保证是 8 字节，更高对齐的类型直接断言拒绝
        debug_assert!(std::mem::align_of::<RefCell<T>>() <= 8);
        let p = lua_newuserdata(lua_state, std::mem::size_of::<RefCell<T>>()) as *mut RefCell<T>;
        ptr::write(p, RefCell::new(value));
        // 5.1 的 luaL_getmetatable 是宏（registry 上的 lua_getfield），直接用 lua_getfield
        lua_getfield(lua_state, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());
        lua_setmetatable(lua_state, -2);
    }
}

/// 校验 userdata 的类型，不抛出 Lua 错误；调用方可以先正常释放 Rust 局部值。
unsafe fn userdata_ptr<T: UserData>(
    lua_state: *mut lua_State,
    idx: c_int,
) -> Result<*mut RefCell<T>, String> {
    let top = lua_gettop(lua_state);
    let matches =
        if lua_type(lua_state, idx) == LUA_TUSERDATA && lua_getmetatable(lua_state, idx) != 0 {
            lua_getfield(lua_state, LUA_REGISTRYINDEX, type_cstring::<T>().as_ptr());
            lua_rawequal(lua_state, -1, -2) != 0
        } else {
            false
        };
    lua_settop(lua_state, top);
    if !matches {
        return Err(format!(
            "argument {idx}: expected {} userdata",
            T::lua_name()
        ));
    }
    Ok(lua_touserdata(lua_state, idx) as *mut RefCell<T>)
}

/// 守卫的地址由 Lua userdata 保持稳定；生命周期由调用栈或协程的 registry 锚点保证。
/// 借用跨 await 时留在同一线程，类型或借用冲突均返回 Err，由最外层 trampoline 处理。
///
/// # Safety
/// lua_state 和 idx 必须有效；返回守卫使用期间，对应 userdata 必须由调用栈或 registry 锚点保活，
/// 且 VM 不得关闭。守卫不得跨线程移动。
pub(crate) unsafe fn try_check<T: UserData>(
    lua_state: *mut lua_State,
    idx: c_int,
) -> Result<RefMut<'static, T>, String> {
    let p = userdata_ptr::<T>(lua_state, idx)?;
    let guard = (*p)
        .try_borrow_mut()
        .map_err(|_| "attempt to mutably borrow userdata that is already borrowed".to_string())?;
    Ok(std::mem::transmute::<RefMut<'_, T>, RefMut<'static, T>>(
        guard,
    ))
}

/// 共享借用与独占借用共用类型校验及错误协议。
/// 守卫借用 Lua 托管的内存，只能用于受调用栈/协程锚点保护的调用期间，不能作为回调的拥有型结果。
///
/// # Safety
/// lua_state 和 idx 必须有效；返回守卫使用期间，对应 userdata 必须保持锚定且 VM 仍存活，
/// 守卫不得跨线程移动。
pub(crate) unsafe fn try_check_shared<T: UserData>(
    lua_state: *mut lua_State,
    idx: c_int,
) -> Result<Ref<'static, T>, String> {
    let p = userdata_ptr::<T>(lua_state, idx)?;
    let guard = (*p)
        .try_borrow()
        .map_err(|_| "attempt to borrow userdata that is mutably borrowed".to_string())?;
    Ok(std::mem::transmute::<Ref<'_, T>, Ref<'static, T>>(guard))
}

unsafe extern "C" fn gc_trampoline<T: UserData>(lua_state: *mut lua_State) -> c_int {
    let p = lua_touserdata(lua_state, 1) as *mut RefCell<T>;
    if !p.is_null() {
        // Drop 里 panic 不能穿过 extern "C" 帧，吞掉
        let _ = catch_unwind(AssertUnwindSafe(|| ptr::drop_in_place(p)));
    }
    0
}

/// require 的 preload 工厂：upvalue 1 是模块/类型表，直接返回。
/// require 会用返回值填充 package.loaded[name]
unsafe extern "C" fn preload_factory(lua_state: *mut lua_State) -> c_int {
    lua_pushvalue(lua_state, LUA_GLOBALSINDEX - 1); // lua_upvalueindex(1)
    1
}

/// 方法 trampoline 外壳：捕获 panic、把 Err 转成 lua_error。
/// 只允许在 lua_resume 的保护帧内调用（即从 Lua 方法路径进入），
/// 否则 lua_error 没有 setjmp 目标会 PANIC
///
/// # Safety
/// lua_state 必须是当前 Lua C 回调的有效状态，且回调位于 lua_resume 或 lua_pcall 的保护帧内。
/// f 不得自行调用 lua_error；错误必须通过 Result 返回，以先释放 Rust 局部资源。
pub(crate) unsafe fn trampoline<F>(lua_state: *mut lua_State, f: F) -> c_int
where
    F: FnOnce(*mut lua_State) -> Result<i32, String>,
{
    let error = match catch_unwind(AssertUnwindSafe(|| f(lua_state))) {
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
        lua_pushlstring(lua_state, error.as_ptr() as *const c_char, error.len());
    }
    // lua_error 使用 longjmp，不会运行 Rust 析构；错误字符串也要在跳转前释放。
    drop(error);
    unsafe { lua_error(lua_state) }
}

/// lua_close 前调用，清掉该状态的注册缓存（否则线程本地 map 会残留死键）
///
/// # Safety
/// lua_state 所属 VM 必须仍存活；全部本地任务已回收，后续只允许关闭该 VM。
pub(crate) unsafe fn cleanup(lua_state: *mut lua_State) {
    unsafe {
        let main = main_state(lua_state);
        REGISTERED.with(|m| {
            if let Some(state) = m.borrow_mut().remove(&main) {
                state.alive.set(false);
            }
        });
    }
}

/// builtin 文件句柄的只读桥接视图；供文件上传/响应等插件消费，不暴露原始描述符。
/// 与原句柄共享生命周期与串行调度；跨取消的语义沿用 builtin FS。
#[derive(Clone)]
pub struct FileReader {
    file: crate::builtin::fs::file::File,
}
impl FileReader {
    /// 逻辑关闭原文件；进行中的系统调用完成后回收资源，不等待它完成。
    pub fn close(&self) {
        self.file.abort();
    }
    /// 返回普通文件的长度与可用修改时间。
    pub async fn info(&self) -> Result<(u64, Option<std::time::SystemTime>), String> {
        self.file.stream_info().await
    }
    /// 按偏移读取，最多 16 MiB，不改变应用可见的文件游标。
    pub async fn read_at(&self, offset: u64, size: usize) -> Result<Vec<u8>, String> {
        self.file.read_at(offset, size).await
    }
}
impl<'lua> FromLua<'lua> for FileReader {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        Ok(Self {
            file: value.read::<crate::builtin::fs::file::File>()?,
        })
    }
}

/// 方法参数：从 Lua 栈取值
pub trait FromLua<'lua>: Sized {
    /// # Safety
    /// value 所属 VM 在转换期间保持存活。转换失败返回 Err，不得执行 Lua、挂起或抛出 Lua 错误。
    /// 同步字符串借用只能用于当前调用；异步参数和跨调用结果必须拥有数据或 registry 锚点。
    /// userdata 借用守卫不能作为拥有型返回值，转换成功或失败均不得改变入口栈高度。
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String>;
}

/// 可克隆的 userdata 作为拥有型参数/接收者：只在转换时短暂借用原实例。
/// 类型自身决定克隆语义；共享句柄可由 Rc/Arc 保活，允许多个协程同时等待。
impl<'lua, T: UserData + Clone> FromLua<'lua> for T {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        Ok(try_check_shared::<T>(lua_state, idx)?.clone())
    }
}

impl<'lua> FromLua<'lua> for f64 {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tonumber(lua_state, idx))
    }
}

/// 毫秒数转换为拥有型时长，时间模块与同步原语共用参数校验。
impl<'lua> FromLua<'lua> for Duration {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        let millis = f64::from_lua(Value::new(lua_state, idx))?;
        if !millis.is_finite() || !(0.0..=1e12).contains(&millis) {
            return Err(format!("argument {idx}: invalid duration (ms)"));
        }
        Ok(Duration::from_secs_f64(millis / 1000.0))
    }
}

impl<'lua> FromLua<'lua> for i64 {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        Ok(lua_tointeger(lua_state, idx) as i64)
    }
}

impl<'lua> FromLua<'lua> for u16 {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TNUMBER {
            return Err(format!("argument {idx}: expected number"));
        }
        let v = lua_tointeger(lua_state, idx);
        if v < 0 || v > u16::MAX as lua_Integer {
            return Err(format!("argument {idx}: number out of range for u16"));
        }
        Ok(v as u16)
    }
}

impl<'lua> FromLua<'lua> for bool {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TBOOLEAN {
            return Err(format!("argument {idx}: expected boolean"));
        }
        Ok(lua_toboolean(lua_state, idx) != 0)
    }
}

/// 原始字节（二进制安全）：Lua 字符串不做 UTF-8 校验
impl<'lua> FromLua<'lua> for Vec<u8> {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(lua_state, idx, &mut len);
        Ok(std::slice::from_raw_parts(p as *const u8, len).to_vec())
    }
}

/// 字符串键值对列表 ↔ Lua table，Rust 侧保留列表顺序并允许重复键。
/// 写入 Lua table 时同名键由后项覆盖；读取顺序取决于 Lua 遍历，非字符串键/值跳过。
#[derive(Clone, Default)]
pub struct StringPairs(pub Vec<(String, String)>);

impl<'lua> FromLua<'lua> for StringPairs {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let mut entries = Vec::new();
        value.table()?.visit(|key, value| {
            if value.kind() == ValueKind::String {
                entries.push((
                    String::from_utf8_lossy(key).into_owned(),
                    value.read::<String>()?,
                ));
            }
            Ok(())
        })?;
        Ok(Self(entries))
    }
}

impl PushLua for StringPairs {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let mut table = output.table(self.0.len());
        for (key, value) in self.0 {
            table.entry(&key, value);
        }
        table.finish()
    }
}

impl<'lua> FromLua<'lua> for String {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(lua_state, idx, &mut len);
        Ok(String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned())
    }
}

/// 二进制字符串的同步借用，不校验 UTF-8；表字段借用沿用调用作用域锚定。
impl<'lua, 'borrow> FromLua<'lua> for &'borrow [u8]
where
    'lua: 'borrow,
{
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        if lua_type(lua_state, value.index) != LUA_TSTRING {
            return Err(value.expected("binary string"));
        }
        let mut len = 0;
        let data = lua_tolstring(lua_state, value.index, &mut len);
        value.anchor_borrow()?;
        Ok(std::slice::from_raw_parts(data.cast::<u8>(), len))
    }
}

/// 零拷贝借用：同步方法专用。借用 Lua 字符串的数据，不复制、不分配。
/// 只能用于同步方法（方法体在 lua_resume 内执行完，字符串被栈锚定不会被 GC）；
/// 异步方法不得使用——future 跨 yield 后栈帧已丢弃，借用会悬垂（宏会拒绝）。
/// 异步方法要用字符串请用 LuaStr（按长度拷贝或锚定）
impl<'lua, 'borrow> FromLua<'lua> for &'borrow str
where
    'lua: 'borrow,
{
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(lua_state, idx, &mut len);
        let bytes = std::slice::from_raw_parts(p as *const u8, len);
        let text = std::str::from_utf8(bytes)
            .map_err(|_| format!("argument {idx}: invalid UTF-8 string"))?;
        value.anchor_borrow()?;
        Ok(text)
    }
}

/// 长度阈值：超过则走 registry 锚定而非拷贝。
/// 拷贝成本 ≈ malloc(10~50ns) + memcpy(~0.1ns/字节)，锚定成本 ≈ 10 次表操作
/// （300~800ns 固定）；二者在约 4KB 处交叉，短串拷贝更省
const STR_ANCHOR_THRESHOLD: usize = 4096;

/// 异步方法的字符串参数：内部按长度选择拷贝或 registry 锚定，调用方不依赖具体存储方式。
/// 同步方法请直接用 &str / &[u8]（零拷贝，表字段按需锚定）。
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
pub struct LuaStr(LuaStrRepr);

/// 异步二进制参数：短串复制，长串使用独立 registry 锚点，不进行文本解码。
pub struct LuaBytes(LuaBytesRepr);

enum LuaBytesRepr {
    Owned(Vec<u8>),
    Anchored {
        bytes: &'static [u8],
        anchor: RegistryRef,
    },
}

impl std::ops::Deref for LuaBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            LuaBytesRepr::Owned(bytes) => bytes,
            LuaBytesRepr::Anchored { bytes, anchor } => {
                assert!(anchor.is_alive(), "Lua VM is closed");
                bytes
            }
        }
    }
}

impl<'lua> FromLua<'lua> for LuaBytes {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        if lua_type(value.lua_state, value.index) != LUA_TSTRING {
            return Err(value.expected("binary string"));
        }
        let mut len = 0;
        let pointer = lua_tolstring(value.lua_state, value.index, &mut len);
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), len);
        let repr = if len <= STR_ANCHOR_THRESHOLD {
            LuaBytesRepr::Owned(bytes.to_vec())
        } else {
            lua_pushvalue(value.lua_state, value.index);
            LuaBytesRepr::Anchored {
                bytes: std::mem::transmute::<&[u8], &'static [u8]>(bytes),
                anchor: RegistryRef::new(value.lua_state),
            }
        };
        Ok(Self(repr))
    }
}

enum LuaStrRepr {
    Owned(String),
    Borrowed(StrAnchor),
}

/// registry 锚定守卫：持有一个 luaL_ref 引用，drop 时 luaL_unref。
/// 被锚定的字符串是 GC 根，其数据指针在守卫存活期间一直有效，
/// 因此 s 的 'static 生命周期是 sound 的（transmute 延展）
struct StrAnchor {
    s: &'static str,
    anchor: RegistryRef,
}

impl std::ops::Deref for LuaStr {
    type Target = str;
    fn deref(&self) -> &str {
        match &self.0 {
            LuaStrRepr::Owned(s) => s,
            LuaStrRepr::Borrowed(a) => {
                assert!(a.anchor.is_alive(), "Lua VM is closed");
                a.s
            }
        }
    }
}

impl<'lua> FromLua<'lua> for LuaStr {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        let mut len: size_t = 0;
        let p = lua_tolstring(lua_state, idx, &mut len);
        let bytes = std::slice::from_raw_parts(p as *const u8, len);
        if len <= STR_ANCHOR_THRESHOLD {
            Ok(LuaStr(LuaStrRepr::Owned(
                String::from_utf8_lossy(bytes).into_owned(),
            )))
        } else {
            // 先校验 UTF-8（此时字符串还在栈上、锚定前不会被 GC），再锚定
            let s = std::str::from_utf8(bytes)
                .map_err(|_| format!("argument {idx}: invalid UTF-8 string"))?;
            lua_pushvalue(lua_state, idx);
            Ok(LuaStr(LuaStrRepr::Borrowed(StrAnchor {
                s: std::mem::transmute::<&str, &'static str>(s),
                anchor: RegistryRef::new(lua_state),
            })))
        }
    }
}

/// 可选参数：实参缺省（栈上无此槽位）或显式 nil 时取 None。
/// 宏不需要特判——Lua 调用少传参数时 lua_type 天然返回 LUA_TNONE。
impl<'lua, T: FromLua<'lua>> FromLua<'lua> for Option<T> {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        match lua_type(lua_state, idx) {
            LUA_TNONE | LUA_TNIL => Ok(None),
            // 组合转换必须保留字段视图的调用作用域与临时标记。
            _ => Ok(Some(T::from_lua(value)?)),
        }
    }
}

/// 任意 Lua 值的拥有型引用，保留 table、函数及 userdata 的原始身份。
/// registry 锚点允许值跨 yield、跨协程传递；不复制 Lua 对象。
/// LuaValue 必须在所属 VM 的同一线程使用，宿主须先回收任务再关闭 VM。
pub struct LuaValue {
    anchor: RegistryRef,
}

impl<'lua> FromLua<'lua> for LuaValue {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) == LUA_TNONE {
            return Err(format!("argument {idx}: expected a value"));
        }
        lua_pushvalue(lua_state, idx);
        Ok(Self {
            anchor: RegistryRef::new(lua_state),
        })
    }
}

impl PushLua for LuaValue {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        (&self).push_lua(output)
    }
}

impl PushLua for &LuaValue {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        self.anchor.push_to(lua_state);
        1
    }
}

/// Lua 函数引用：参数需要在调用返回后继续存活（如 HTTP handler）时使用。
/// 锚定在 registry（luaL_ref），可跨 yield、跨协程使用；Drop 时解除锚定。
/// 借用 VM 的单线程约束与 lua_State 相同。
pub struct LuaFunction {
    anchor: RegistryRef,
}

impl LuaFunction {
    /// 在独立协程中执行回调，参数和结果均通过桥接 trait 转换。
    /// 协程锚点覆盖整个等待与结果读取过程，取消时随 Future 一起回收。
    /// 宿主必须先回收任务，再关闭所属 VM。
    /// 结果必须拥有数据或独立锚点，不能返回依赖临时协程的字符串借用。
    ///
    /// ```compile_fail
    /// use cosmos::userdata::LuaFunction;
    /// async fn invalid(callback: &LuaFunction) {
    ///     let _: &str = callback.call(()).await.unwrap();
    /// }
    /// ```
    pub async fn call<A: PushLua, R: for<'lua> FromLua<'lua>>(&self, args: A) -> Result<R, String> {
        if !self.anchor.is_alive() {
            return Err("Lua VM is closed".to_string());
        }
        let thread = unsafe {
            let lua_state = lua_newthread(self.anchor.main);
            self.anchor.push_to(lua_state);
            args.push_lua(&mut Output::new(lua_state));
            crate::light_thread::LightThread::new(self.anchor.main, lua_state)
        };
        thread.run::<R>().await
    }
}

impl<'lua> FromLua<'lua> for LuaFunction {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        if lua_type(lua_state, idx) != LUA_TFUNCTION {
            return Err(format!("argument {idx}: expected function"));
        }
        lua_pushvalue(lua_state, idx);
        Ok(LuaFunction {
            anchor: RegistryRef::new(lua_state),
        })
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
    /// 将值压入所属 VM 的栈，并返回压入的值数。
    ///
    /// # Safety
    /// 输出上下文所属 VM 在转换期间保持存活，值中的 Lua 引用必须属于该 VM。
    /// 仅通过输出协议组合值，不执行 Lua 或挂起；返回数量必须等于实际输出的值数。
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32;
}

impl PushLua for () {
    unsafe fn push_lua(self, _output: &mut Output<'_>) -> i32 {
        0
    }
}
impl PushLua for i64 {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        unsafe { lua_pushinteger(lua_state, self as lua_Integer) };
        1
    }
}
impl PushLua for u16 {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        unsafe { lua_pushinteger(lua_state, self as lua_Integer) };
        1
    }
}
impl PushLua for f64 {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        unsafe { lua_pushnumber(lua_state, self) };
        1
    }
}
impl PushLua for bool {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        unsafe { lua_pushboolean(lua_state, self as c_int) };
        1
    }
}
impl PushLua for String {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        self.as_bytes().push_lua(output)
    }
}

impl PushLua for &[u8] {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        lua_pushlstring(output.lua_state, self.as_ptr().cast(), self.len());
        1
    }
}

impl PushLua for Vec<u8> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        self.as_slice().push_lua(output)
    }
}
/// Lua nil 作为"一个值"的标记：多返回值里占位用
/// （`()` 是 0 个返回值，二者语义不同）
pub struct Nil;

/// 可保存在表与数组中的空值，对应 cosmos.null，与 Lua nil 区分。
/// 使用空指针 lightuserdata，与 ngx.null / cjson.null 的表示一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Null;

impl<'lua> FromLua<'lua> for Null {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        if lua_type(lua_state, value.index) == LUA_TLIGHTUSERDATA
            && lua_touserdata(lua_state, value.index).is_null()
        {
            Ok(Null)
        } else {
            Err(value.expected("cosmos.null"))
        }
    }
}

impl PushLua for Null {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        unsafe { lua_pushlightuserdata(output.lua_state, std::ptr::null_mut()) };
        1
    }
}

impl<'lua> FromLua<'lua> for Nil {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let lua_state = value.lua_state;
        let idx = value.index;
        match lua_type(lua_state, idx) {
            LUA_TNONE | LUA_TNIL => Ok(Nil),
            _ => Err(format!("argument {idx}: expected nil")),
        }
    }
}

/// 无返回值的回调忽略 Lua 返回内容，供 WebSocket handler 等场景使用。
impl<'lua> FromLua<'lua> for () {
    unsafe fn from_lua(_value: Value<'lua>) -> Result<Self, String> {
        Ok(())
    }
}

impl PushLua for Nil {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        unsafe { lua_pushnil(lua_state) };
        1
    }
}
impl<T: PushLua> PushLua for Option<T> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        match self {
            Some(v) => v.push_lua(&mut Output::new(lua_state)),
            // None 压一个 nil：作为表字段时与 setfield 配对，
            // 作为多返回值时 `local v = f()` 同样是 nil
            None => {
                unsafe { lua_pushnil(lua_state) };
                1
            }
        }
    }
}
/// userdata 返回：直接构造压栈，无需为返回值单独装箱
impl<T: UserData> PushLua for T {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        push::<T>(lua_state, self);
        1
    }
}

/// 多返回值：2 元组依次压栈（split 这类"一次返回多个 userdata"的同步方法用）
impl<A: PushLua, B: PushLua> PushLua for (A, B) {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        let (a, b) = self;
        a.push_lua(&mut Output::new(lua_state)) + b.push_lua(&mut Output::new(lua_state))
    }
}

/// 三元组组合已有的二元组协议，按顺序输出三个成员。
impl<A: PushLua, B: PushLua, C: PushLua> PushLua for (A, B, C) {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        ((self.0, self.1), self.2).push_lua(output)
    }
}

/// 字符串切片：避免为 &'static str 常量额外分配 String
impl PushLua for &str {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        self.as_bytes().push_lua(output)
    }
}

/// Result<T, E> 统一走 nil + 错误串（标准 Lua 形态）：
/// Ok(v) 压 v，Err(e) 压 nil + e.to_string()（两个返回值）。
/// 同步与异步方法共用这一份语义
impl<T: PushLua, E: Display> PushLua for Result<T, E> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let lua_state = output.lua_state;
        match self {
            Ok(v) => v.push_lua(&mut Output::new(lua_state)),
            Err(e) => {
                let msg = e.to_string();
                unsafe {
                    lua_pushnil(lua_state);
                    lua_pushlstring(lua_state, msg.as_ptr() as *const c_char, msg.len());
                }
                2
            }
        }
    }
}

#[cfg(test)]
mod derive_tests {
    use super::*;

    fn with_state(f: impl FnOnce(*mut lua_State)) {
        unsafe {
            let lua_state = luaL_newstate();
            assert!(!lua_state.is_null(), "luaL_newstate failed");
            luaL_openlibs(lua_state);
            init(lua_state);
            f(lua_state);
            cleanup(lua_state);
            lua_close(lua_state);
        }
    }

    /// 建一张整数/字符串字段的表（栈顶 +1）
    unsafe fn push_int_table(lua_state: *mut lua_State, pairs: &[(&str, lua_Integer)]) {
        lua_createtable(lua_state, 0, pairs.len() as c_int);
        for (k, v) in pairs {
            lua_pushlstring(lua_state, k.as_ptr() as *const c_char, k.len());
            lua_pushinteger(lua_state, *v);
            lua_settable(lua_state, -3);
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
        with_state(|lua_state| unsafe {
            push_int_table(lua_state, &[("c", 3)]);
            let v = FieldDefaults::from_lua(Value::new(lua_state, 1)).expect("read table");
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
        with_state(|lua_state| unsafe {
            push_int_table(lua_state, &[("a", 10), ("b", 20), ("c", 30), ("d", 40)]);
            let v = FieldDefaults::from_lua(Value::new(lua_state, 1)).expect("read table");
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
        with_state(|lua_state| unsafe {
            push_int_table(lua_state, &[("a", 1)]);
            let err = FieldDefaults::from_lua(Value::new(lua_state, 1)).unwrap_err();
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
        with_state(|lua_state| unsafe {
            push_int_table(lua_state, &[("b", 5)]);
            let v = ContainerDefaults::from_lua(Value::new(lua_state, 1)).expect("read table");
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
        with_state(|lua_state| unsafe {
            push_int_table(lua_state, &[("b", 5)]);
            let v = FnDefaults::from_lua(Value::new(lua_state, 1)).expect("read table");
            assert_eq!(v, FnDefaults { a: 11, b: 5 });
        });
    }

    #[test]
    fn non_table_is_error() {
        with_state(|lua_state| unsafe {
            lua_pushinteger(lua_state, 1);
            let err = ContainerDefaults::from_lua(Value::new(lua_state, 1)).unwrap_err();
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
        with_state(|lua_state| unsafe {
            Pushed { a: 42, b: None }.push_lua(&mut Output::new(lua_state));
            assert_eq!(lua_type(lua_state, -1), LUA_TTABLE);

            lua_getfield(lua_state, -1, c"a".as_ptr());
            assert_eq!(lua_tointeger(lua_state, -1), 42);
            lua_settop(lua_state, -2);

            lua_getfield(lua_state, -1, c"b".as_ptr());
            assert_eq!(lua_type(lua_state, -1), LUA_TNIL);
        });
    }

    #[derive(cosmos::PushLua)]
    enum Outcome<T: PushLua> {
        Empty,
        Data(T),
        Closed {
            lua_state: Nil,
            reason: &'static str,
            partial: T,
        },
    }

    #[test]
    fn enum_push_preserves_count_order_and_binary_data() {
        with_state(|lua_state| unsafe {
            assert_eq!(
                Outcome::<Vec<u8>>::Empty.push_lua(&mut Output::new(lua_state)),
                0
            );
            assert_eq!(lua_gettop(lua_state), 0);

            assert_eq!(
                Outcome::Data(vec![0, 255, 1]).push_lua(&mut Output::new(lua_state)),
                1
            );
            assert_eq!(
                Vec::<u8>::from_lua(Value::new(lua_state, 1)).unwrap(),
                [0, 255, 1]
            );
            lua_settop(lua_state, 0);

            let outcome = Outcome::Closed {
                lua_state: Nil,
                reason: "closed",
                partial: vec![255, 0],
            };
            assert_eq!(outcome.push_lua(&mut Output::new(lua_state)), 3);
            assert_eq!(lua_gettop(lua_state), 3);
            assert_eq!(lua_type(lua_state, 1), LUA_TNIL);
            assert_eq!(
                String::from_lua(Value::new(lua_state, 2)).unwrap(),
                "closed"
            );
            assert_eq!(
                Vec::<u8>::from_lua(Value::new(lua_state, 3)).unwrap(),
                [255, 0]
            );
        });
    }

    #[derive(cosmos::FromLua)]
    enum CallbackReply<T: for<'lua> FromLua<'lua>> {
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
        with_state(|lua_state| unsafe {
            type Reply = CallbackReply<ContainerDefaults>;
            assert!(matches!(
                Reply::from_lua(Value::new(lua_state, 1)).unwrap(),
                Reply::Nothing
            ));
            lua_pushinteger(lua_state, 42);
            assert!(matches!(
                Reply::from_lua(Value::new(lua_state, -1)).unwrap(),
                Reply::Number { value: 42 }
            ));

            push_int_table(lua_state, &[("a", 7), ("b", 9)]);
            match Reply::from_lua(Value::new(lua_state, -1)).unwrap() {
                Reply::Table(value) => assert_eq!(value, ContainerDefaults { a: 7, b: 9 }),
                _ => panic!("unexpected return variant"),
            }
            assert_eq!(lua_gettop(lua_state), 2);
            lua_settop(lua_state, 0);

            vec![0u8, 255, 1].push_lua(&mut Output::new(lua_state));
            match Reply::from_lua(Value::new(lua_state, 1)).unwrap() {
                Reply::Body(bytes) => assert_eq!(bytes, [0, 255, 1]),
                _ => panic!("unexpected return variant"),
            }
            lua_settop(lua_state, 0);
            Nil.push_lua(&mut Output::new(lua_state));
            "callback error".push_lua(&mut Output::new(lua_state));
            match Reply::from_lua(Value::new(lua_state, -2)).unwrap() {
                Reply::Empty(_, Some(error)) => assert_eq!(error, "callback error"),
                _ => panic!("unexpected return variant"),
            }
            assert_eq!(lua_gettop(lua_state), 2);
        });
    }

    #[test]
    fn failed_nested_conversion_restores_stack_and_allows_retry() {
        with_state(|lua_state| unsafe {
            lua_createtable(lua_state, 0, 1);
            "bad".push_lua(&mut Output::new(lua_state));
            lua_setfield(lua_state, -2, c"a".as_ptr());
            assert!(
                CallbackReply::<ContainerDefaults>::from_lua(Value::new(lua_state, -1)).is_err()
            );
            assert_eq!(lua_gettop(lua_state), 1);

            10i64.push_lua(&mut Output::new(lua_state));
            lua_setfield(lua_state, -2, c"a".as_ptr());
            let value = ContainerDefaults::from_lua(Value::new(lua_state, -1)).unwrap();
            assert_eq!(value.a, 10);
            assert_eq!(lua_gettop(lua_state), 1);
        });
    }

    #[test]
    fn table_contexts_restore_stack_on_panic_and_reject_multi_value_fields() {
        with_state(|lua_state| unsafe {
            let mut output = Output::new(lua_state);
            let mut table = output.table(1);
            table.field(c"value", 42i64);
            assert_eq!(table.finish(), 1);

            let failure = catch_unwind(AssertUnwindSafe(|| {
                let mut table = output.table(1);
                table.field(c"invalid", (1i64, 2i64));
                table.finish();
            }));
            assert!(failure.is_err());
            assert_eq!(lua_gettop(lua_state), 1);

            let failure = catch_unwind(AssertUnwindSafe(|| {
                Value::new(lua_state, -1)
                    .table()
                    .unwrap()
                    .visit(|_, value| {
                        assert_eq!(value.read::<i64>()?, 42);
                        panic!("iteration aborted");
                    })
                    .unwrap();
            }));
            assert!(failure.is_err());
            assert_eq!(lua_gettop(lua_state), 1);
            assert_eq!(
                Value::new(lua_state, 1)
                    .read::<StringPairs>()
                    .unwrap()
                    .0
                    .len(),
                0
            );
        });
    }

    #[test]
    fn string_pairs_preserve_nul_keys_and_values_and_ignore_non_string_keys() {
        with_state(|lua_state| unsafe {
            let mut output = Output::new(lua_state);
            StringPairs(vec![(
                "key\0\u{4e2d}\u{6587}".to_string(),
                "value\0\u{5c3e}".to_string(),
            )])
            .push_lua(&mut output);
            lua_pushinteger(lua_state, 7);
            "ignore numeric keys".push_lua(&mut output);
            lua_settable(lua_state, -3);
            let pairs = Value::new(lua_state, -1).read::<StringPairs>().unwrap();
            assert_eq!(
                pairs.0,
                [(
                    "key\0\u{4e2d}\u{6587}".to_string(),
                    "value\0\u{5c3e}".to_string()
                )]
            );
            assert_eq!(lua_gettop(lua_state), 1);
        });
    }

    #[test]
    fn userdata_conversion_reports_type_and_borrow_errors_without_longjmp() {
        struct Value;
        impl UserData for Value {}
        struct Other;
        impl UserData for Other {}

        with_state(|lua_state| unsafe {
            push(lua_state, Value);
            let guard = try_check::<Value>(lua_state, -1).unwrap();
            assert!(try_check_shared::<Value>(lua_state, 1).is_err());
            assert!(try_check::<Value>(lua_state, 1).is_err());
            assert_eq!(lua_gettop(lua_state), 1);
            drop(guard);
            assert!(try_check::<Value>(lua_state, 1).is_ok());

            push(lua_state, Other);
            assert!(try_check::<Value>(lua_state, -1).is_err());
            assert!(try_check_shared::<Value>(lua_state, -1).is_err());
            assert_eq!(lua_gettop(lua_state), 2);
        });
    }

    #[test]
    fn module_paths_preserve_identity_in_both_registration_orders() {
        struct Parent;
        #[cosmos::userdata(name = "module_test.parent")]
        impl Parent {
            fn new() -> Self {
                Self
            }

            fn value(&self) -> i64 {
                7
            }
        }

        struct Child;
        #[cosmos::userdata(name = "module_test.parent.child")]
        impl Child {
            fn new() -> Self {
                Self
            }
        }

        for child_first in [true, false] {
            with_state(|lua_state| unsafe {
                // 注册与重复注册都必须保留调用方已有的栈值。
                42i64.push_lua(&mut Output::new(lua_state));
                if child_first {
                    register::<Child>(lua_state);
                } else {
                    register::<Parent>(lua_state);
                }
                assert_eq!(lua_gettop(lua_state), 1);
                assert_eq!(lua_tointeger(lua_state, 1), 42);
                crate::libs::run_lua_source(
                    lua_state,
                    "module registration: first phase",
                    r#"
                    cached_root = require("cosmos")
                    cached_module = require("cosmos.module_test")
                    cached_parent = require("cosmos.module_test.parent")
                    assert(cached_root == cosmos)
                    assert(cached_root.module_test == cached_module)
                    assert(cached_module.parent == cached_parent)
                    cached_parent.marker = "\228\191\157\231\149\153"
                "#,
                )
                .unwrap();

                register::<Parent>(lua_state);
                register::<Child>(lua_state);
                register::<Parent>(lua_state);
                assert_eq!(lua_gettop(lua_state), 1);
                crate::libs::run_lua_source(
                    lua_state,
                    "module registration: second phase",
                    r#"
                    local parent = require("cosmos.module_test.parent")
                    local child = require("cosmos.module_test.parent.child")
                    assert(require("cosmos") == cached_root)
                    assert(require("cosmos.module_test") == cached_module)
                    assert(parent == cached_parent and parent.marker == "\228\191\157\231\149\153")
                    assert(parent.child == child)
                    assert(parent.new():value() == 7)
                    assert(getmetatable(parent.new()) == parent)
                    assert(getmetatable(child.new()) == child)
                    parent.lua_method = function() return 9 end
                "#,
                )
                .unwrap();
                // 懒 push 同样复用注册结果，不抹去 Lua 增补的方法。
                push(lua_state, Parent);
                assert_eq!(lua_gettop(lua_state), 2);
                crate::libs::run_lua_source(
                    lua_state,
                    "lazy module registration",
                    r#"
                    assert(cached_parent.lua_method() == 9)
                "#,
                )
                .unwrap();
                assert_eq!(lua_gettop(lua_state), 2);
            });
        }
    }

    #[test]
    fn lua_value_preserves_identity_nil_and_stack_height() {
        with_state(|lua_state| unsafe {
            lua_createtable(lua_state, 0, 0);
            let value = LuaValue::from_lua(Value::new(lua_state, 1)).unwrap();
            assert_eq!(lua_gettop(lua_state), 1);
            let thread = lua_newthread(lua_state);
            assert_eq!(value.push_lua(&mut Output::new(thread)), 1);
            lua_xmove(thread, lua_state, 1);
            assert_ne!(lua_rawequal(lua_state, 1, -1), 0);
            lua_settop(lua_state, 0);

            // registry 的负键也可被 Lua 写入，nil 引用不能误读该槽位。
            lua_pushinteger(lua_state, LUA_REFNIL as lua_Integer);
            "not nil".push_lua(&mut Output::new(lua_state));
            lua_settable(lua_state, LUA_REGISTRYINDEX);
            lua_pushnil(lua_state);
            let nil = LuaValue::from_lua(Value::new(lua_state, 1)).unwrap();
            lua_settop(lua_state, 0);
            assert_eq!(nil.push_lua(&mut Output::new(lua_state)), 1);
            assert_eq!(lua_type(lua_state, 1), LUA_TNIL);
            assert!(LuaValue::from_lua(Value::new(lua_state, 2)).is_err());
            assert_eq!(lua_gettop(lua_state), 1);
        });
    }

    #[test]
    fn owned_userdata_receiver_does_not_hold_borrow_across_yield() {
        use std::cell::Cell;
        use std::future::Future;
        use std::rc::Rc;
        use std::task::{Context, Poll, Waker};

        #[derive(Clone)]
        struct Shared(Rc<Cell<i64>>);

        #[cosmos::userdata(name = "OwnedReceiverTest")]
        impl Shared {
            async fn tick(self) -> i64 {
                let mut first = true;
                std::future::poll_fn(|cx| {
                    if first {
                        first = false;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
                self.0.set(self.0.get() + 1);
                self.0.get()
            }
        }

        with_state(|lua_state| unsafe {
            let thread = lua_newthread(lua_state);
            assert_eq!(
                luaL_loadstring(thread, c"local s = ...; return s:tick()".as_ptr()),
                0
            );
            push(thread, Shared(Rc::new(Cell::new(0))));
            let light_thread = crate::light_thread::LightThread::new(lua_state, thread);
            let mut run = Box::pin(light_thread.run::<i64>());
            lua_pushvalue(thread, 2);
            lua_xmove(thread, lua_state, 1);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(run.as_mut().poll(&mut cx).is_pending());
            // 原 userdata 在挂起期间仍可独占借用，且句柄操作相同的共享状态。
            try_check::<Shared>(lua_state, 1).unwrap().0.set(10);
            assert_eq!(run.as_mut().poll(&mut cx), Poll::Ready(Ok(11)));
            drop(run);
        });
    }

    #[test]
    fn typed_callback_keeps_arguments_and_results_alive_across_yield() {
        with_state(|lua_state| unsafe {
            crate::builtin::open(lua_state);
            assert_eq!(
                luaL_loadstring(
                    lua_state,
                    c"return function(value) cosmos.time.sleep(1); return nil, value end".as_ptr()
                ),
                0
            );
            assert_eq!(lua_pcall(lua_state, 0, 1, 0), 0);
            let callback = LuaFunction::from_lua(Value::new(lua_state, -1)).unwrap();
            lua_settop(lua_state, 0);
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
                _ => panic!("unexpected return variant"),
            }
            assert_eq!(lua_gettop(lua_state), 0);
            drop(local);
        });
    }
}
