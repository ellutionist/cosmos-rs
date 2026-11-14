//! 从外部 crate 验证受控桥接，避免宏意外依赖 cosmos 私有实现。
#![deny(warnings)]

use cosmos::userdata::{
    Call, FromLua, LuaBytes, LuaCallable, LuaClosure, LuaFunction, LuaStr, Nil, Output, PushLua,
    Value,
};
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
    #[lua(default)]
    data: Option<&'a [u8]>,
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
struct UnsignedReply {
    small: u8,
    wide: u32,
}

#[derive(cosmos::PushLua)]
struct InvalidField {
    value: (),
}

struct InvalidContainer(bool);

impl PushLua for InvalidContainer {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let mut table = output.array(1);
        if self.0 {
            table.index(1, ());
            table.finish()
        } else {
            table.finish_with_metatable("invalid metatable")
        }
    }
}

struct MetaText<'lua>(&'lua str);

impl<'lua> FromLua<'lua> for MetaText<'lua> {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self> {
        value
            .table()?
            .metatable_field::<&str>(c"text")?
            .map(Self)
            .ok_or_else(|| "missing metatable text".into())
    }
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
    fn unsigned_fields() -> UnsignedReply {
        UnsignedReply {
            small: 0,
            wide: u32::MAX,
        }
    }
    fn unsigned_values() -> (u8, u32) {
        (u8::MAX, 0)
    }

    async fn binary_echo(value: LuaBytes) -> Vec<u8> {
        tokio::task::yield_now().await;
        value.to_vec()
    }

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

    fn borrowed_after_callback(
        value: BorrowedOptions<'_>,
        callback: LuaFunction,
    ) -> (String, Vec<u8>) {
        let mut run = Box::pin(callback.call::<_, ()>(()));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            run.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        (
            value.text.to_owned(),
            value.data.unwrap_or_default().to_vec(),
        )
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

    fn invalid_container(element: bool) -> InvalidContainer {
        InvalidContainer(element)
    }

    fn borrowed_metatable_after_callback(value: MetaText<'_>, callback: LuaFunction) -> String {
        let mut run = Box::pin(callback.call::<_, ()>(()));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            run.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        value.0.to_owned()
    }
}

struct Plugin;

impl Module for Plugin {
    fn register(&self, machine: &mut Machine) -> Result<()> {
        machine.register::<Api>();
        machine.load("bridge plugin definitions", "bridge_ready = true")
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
        local integers=api.unsigned_fields()
        assert(integers.small==0 and integers.wide==4294967295)
        local small,wide=api.unsigned_values()
        assert(small==255 and wide==0)
        assert(type(api.empty({})) == 'table')
        assert(not pcall(api.empty, 'bad'))
        assert(api.borrowed('\229\144\140\230\173\165\229\128\159\231\148\168') == '\229\144\140\230\173\165\229\128\159\231\148\168')
        assert(api.passthrough('a\0b') == 'a\0b')
        assert(api.structured({text='\232\161\168\229\173\151\230\174\181\229\128\159\231\148\168'}) == '\232\161\168\229\173\151\230\174\181\229\128\159\231\148\168')
        assert(api.borrowed_identity('\229\142\159\230\160\183\232\191\148\229\155\158\229\128\159\231\148\168') == '\229\142\159\230\160\183\232\191\148\229\155\158\229\128\159\231\148\168')
        assert(api.borrowed_options({text='\231\187\147\230\158\132\228\189\147\229\128\159\231\148\168'}) == '\231\187\147\230\158\132\228\189\147\229\128\159\231\148\168')
        local target = {text=string.rep('\230\140\129\230\156\137',8192), data=string.rep('\0\255binary',8192)}
        local retained, binary = api.borrowed_after_callback(target, function()
            target.text = nil
            target.data = nil
            collectgarbage('collect')
            for i=1,32 do string.rep('\232\166\134\229\134\153',8192) end
            collectgarbage('collect')
        end)
        assert(retained == string.rep('\230\140\129\230\156\137',8192), 'borrow must remain valid after reentrant field deletion and GC')
        assert(binary == string.rep('\0\255binary',8192), 'binary field borrow must survive reentry and GC; length=' .. tostring(binary and #binary))
        local meta = {text=string.rep('\229\133\131\232\161\168\229\128\159\231\148\168',8192)}
        local container = setmetatable({}, meta)
        local retained_meta = api.borrowed_metatable_after_callback(container, function()
            meta.text = nil
            collectgarbage('collect')
            for i=1,32 do string.rep('\232\166\134\229\134\153',8192) end
        end)
        assert(retained_meta == string.rep('\229\133\131\232\161\168\229\128\159\231\148\168',8192))
        local gc = cosmos.thread.spawn(function()
            for i=1,10 do collectgarbage('collect'); cosmos.time.sleep(0) end
        end)
        for _, n in ipairs({8,4096,4097,8192}) do
            local binary = string.rep(string.char(0,128,255), math.floor(n/3)) .. string.rep(string.char(255), n%3)
            assert(api.binary_echo(binary) == binary, 'async binary arguments must not undergo text decoding')
            local reply = api.echo(string.rep('a',n)..'\0tail', {prefix='\229\137\141\231\188\128', ignored=true, [1]=false})
            assert(reply.len == n+5)
            assert(reply.text == '\229\137\141\231\188\128'..string.rep('a',n)..'\0tail')
        end
        assert(api.echo('\233\187\152\232\174\164').text == '\233\187\152\232\174\164')
        assert(gc:join() == nil)
        local next = api.cursor(2)
        assert(next() == string.char(0,255,1))
        collectgarbage('collect')
        assert(next() == string.char(0,255,0))
        assert(select('#', next()) == 1 and next() == nil)
        local ok, err = pcall(api.invalid_field)
        assert(not ok and err:find('table fields must produce exactly one value', 1, true))
        local ok, err = pcall(api.invalid_container, true)
        assert(not ok and err:find('array elements must produce exactly one value', 1, true))
        local ok, err = pcall(api.invalid_container, false)
        assert(not ok and err:find('metatable must be a table or nil', 1, true))
        assert(api.borrowed('\229\164\177\232\180\165\229\144\142\229\143\175\231\187\167\231\187\173\232\176\131\231\148\168') == '\229\164\177\232\180\165\229\144\142\229\143\175\231\187\167\231\187\173\232\176\131\231\148\168')
    "#)).unwrap();
}
