//! cosmos-network 的 Lua 桥接层。
//!
//! 职责仅限本 crate 无法下沉到 cosmos 的叶子转换：
//! - 本 crate 自有类型的 `FromLua` / `PushLua`（Headers、LuaBytes、ReceivePattern 等），
//!   实现里只组合 cosmos 的基层原语，不写多值压栈逻辑；
//! - TCP 迭代器状态与通用 Rust 调用桥接的组合；
//! - 全部 userdata 类型注册。
//!
//! 多返回值/表结构一律由 `#[derive(cosmos::PushLua)]` 与 `&str` / `Vec<u8>` /
//! `Nil` / `StringPairs` 等 cosmos 基层类型组合而成，功能模块只构造值。

use cosmos::userdata::{
    Call, FromLua, LuaCallable, LuaClosure, Output, PushLua, StringPairs, Value,
};

use crate::http::body::Body;
use crate::http::client::HttpClient;
use crate::http::file::HttpFile;
use crate::http::server::{HttpRequest, HttpServer, HttpServerHandle};
use crate::http::{Headers, LuaBytes};
use crate::socket::{Socket, SocketListener};
use crate::tcp::stream::{
    ReceivePattern, Tcp, TcpConn, TcpListener, TcpReader, TcpUntil, TcpWriter, UntilIter,
};
use crate::tcp::tls::{ClientCfg, ServerCfg, Tls, TlsConn, TlsContext, TlsListener};
use crate::udp::Udp;
use crate::websocket::conn::WsConn;
use crate::websocket::Ws;

// ---------------------------------------------------------------------------
// HTTP：Headers / LuaBytes
// ---------------------------------------------------------------------------

impl<'lua> FromLua<'lua> for Headers {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let pairs = value.read::<StringPairs>()?;
        Ok(Headers(
            pairs
                .0
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
        ))
    }
}

impl PushLua for Headers {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        StringPairs(self.0).push_lua(output)
    }
}

/// Bytes ↔ Lua 字符串（持有 Bytes 零拷贝，push 时拷进 Lua）
impl PushLua for LuaBytes {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        self.0.as_ref().push_lua(output)
    }
}

impl<'lua> FromLua<'lua> for LuaBytes {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let bytes = value.read::<Vec<u8>>()?;
        Ok(LuaBytes(bytes.into()))
    }
}

// ---------------------------------------------------------------------------
// TCP：receive 参数与迭代器闭包
// ---------------------------------------------------------------------------

#[derive(cosmos::FromLua)]
enum ReceiveArg {
    #[lua(type = "number")]
    Size(i64),
    #[lua(type = "string")]
    Pattern(String),
}

impl<'lua> FromLua<'lua> for ReceivePattern {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        match value.read::<ReceiveArg>()? {
            ReceiveArg::Size(size) => Ok(ReceivePattern::Size(size.max(0) as usize)),
            ReceiveArg::Pattern(s) => {
                let pat = s.strip_prefix('*').unwrap_or(&s);
                match pat {
                    "a" => Ok(ReceivePattern::All),
                    "l" => Ok(ReceivePattern::Line),
                    "L" => Ok(ReceivePattern::LineKeep),
                    _ => Err(format!(
                        "unsupported receive pattern '{s}' (expected '*l', '*L', '*a' or a number)"
                    )),
                }
            }
        }
    }
}

/// receiveuntil 返回的迭代器：压栈时构造 TcpUntil userdata 并包成 Lua 闭包
/// （upvalue = userdata）。Lua 用法：local iter = reader:receiveuntil("--"); iter()
impl PushLua for UntilIter {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        LuaClosure(self.0).push_lua(output)
    }
}

// 返回值经 suspend 生成，捕获守卫仅由当前挂起操作持有。
unsafe impl LuaCallable for TcpUntil {
    fn call(call: &mut Call<'_>) -> Result<i32, String> {
        let mut state = unsafe { call.capture().borrow_mut::<Self>()? };
        Ok(call.suspend(async move { state.next().await }))
    }
}

