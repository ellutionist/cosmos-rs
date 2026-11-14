//! 从外部插件验证空值的转换、表字段与异步返回。

use cosmos::userdata::{FromLua, Null, Value, ValueKind};
use cosmos::{Machine, Module};

#[derive(cosmos::FromLua, cosmos::PushLua)]
struct Row {
    value: Null,
}

struct ClassifiedNull;

impl<'lua> FromLua<'lua> for ClassifiedNull {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        if value.kind() != ValueKind::Null {
            return Err(value.expected("cosmos.null"));
        }
        value.read::<Null>()?;
        Ok(Self)
    }
}

struct Api;

#[cosmos::userdata(name = "test.null")]
impl Api {
    fn roundtrip(row: Row) -> Row {
        row
    }

    async fn echo(value: Null) -> Null {
        tokio::task::yield_now().await;
        value
    }

    fn classify(_value: ClassifiedNull) -> bool {
        true
    }
}

struct Plugin;

impl Module for Plugin {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Api>();
        Ok(())
    }
}

#[test]
fn null_preserves_fields_identity_and_async_values() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Plugin).unwrap();
    runtime
        .block_on(machine.exec(
            r#"
        local null = cosmos.null
        assert(null ~= nil and null ~= false)
        assert(type(null) == "userdata")
        assert(require("cosmos").null == null)
        local row = cosmos.test.null.roundtrip({ value = null })
        assert(row.value == null and next(row) ~= nil)
        local array = { null, "value", null }
        assert(#array == 3 and array[1] == array[3])
        assert(cosmos.test.null.classify(null))
        assert(not pcall(cosmos.test.null.classify, {}))
        assert(not pcall(cosmos.test.null.echo, nil))
        assert(not pcall(cosmos.test.null.echo, false))
        assert(cosmos.test.null.echo(null) == null)
        local channel = cosmos.sync.channel.new(1)
        assert(channel:send(null))
        collectgarbage("collect")
        assert(channel:recv() == null and cosmos.null == null)
    "#,
        ))
        .unwrap();
}
