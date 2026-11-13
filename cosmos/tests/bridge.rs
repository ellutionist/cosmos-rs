//! 从外部 crate 验证受控桥接，避免宏意外依赖 cosmos 私有实现。
#![deny(warnings)]

use cosmos::userdata::{Call, LuaCallable, LuaClosure, LuaFunction, LuaStr, Nil, Output, PushLua};
use cosmos::{Machine, Module};
use std::future::Future;
use std::task::{Context, Poll, Waker};

// 调用方常见的 Result 别名不能影响过程宏生成代码。
type Result<T> = std::result::Result<T, String>;

#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    prefix: String,
}

#[derive(cosmos::FromLua)]
struct BorrowedOptions<'a> {
    text: &'a str,
}

#[derive(cosmos::PushLua)]
struct Reply {
    text: String,
    len: i64,
}

#[derive(cosmos::FromLua)]
struct Empty {}

#[derive(cosmos::FromLua)]
struct Borrowed<'lua> {
    text: &'lua str,
}

#[derive(cosmos::PushLua)]
struct EmptyReply {}

#[derive(cosmos::PushLua)]
struct InvalidField {
    value: (),
}

struct Binary(Vec<u8>);

impl PushLua for Binary {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        self.0.as_slice().push_lua(output)
    }
}

#[derive(cosmos::PushLua)]
enum Item {
    Data(Binary),
    End(Nil),
}

struct Cursor(i64);

#[cosmos::userdata(name = "test.cursor")]
impl Cursor {}

// 只捕获自己的 userdata 状态，输出数量由 suspend 产生；守卫不离开本次挂起操作。
unsafe impl LuaCallable for Cursor {
    fn call(call: &mut Call<'_>) -> std::result::Result<i32, String> {
        let mut cursor = unsafe { call.capture().borrow_mut::<Self>()? };
        Ok(call.suspend(async move {
            tokio::task::yield_now().await;
            if cursor.0 == 0 {
                Item::End(Nil)
            } else {
                cursor.0 -= 1;
                Item::Data(Binary(vec![0, 255, cursor.0 as u8]))
            }
        }))
    }
}

struct Api;

#[cosmos::userdata(name = "test.bridge")]
impl Api {
    async fn noop() {}

    fn empty(_value: Empty) -> EmptyReply {
        EmptyReply {}
    }

    fn borrowed(value: &str) -> String {
        value.to_owned()
    }

    fn passthrough(value: &str) -> &str {
        value
    }

    fn structured(value: Borrowed<'_>) -> String {
        value.text.to_owned()
    }

    fn borrowed_identity(value: &str) -> &str {
        value
    }

    fn borrowed_options(value: BorrowedOptions<'_>) -> String {
        value.text.to_owned()
    }

    fn borrowed_after_callback(value: BorrowedOptions<'_>, callback: LuaFunction) -> String {
        let mut run = Box::pin(callback.call::<_, ()>(()));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            run.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        value.text.to_owned()
    }

    async fn echo(value: LuaStr, opts: Option<Options>) -> Reply {
        tokio::task::yield_now().await;
        Reply {
            text: format!("{}{}", opts.unwrap_or_default().prefix, &*value),
            len: value.len() as i64,
        }
    }

    fn cursor(count: i64) -> LuaClosure<Cursor> {
        LuaClosure(Cursor(count))
    }

    fn invalid_field() -> InvalidField {
        InvalidField { value: () }
    }
}

struct Plugin;

impl Module for Plugin {
    fn register(&self, machine: &mut Machine) -> Result<()> {
        machine.register::<Api>();
        machine.load("桥接插件定义", "bridge_ready = true")
    }
}

#[test]
fn external_macros_and_closures_use_only_controlled_bridge_operations() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Plugin).unwrap();
    runtime.block_on(machine.exec(r#"
        assert(bridge_ready)
        local api = require('cosmos.test.bridge')
        assert(select('#', api.noop()) == 0)
        assert(type(api.empty({})) == 'table')
        assert(not pcall(api.empty, 'bad'))
        assert(api.borrowed('同步借用') == '同步借用')
        assert(api.passthrough('a\0b') == 'a\0b')
        assert(api.structured({text='表字段借用'}) == '表字段借用')
        assert(api.borrowed_identity('原样返回借用') == '原样返回借用')
        assert(api.borrowed_options({text='结构体借用'}) == '结构体借用')
        local target = {text=string.rep('持有',8192)}
        local retained = api.borrowed_after_callback(target, function()
            target.text = nil
            collectgarbage('collect')
            for i=1,32 do string.rep('覆写',8192) end
            collectgarbage('collect')
        end)
        assert(retained == string.rep('持有',8192), '重入删除字段并 GC 后借用仍应有效')
        local gc = cosmos.thread.spawn(function()
            for i=1,10 do collectgarbage('collect'); cosmos.time.sleep(0) end
        end)
        for _, n in ipairs({8,4096,4097,8192}) do
            local reply = api.echo(string.rep('a',n)..'\0tail', {prefix='前缀', ignored=true, [1]=false})
            assert(reply.len == n+5)
            assert(reply.text == '前缀'..string.rep('a',n)..'\0tail')
        end
        assert(api.echo('默认').text == '默认')
        assert(gc:join() == nil)
        local next = api.cursor(2)
        assert(next() == string.char(0,255,1))
        collectgarbage('collect')
        assert(next() == string.char(0,255,0))
        assert(select('#', next()) == 1 and next() == nil)
        local ok, err = pcall(api.invalid_field)
        assert(not ok and err:find('表字段必须恰好输出一个值', 1, true))
        assert(api.borrowed('失败后可继续调用') == '失败后可继续调用')
    "#)).unwrap();
}
