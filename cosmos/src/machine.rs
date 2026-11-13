//! 可嵌入的运行环境：管理 Lua VM、模块注入和本地协程调度。

use std::ffi::CString;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};
use tokio::runtime::Handle;
use tokio::task::{JoinHandle, LocalSet};

use crate::coroutine::LuaCoroutine;
use crate::ffi::*;
use crate::userdata::{self, UserData};
use crate::{builtin, stack};

/// 编译进宿主的插件，通过 Machine 的安全接口注册类型和加载 Lua 定义。
/// 注册在 Machine 所在线程同步执行，无需 Send/Sync，也不要求插件操作 Lua 裸指针。
pub trait Module {
    fn register(&self, machine: &mut Machine) -> Result<(), String>;
}

/// 一个独立 Lua VM 与 LocalSet，复用调用方注入的 Tokio runtime。
/// 多线程 runtime 的工作线程处理普通 Rust 任务，Lua 协程始终留在本对象所在线程。
/// 脚本之间保留全局状态和后台任务；销毁时先取消任务，再关闭 VM。
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<cosmos::Machine>();
/// ```
pub struct Machine {
    lua_state: NonNull<lua_State>,
    local: Option<LocalSet>,
    runtime: Handle,
}

impl Machine {
    /// 创建 VM 并装载内建模块。runtime 可为单线程或多线程，需启用所用的 IO/时间驱动。
    pub fn new(runtime: Handle) -> Result<Self, String> {
        let lua_state = NonNull::new(unsafe { luaL_newstate() })
            .ok_or_else(|| "创建 Lua VM 失败".to_string())?;
        let machine = Self {
            lua_state,
            local: Some(LocalSet::new()),
            runtime,
        };
        unsafe {
            userdata::init(lua_state.as_ptr());
            luaL_openlibs(lua_state.as_ptr());
            builtin::open(lua_state.as_ptr());
        }
        Ok(machine)
    }

    /// 注入插件；插件可注册 userdata、加载定义，也可启动本地后台任务。
    pub fn add_module<M: Module>(&mut self, module: M) -> Result<(), String> {
        let runtime = self.runtime.clone();
        let _runtime = runtime.enter();
        let _local = self.local.as_ref().unwrap().enter();
        module.register(self)
    }

    /// 将声明了 Lua 路径的 userdata 类型注册到本 VM。
    pub fn register<T: UserData>(&mut self) {
        unsafe { userdata::register::<T>(self.lua_state.as_ptr()) };
    }

    /// 同步加载模块的 Lua 定义；需要异步等待时返回错误，用户脚本应通过 exec 执行。
    pub fn load(&mut self, name: &str, source: &str) -> Result<(), String> {
        let mut coroutine = self.prepare(name, source, &[])?;
        let _entered = self.runtime.enter();
        let _local = self.local.as_ref().unwrap().enter();
        let mut context = Context::from_waker(Waker::noop());
        match Pin::new(&mut coroutine).poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(format!("模块 {name} 的同步定义不能等待异步操作")),
        }
    }

    /// 启动本地任务，在本 Machine 执行脚本时推进，销毁时取消。
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        self.local.as_ref().unwrap().spawn_local(future)
    }

    /// 在注入的 runtime 上异步执行，宿主负责驱动 Runtime。
    pub async fn exec(&mut self, source: &str) -> Result<(), String> {
        self.exec_with_args("=Machine", source, &[]).await
    }

    /// 指定 chunk 名称及传给 Lua `...` 的字符串参数。
    pub async fn exec_with_args(
        &mut self,
        name: &str,
        source: &str,
        args: &[String],
    ) -> Result<(), String> {
        let current =
            Handle::try_current().map_err(|_| "执行 Lua 需要 Tokio 上下文".to_string())?;
        if current.id() != self.runtime.id() {
            return Err("必须在注入的 Tokio runtime 中执行 Lua".to_string());
        }
        let coroutine = self.prepare(name, source, args)?;
        self.local.as_ref().unwrap().run_until(coroutine).await
    }

    /// 读取并执行文件，参数对应 Lua chunk 的 `...`。
    pub async fn exec_file(
        &mut self,
        path: impl AsRef<Path>,
        args: &[String],
    ) -> Result<(), String> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
        self.exec_with_args(&format!("@{}", path.display()), &source, args)
            .await
    }

    fn prepare(
        &mut self,
        name: &str,
        source: &str,
        args: &[String],
    ) -> Result<LuaCoroutine, String> {
        let name = CString::new(name).map_err(|_| "chunk 名称不能包含 NUL".to_string())?;
        unsafe {
            let lua_state = self.lua_state.as_ptr();
            let top = lua_gettop(lua_state);
            let thread = lua_newthread(lua_state);
            if luaL_loadbuffer(thread, source.as_ptr().cast(), source.len(), name.as_ptr()) != 0 {
                let error = stack::err_message(thread);
                lua_settop(lua_state, top);
                return Err(error);
            }
            for arg in args {
                lua_pushlstring(thread, arg.as_ptr().cast(), arg.len());
            }
            Ok(LuaCoroutine::new(lua_state, thread))
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // 本地任务可能持有 userdata 的 Ref/RefMut，必须在 lua_close 之前释放。
        drop(self.local.take());
        unsafe {
            userdata::cleanup(self.lua_state.as_ptr());
            lua_close(self.lua_state.as_ptr());
        }
    }
}
