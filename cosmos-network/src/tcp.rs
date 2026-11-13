//! TCP 模块：连接状态枚举 + 读写流分离。
//!
//! 连接生命周期建模为 State 枚举（Connected/Closed）。connect 是阻塞式异步调用，
//! 返回时必然已连通或失败，因此没有可观察的 Connecting 中间态。
//! split() 把 TcpStream 拆成 reader/writer 两个独立 userdata（各占一个 RefCell），
//! 因此两个协程可以同时一个读一个写——共享的只有 Rc<Cell<State>> 连接状态。
//!
//! receive 语义对齐 OpenResty tcpsock:receive：
//!   receive()      等价 "*l"（缺省按行读）
//!   receive("*a")  读到 EOF（EOF 是正常终止，返回全部数据）
//!   receive("*l")  读一行（去 \n、忽略 \r），干净 EOF 返回 nil，提前关闭返回 nil,"closed",partial
//!   receive(n)     读精确 n 字节，提前关闭返回 nil,"closed",partial
//! 三值协议：data, err, partial，err 取 "closed" / "timeout"。
//! settimeout(ms) 对齐 OpenResty；ms <= 0 清除超时，超时不关闭连接。
//! connect(host, port, timeout_ms?) 的第三参可选，缺省无上限。
//! write 用 write_all 保证整块写完（不会因部分写而返回短长度）。
//! 关闭后的 reader/writer 再调用不 panic：receive 返回 nil,"closed"，write 返回 nil,"closed"。

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::Instant;

use cosmos::coroutine::AsyncOutput;
use cosmos::ffi::{
    c_char, c_int, lua_pushlstring, lua_pushnil, lua_State, lua_tointeger, lua_type,
    LUA_TNUMBER, LUA_TSTRING,
};
use cosmos::userdata::{FromLua, IntoOutput, LuaStr};

/// 可选 deadline 包裹一个 future：Some 时到点返回 Err(())（调用方转成 "timeout"），
/// None 时无限等待。用于 settimeout 生效后的各读写点。
async fn with_deadline<F, T>(deadline: Option<Instant>, fut: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(d) => tokio::time::timeout_at(d, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}

/// settimeout 的毫秒参数：<= 0 表示清除超时（None）
fn timeout_from_ms(ms: i64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms as u64))
}

/// 连接状态（Lua 侧 state() 返回对应的整数）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Connected = 0,
    Closed = 1,
}

/// 连接入口：静态方法 Tcp.connect(host, port, timeout_ms?)
pub struct Tcp;

#[cosmos::userdata]
impl Tcp {
    /// 连接失败或超时返回 nil + 错误串（local conn, err = Tcp.connect(...)）；
    /// timeout_ms 可选：<= 0 或缺省表示不设连接超时
    async fn connect(host: String, port: i64, timeout_ms: Option<i64>) -> Result<TcpConn, String> {
        let deadline = timeout_ms.and_then(timeout_from_ms).map(|d| Instant::now() + d);
        match with_deadline(deadline, TcpStream::connect((host.as_str(), port as u16))).await {
            Ok(Ok(stream)) => Ok(TcpConn {
                stream: Some(stream),
                state: Rc::new(Cell::new(State::Connected)),
                timeout: None,
            }),
            Ok(Err(e)) => Err(e.to_string()),
            Err(()) => Err("timeout".to_string()),
        }
    }
}

pub struct TcpConn {
    stream: Option<TcpStream>,
    state: Rc<Cell<State>>,
    /// 读写超时；split 时传递给 reader/writer（对齐 OpenResty settimeout）
    timeout: Option<Duration>,
}

#[cosmos::userdata]
impl TcpConn {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 设置读写超时（毫秒）；ms <= 0 表示不超时
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 拆成 reader/writer 两个 userdata（共享连接状态），
    /// 之后连接本体不再可用，读写由两半各自进行
    fn split(&mut self) -> (TcpReader, TcpWriter) {
        let stream = self.stream.take().expect("connection already split");
        let (r, w) = stream.into_split();
        (
            TcpReader {
                inner: Some(BufReader::new(r)),
                state: self.state.clone(),
                buf: Vec::new(),
                timeout: self.timeout,
            },
            TcpWriter {
                inner: Some(w),
                state: self.state.clone(),
                timeout: self.timeout,
            },
        )
    }