// ---------------------------------------------------------------------------
// TLS：ctx 参数
// ---------------------------------------------------------------------------

impl<'lua> FromLua<'lua> for ClientCfg {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let mut ctx = value.borrow_mut::<TlsContext>()?;
        ctx.client_config().map(ClientCfg)
    }
}

impl<'lua> FromLua<'lua> for ServerCfg {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        let mut ctx = value.borrow_mut::<TlsContext>()?;
        ctx.server_config().map(ServerCfg)
    }
}

// ---------------------------------------------------------------------------
// 类型注册
// ---------------------------------------------------------------------------

/// 注册全部网络 userdata 类型。
pub(crate) fn register(machine: &mut cosmos::Machine) {
    machine.register::<Udp>();
    machine.register::<Socket>();
    machine.register::<SocketListener>();
    machine.register::<Tcp>();
    machine.register::<TcpConn>();
    machine.register::<TcpReader>();
    machine.register::<TcpWriter>();
    machine.register::<TcpUntil>();
    machine.register::<TcpListener>();

    machine.register::<Tls>();
    machine.register::<TlsContext>();
    machine.register::<TlsConn>();
    machine.register::<TlsListener>();

    machine.register::<Body>();
    machine.register::<HttpFile>();
    machine.register::<crate::http::url::HttpUrl>();
    machine.register::<HttpClient>();
    machine.register::<HttpServer>();
    machine.register::<HttpServerHandle>();
    machine.register::<HttpRequest>();

    machine.register::<Ws>();
    machine.register::<WsConn>();
}

// ---------------------------------------------------------------------------
// URL：有序列表保留重复查询键，拒绝稀疏/循环输入，不保存字段视图。
// ---------------------------------------------------------------------------
pub(crate) trait UrlItemSize {
    fn url_bytes(&self) -> usize;
}
impl UrlItemSize for cosmos::userdata::LuaBytes {
    fn url_bytes(&self) -> usize {
        self.len()
    }
}
pub(crate) struct UrlItems<T>(pub(crate) Result<Vec<T>, String>);
pub(crate) struct UrlList<T>(pub(crate) Vec<T>);
impl<'lua, T: FromLua<'lua> + UrlItemSize> FromLua<'lua> for UrlItems<T> {
    unsafe fn from_lua(value: Value<'lua>) -> Result<Self, String> {
        use crate::http::url::{MAX_URL_BYTES, MAX_URL_ITEMS};
        use cosmos::userdata::ValueKind;
        let parsed = (|| {
            let mut items = std::collections::BTreeMap::new();
            let mut bytes = 0usize;
            value.table()?.entries(|key, value| {
                if items.len() == MAX_URL_ITEMS {
                    return Err("URL sequence exceeds 1024 items".into());
                }
                if key.kind() != ValueKind::Number {
                    return Err("URL sequence requires consecutive integer keys".into());
                }
                let index = key.read::<f64>()?;
                if !index.is_finite()
                    || index.fract() != 0.0
                    || !(1.0..=MAX_URL_ITEMS as f64).contains(&index)
                {
                    return Err("URL sequence requires consecutive integer keys".into());
                }
                let item = value.read::<T>()?;
                bytes = bytes.saturating_add(item.url_bytes());
                if bytes > MAX_URL_BYTES {
                    return Err("URL sequence input exceeds 64 KiB".into());
                }
                items.insert(index as usize, item);
                Ok(())
            })?;
            if items.keys().copied().ne(1..=items.len()) {
                return Err("URL sequence must not contain holes".into());
            }
            Ok(items.into_values().collect())
        })();
        Ok(Self(parsed))
    }
}
impl<T: PushLua> PushLua for UrlList<T> {
    unsafe fn push_lua(self, output: &mut Output<'_>) -> i32 {
        let mut table = output.array(self.0.len());
        for (index, value) in self.0.into_iter().enumerate() {
            table.index(index + 1, value);
        }
        table.finish()
    }
}
