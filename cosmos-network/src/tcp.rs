//! TCP 模块：连接状态枚举 + 读写流分离。
//!
//! 连接生命周期建模为 State 枚举（Connecting/Connected/Closed）。
//! split() 把 TcpStream 拆成 reader/writer 两个独立 userdata（各占一个 RefCell），
//! 因此两个协程可以同时一个读一个写——共享的只有 Rc<Cell<State>> 连接状态。
//!
//! receive 语义对齐 OpenResty tcpsock:receive：
//!   receive("*a")  读到 EOF（EOF 是正常终止，返回全部数据）
//!   receive("*l")  读一行（去 \n、忽略 \r），干净 EOF 返回 nil，提前关闭返回 nil,"closed",partial
//!   receive(n)     读精确 n 字节，提前关闭返回 nil,"closed",partial
//! 统一三值协议：data, err, partial

use std::cell::Cell;
use std::rc::Rc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use cosmos::coroutine::AsyncOutput;
use cosmos::ffi::{
    c_char, c_int, lua_pushlstring, lua_pushnil, lua_State, lua_tointeger, lua_type,
    LUA_TNUMBER, LUA_TSTRING,
};
use cosmos::userdata::{FromLua, IntoOutput, LuaStr};

/// 连接状态（Lua 侧 state() 返回对应的整数）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Connecting = 0,
    Connected = 1,
    Closed = 2,
}

/// 连接入口：静态方法 Tcp.connect(host, port)
pub struct Tcp;

#[cosmos::userdata]
impl Tcp {
    /// 连接失败返回 nil + 错误串（标准 Lua 形态：local conn, err = Tcp.connect(...)）
    async fn connect(host: String, port: i64) -> Result<TcpConn, String> {
        match TcpStream::connect((host.as_str(), port as u16)).await {
            Ok(stream) => Ok(TcpConn {
                stream: Some(stream),
                state: Rc::new(Cell::new(State::Connected)),
            }),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub struct TcpConn {
    stream: Option<TcpStream>,
    state: Rc<Cell<State>>,
}

#[cosmos::userdata]
impl TcpConn {
    fn state(&self) -> i64 {
        self.state.get() as i64
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
            },
            TcpWriter {
                inner: Some(w),
                state: self.state.clone(),
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
}

#[cosmos::userdata]
impl TcpReader {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 对齐 OpenResty tcpsock:receive：local data, err, partial = reader:receive(pattern)
    async fn receive(&mut self, pattern: ReceivePattern) -> ReceiveOutcome {
        let inner = self.inner.as_mut().expect("reader already closed");
        match pattern {
            ReceivePattern::All => {
                self.buf.clear();
                match inner.read_to_end(&mut self.buf).await {
                    Ok(_) => {
                        // EOF 是 *a 的正常终止
                        self.state.set(State::Closed);
                        ReceiveOutcome::Data(std::mem::take(&mut self.buf))
                    }
                    Err(e) => {
                        self.state.set(State::Closed);
                        ReceiveOutcome::Error(e.to_string(), std::mem::take(&mut self.buf))
                    }
                }
            }
            ReceivePattern::Line => {
                self.buf.clear();
                match inner.read_until(b'\n', &mut self.buf).await {
                    Ok(0) => {
                        // 干净 EOF：无数据可读
                        self.state.set(State::Closed);
                        ReceiveOutcome::Eof
                    }
                    Ok(_) => {
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
                    Err(e) => {
                        self.state.set(State::Closed);
                        ReceiveOutcome::Error(e.to_string(), std::mem::take(&mut self.buf))
                    }
                }
            }
            ReceivePattern::Size(n) => {
                let n = n.max(0) as usize;
                self.buf.resize(n, 0); // 用时扩展
                let mut filled = 0usize;
                while filled < n {
                    match inner.read(&mut self.buf[filled..]).await {
                        Ok(0) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Closed(std::mem::take(&mut self.buf));
                        }
                        Ok(nread) => filled += nread,
                        Err(e) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Error(
                                e.to_string(),
                                std::mem::take(&mut self.buf),
                            );
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

pub struct TcpWriter {
    inner: Option<OwnedWriteHalf>,
    state: Rc<Cell<State>>,
}

#[cosmos::userdata]
impl TcpWriter {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 写一块数据，返回实际写入字节数；出错返回 nil + 错误串并置状态 Closed。
    /// data 用 LuaStr：短串拷贝、长串锚定（见 cosmos::userdata::LuaStr）
    async fn write(&mut self, data: LuaStr) -> Result<i64, String> {
        let inner = self.inner.as_mut().expect("writer already closed");
        match inner.write(data.as_bytes()).await {
            Ok(n) => Ok(n as i64),
            Err(e) => {
                self.state.set(State::Closed);
                Err(e.to_string())
            }
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