    fn close(&mut self) {
        self.stream.take(); // drop → 关连接
        self.state.set(State::Closed);
    }
}

/// receive 的读取模式（OpenResty tcpsock:receive 的参数）
pub enum ReceivePattern {
    All,          // "*a"
    Line,         // "*l"
    Size(usize),  // 数字：精确 n 字节
}

impl FromLua for ReceivePattern {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        match lua_type(L, idx) {
            LUA_TNUMBER => Ok(ReceivePattern::Size(lua_tointeger(L, idx).max(0) as usize)),
            LUA_TSTRING => {
                let s = <String as FromLua>::from_lua(L, idx)?;
                match s.as_str() {
                    "*a" => Ok(ReceivePattern::All),
                    "*l" => Ok(ReceivePattern::Line),
                    _ => Err(format!(
                        "unsupported receive pattern '{s}' (expected '*l', '*a' or a number)"
                    )),
                }
            }
            _ => Err(format!("argument {idx}: expected number or '*l'/'*a'")),
        }
    }
}

/// receive 的结果：data, err, partial 三值协议（data/partial 为原始字节）
pub enum ReceiveOutcome {
    Data(Vec<u8>),           // 1 值：数据（原始字节）
    Eof,                     // 1 值：nil（干净关闭）
    Closed(Vec<u8>),         // 3 值：nil, "closed", partial
    Timeout(Vec<u8>),        // 3 值：nil, "timeout", partial
    Error(String, Vec<u8>),  // 3 值：nil, err, partial
}

impl IntoOutput for ReceiveOutcome {
    fn into_output(self) -> AsyncOutput {
        match self {
            // 原始字节直接压栈（Lua 字符串是字节串，不做有损 UTF-8 转换）
            ReceiveOutcome::Data(bytes) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushlstring(L, bytes.as_ptr() as *const c_char, bytes.len());
                1
            })),
            ReceiveOutcome::Eof => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushnil(L);
                1
            })),
            ReceiveOutcome::Closed(partial) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushnil(L);
                lua_pushlstring(L, c"closed".as_ptr(), 6);
                lua_pushlstring(L, partial.as_ptr() as *const c_char, partial.len());
                3
            })),
            ReceiveOutcome::Timeout(partial) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushnil(L);
                lua_pushlstring(L, c"timeout".as_ptr(), 7);
                lua_pushlstring(L, partial.as_ptr() as *const c_char, partial.len());
                3
            })),
            ReceiveOutcome::Error(msg, partial) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushnil(L);
                lua_pushlstring(L, msg.as_ptr() as *const c_char, msg.len());
                lua_pushlstring(L, partial.as_ptr() as *const c_char, partial.len());
                3
            })),
        }
    }
}

pub struct TcpReader {
    inner: Option<BufReader<OwnedReadHalf>>,
    state: Rc<Cell<State>>,
    /// 内置读缓冲：随数据增长，每次 receive 复用，读完 mem::take 零拷贝移出
    buf: Vec<u8>,
    timeout: Option<Duration>,
}

