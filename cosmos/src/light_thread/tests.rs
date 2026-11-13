use super::*;
use crate::ffi::*;
use crate::userdata::{self, PushLua};
use std::cell::Cell;
use std::ffi::CStr;
use std::task::Waker;

struct Probe(Rc<Cell<bool>>);

#[cosmos::userdata(name = "test.lifecycle")]
impl Probe {
    async fn hold(&mut self) {
        std::future::pending::<()>().await;
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

struct Vm(*mut lua_State);

impl Drop for Vm {
    fn drop(&mut self) {
        unsafe {
            userdata::cleanup(self.0);
            lua_close(self.0);
        }
    }
}

fn with_thread(source: &CStr, probe: Probe, test: impl FnOnce(*mut lua_State, LightThread)) {
    unsafe {
        let vm = Vm(luaL_newstate());
        assert!(!vm.0.is_null());
        userdata::init(vm.0);
        luaL_openlibs(vm.0);
        let lua_state = lua_newthread(vm.0);
        assert_eq!(luaL_loadstring(lua_state, source.as_ptr()), 0);
        probe.push_lua(lua_state);
        test(vm.0, LightThread::new(vm.0, lua_state));
    }
}

#[test]
fn dropping_an_unpolled_run_releases_the_anchor_and_publishes_cancellation() {
    let dropped = Rc::new(Cell::new(false));
    with_thread(
        c"error('must not execute')",
        Probe(dropped.clone()),
        |main, thread| {
            let mut context = Context::from_waker(Waker::noop());
            let run = thread.run::<()>();
            let mut duplicate = Box::pin(thread.clone().run::<()>());
            assert!(matches!(
                duplicate.as_mut().poll(&mut context),
                Poll::Ready(Err(_))
            ));
            drop(run);
            let mut completion = Box::pin(thread.completion());
            assert_eq!(
                completion.as_mut().poll(&mut context),
                Poll::Ready(Err(CANCELLED.to_string()))
            );
            // 即使共享句柄仍活着，取消的执行也不能继续锚定其参数或 Lua 协程。
            unsafe {
                assert_eq!(
                    luaL_loadstring(main, c"collectgarbage('collect')".as_ptr()),
                    0
                );
                assert_eq!(lua_pcall(main, 0, 0, 0), 0);
            }
            assert!(dropped.get());
        },
    );
}

#[test]
fn cancellation_releases_pending_borrows_before_publishing_completion() {
    for request_cancel in [false, true] {
        let dropped = Rc::new(Cell::new(false));
        with_thread(
            c"object = ...; object:hold()",
            Probe(dropped.clone()),
            |main, thread| {
                let mut context = Context::from_waker(Waker::noop());
                let mut run = Box::pin(thread.run::<()>());
                assert!(run.as_mut().poll(&mut context).is_pending());
                unsafe {
                    lua_getfield(main, LUA_GLOBALSINDEX, c"object".as_ptr());
                    assert!(userdata::try_check::<Probe>(main, -1).is_err());
                }
                let mut completion = Box::pin(thread.completion());
                assert!(completion.as_mut().poll(&mut context).is_pending());
                if request_cancel {
                    thread.cancel();
                    assert_eq!(
                        run.as_mut().poll(&mut context),
                        Poll::Ready(Err(CANCELLED.to_string()))
                    );
                }
                drop(run);
                assert_eq!(
                    completion.as_mut().poll(&mut context),
                    Poll::Ready(Err(CANCELLED.to_string()))
                );
                unsafe {
                    assert!(userdata::try_check::<Probe>(main, -1).is_ok());
                    lua_settop(main, 0);
                    assert_eq!(
                        luaL_loadstring(main, c"object=nil; collectgarbage('collect')".as_ptr()),
                        0
                    );
                    assert_eq!(lua_pcall(main, 0, 0, 0), 0);
                }
                assert!(dropped.get());
            },
        );
    }
}
