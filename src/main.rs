use std::ffi::CString;

use cosmos::coroutine::LuaCoroutine;
use cosmos::ffi::*;
use cosmos::{libs, stack, userdata};

mod demo;

fn main() {
    let mut cli = std::env::args();
    let path = cli.nth(1).expect("usage: cosmos-rs <script.lua> [args...]");
    let script_args: Vec<String> = cli.collect();
    let code = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    match rt.block_on(run(&code, &script_args)) {
        Ok(()) => {}
        Err(msg) => {
            eprintln!("lua error: {msg}");
            std::process::exit(1);
        }
    }
}

async fn run(code: &str, args: &[String]) -> Result<(), String> {
    unsafe {
        let L = luaL_newstate();
        if L.is_null() {
            return Err("failed to create lua state".to_string());
        }
        luaL_openlibs(L);
        libs::open_cosmos(L);
        userdata::init(L);
        demo::register(L);
        cosmos_ext_demo::register(L);

        // 脚本跑在独立协程上（主栈只承载全局状态），这样 sleep 才能 yield
        let thread = lua_newthread(L);

        let ccode = CString::new(code).unwrap();
        if luaL_loadstring(thread, ccode.as_ptr()) != 0 {
            let msg = stack::err_message(thread);
            lua_close(L);
            return Err(msg);
        }

        // 初始参数：压在 chunk 函数之上，首次 resume 按 gettop - 1 传给脚本的 `...`
        for arg in args {
            lua_pushlstring(thread, arg.as_ptr() as *const cosmos::ffi::c_char, arg.len());
        }

        let result = LuaCoroutine::new(L, thread).await;
        userdata::cleanup(L);
        lua_close(L);
        result
    }
}