#[cosmos::userdata]
impl TcpReader {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 设置读超时（毫秒）；ms <= 0 表示不超时
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 对齐 OpenResty tcpsock:receive：local data, err, partial = reader:receive(pattern?)
    /// pattern 缺省（或 nil）等价 "*l"
    async fn receive(&mut self, pattern: Option<ReceivePattern>) -> ReceiveOutcome {
        // 已关闭：不再 panic，按 closed 协议返回
        let Some(inner) = self.inner.as_mut() else {
            return ReceiveOutcome::Closed(Vec::new());
        };
        // 每次 receive 一个 deadline（绝对时刻），Size 模式多次 read 共用同一 deadline
        let deadline = self.timeout.map(|d| Instant::now() + d);
        match pattern.unwrap_or(ReceivePattern::Line) {
            ReceivePattern::All => {
                self.buf.clear();
                match with_deadline(deadline, inner.read_to_end(&mut self.buf)).await {
                    Ok(Ok(_)) => {
                        // EOF 是 *a 的正常终止
                        self.state.set(State::Closed);
                        ReceiveOutcome::Data(std::mem::take(&mut self.buf))
                    }
                    Ok(Err(e)) => {
                        self.state.set(State::Closed);
                        ReceiveOutcome::Error(e.to_string(), std::mem::take(&mut self.buf))
                    }
                    // 超时不断连接，已读部分作为 partial 交回
                    Err(()) => ReceiveOutcome::Timeout(std::mem::take(&mut self.buf)),
                }
            }
            ReceivePattern::Line => {
                self.buf.clear();
                match with_deadline(deadline, inner.read_until(b'\n', &mut self.buf)).await {
                    Ok(Ok(0)) => {
                        // 干净 EOF：无数据可读
                        self.state.set(State::Closed);
                        ReceiveOutcome::Eof
                    }
                    Ok(Ok(_)) => {
                        if self.buf.last() == Some(&b'\n') {
                            self.buf.pop();
                            self.buf.retain(|&b| b != b'\r'); // OpenResty：所有 CR 忽略
                            ReceiveOutcome::Data(std::mem::take(&mut self.buf))
                        } else {
                            // 提前关闭：没等到 \n 就 EOF
                            self.state.set(State::Closed);
                            ReceiveOutcome::Closed(std::mem::take(&mut self.buf))
                        }
                    }
                    Ok(Err(e)) => {
                        self.state.set(State::Closed);
                        ReceiveOutcome::Error(e.to_string(), std::mem::take(&mut self.buf))
                    }
                    Err(()) => ReceiveOutcome::Timeout(std::mem::take(&mut self.buf)),
                }
            }
            ReceivePattern::Size(n) => {
                let n = n.max(0) as usize;
                self.buf.resize(n, 0); // 用时扩展
                let mut filled = 0usize;
                while filled < n {
                    match read_chunk(deadline, inner, &mut self.buf[filled..]).await {
                        Ok(0) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Closed(std::mem::take(&mut self.buf));
                        }
                        Ok(nread) => filled += nread,
                        Err(ChunkError::Io(e)) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Error(
                                e.to_string(),
                                std::mem::take(&mut self.buf),
                            );
                        }
                        Err(ChunkError::Timeout) => {
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Timeout(std::mem::take(&mut self.buf));
                        }
                    }
                }
                ReceiveOutcome::Data(std::mem::take(&mut self.buf))
            }
        }
    }

    fn close(&mut self) {
        self.inner.take(); // drop → shutdown 读半
        self.state.set(State::Closed);
    }
}

enum ChunkError {
    Io(std::io::Error),
    Timeout,
}

/// 单次 read，带可选 deadline；Size 模式攒够 n 字节前反复调用（共用同一 deadline）
async fn read_chunk(
    deadline: Option<Instant>,
    inner: &mut BufReader<OwnedReadHalf>,
    buf: &mut [u8],
) -> Result<usize, ChunkError> {
    match with_deadline(deadline, inner.read(buf)).await {
        Ok(Ok(n)) => Ok(n),
        Ok(Err(e)) => Err(ChunkError::Io(e)),
        Err(()) => Err(ChunkError::Timeout),
    }
}

pub struct TcpWriter {
    inner: Option<OwnedWriteHalf>,
    state: Rc<Cell<State>>,
    timeout: Option<Duration>,
}

#[cosmos::userdata]
impl TcpWriter {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 设置写超时（毫秒）；ms <= 0 表示不超时
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 整块写完（write_all），返回写入字节数；出错/超时返回 nil + 错误串。
    /// data 用 LuaStr：短串拷贝、长串锚定（见 cosmos::userdata::LuaStr）
    async fn write(&mut self, data: LuaStr) -> Result<i64, String> {
        let Some(inner) = self.inner.as_mut() else {
            return Err("closed".to_string());
        };
        let bytes = data.as_bytes();
        let deadline = self.timeout.map(|d| Instant::now() + d);
        match with_deadline(deadline, inner.write_all(bytes)).await {
            Ok(Ok(())) => Ok(bytes.len() as i64),
            Ok(Err(e)) => {
                self.state.set(State::Closed);
                Err(e.to_string())
            }
            // 超时不断连接：write_all 未完成的部分可能已部分写出
            Err(()) => Err("timeout".to_string()),
        }
    }

    fn close(&mut self) {
        self.inner.take(); // drop → shutdown 写半（对端读会收到 EOF）
        self.state.set(State::Closed);
    }
}

pub unsafe fn register(L: *mut lua_State) {
    cosmos::userdata::register::<Tcp>(L);
    cosmos::userdata::register::<TcpConn>(L);
    cosmos::userdata::register::<TcpReader>(L);
    cosmos::userdata::register::<TcpWriter>(L);
}
