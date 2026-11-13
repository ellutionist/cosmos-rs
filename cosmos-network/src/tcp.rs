//! TCP 模块：连接状态枚举 + 读写流分离。
//!
//! 连接生命周期建模为 State 枚举（Connected/Closed）。connect 是阻塞式异步调用，
//! 返回时必然已连通或失败，因此没有可观察的 Connecting 中间态。
//! split() 把 TcpStream 拆成 reader/writer 两个独立 userdata（各占一个 RefCell），
//! 因此两个协程可以同时一个读一个写——共享的只有 Rc<Cell<State>> 连接状态。
//!
//! receive 语义对齐 OpenResty tcpsock:receive（'*' 前缀可省略，同 LuaSocket）：
//!   receive()      等价 "*l"（缺省按行读）
//!   receive("*a")  读到 EOF（EOF 是正常终止，返回全部数据）
//!   receive("*l")  读一行（去行尾 \n 与 \r），干净 EOF 返回 nil，提前关闭返回 nil,"closed",partial
//!   receive("*L")  读一行，保留行尾 \n（含可能的前置 \r）
//!   receive(n)     读精确 n 字节，提前关闭返回 nil,"closed",partial
//! receiveuntil(delim) 读到分隔符（不含），分隔符被消费；可跨 BufReader 分块边界匹配。
//! 三值协议：data, err, partial，err 取 "closed" / "timeout" / "buffer overflow"。
//! settimeout(ms) 对齐 OpenResty；ms <= 0 清除超时，超时不关闭连接。
//! setbufsize(n) 限制 *a / *l / *L / receiveuntil 的累积字节数；n <= 0 解除，
//!   超限返回 nil,"buffer overflow",partial，未读数据留在流里；receive(n) 不受限。
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

/// setbufsize 的字节参数：<= 0 表示不限（None）
fn size_limit(n: i64) -> Option<usize> {
    (n > 0).then_some(n as usize)
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
                max_buf: None,
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
    /// *a/*l/*L/receiveuntil 累积上限；split 时传递给 reader
    max_buf: Option<usize>,
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

    /// 设置读取缓冲上限（字节，*a/*l/*L/receiveuntil 生效）；n <= 0 不限
    fn setbufsize(&mut self, n: i64) {
        self.max_buf = size_limit(n);
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
                max_buf: self.max_buf,
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

/// receive 的读取模式（OpenResty tcpsock:receive 的参数，'*' 前缀可省略）
pub enum ReceivePattern {
    All,          // "*a"
    Line,         // "*l"：去掉行尾 \n 与 \r
    LineKeep,     // "*L"：保留行尾 \n
    Size(usize),  // 数字：精确 n 字节
}

impl FromLua for ReceivePattern {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        match lua_type(L, idx) {
            LUA_TNUMBER => Ok(ReceivePattern::Size(lua_tointeger(L, idx).max(0) as usize)),
            LUA_TSTRING => {
                let s = <String as FromLua>::from_lua(L, idx)?;
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
            _ => Err(format!("argument {idx}: expected number or '*l'/'*L'/'*a'")),
        }
    }
}

/// receive 的结果：data, err, partial 三值协议（data/partial 为原始字节）
pub enum ReceiveOutcome {
    Data(Vec<u8>),           // 1 值：数据（原始字节）
    Eof,                     // 1 值：nil（干净关闭）
    Closed(Vec<u8>),         // 3 值：nil, "closed", partial
    Timeout(Vec<u8>),        // 3 值：nil, "timeout", partial
    Overflow(Vec<u8>),       // 3 值：nil, "buffer overflow", partial
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
            ReceiveOutcome::Overflow(partial) => AsyncOutput::Custom(Box::new(move |L| unsafe {
                lua_pushnil(L);
                lua_pushlstring(L, c"buffer overflow".as_ptr(), 15);
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
    /// 内置读缓冲：跨 fill 复用，返回时 mem::take 零拷贝移出
    buf: Vec<u8>,
    timeout: Option<Duration>,
    /// *a/*l/*L/receiveuntil 的累积上限；None 不限
    max_buf: Option<usize>,
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

    /// 设置读取缓冲上限（字节，*a/*l/*L/receiveuntil 生效）；n <= 0 不限
    fn setbufsize(&mut self, n: i64) {
        self.max_buf = size_limit(n);
    }

    /// 对齐 OpenResty tcpsock:receive：local data, err, partial = reader:receive(pattern?)
    /// pattern 缺省（或 nil）等价 "*l"
    async fn receive(&mut self, pattern: Option<ReceivePattern>) -> ReceiveOutcome {
        // 已关闭：不再 panic，按 closed 协议返回
        let Some(inner) = self.inner.as_mut() else {
            return ReceiveOutcome::Closed(Vec::new());
        };
        // 每次 receive 一个 deadline（绝对时刻），分块读取共用同一 deadline
        let deadline = self.timeout.map(|d| Instant::now() + d);
        let cap = self.max_buf;
        match pattern.unwrap_or(ReceivePattern::Line) {
            ReceivePattern::All => {
                self.buf.clear();
                loop {
                    let (take, got, eof) = {
                        let avail = match fill(deadline, inner).await {
                            Ok(b) => b,
                            Err(FillError::Io(e)) => {
                                self.state.set(State::Closed);
                                return ReceiveOutcome::Error(
                                    e.to_string(),
                                    std::mem::take(&mut self.buf),
                                );
                            }
                            Err(FillError::Timeout) => {
                                return ReceiveOutcome::Timeout(std::mem::take(&mut self.buf));
                            }
                        };
                        let got = avail.len();
                        let take =
                            cap.map_or(got, |c| got.min(c.saturating_sub(self.buf.len())));
                        self.buf.extend_from_slice(&avail[..take]);
                        (take, got, got == 0)
                    };
                    if eof {
                        // EOF 是 *a 的正常终止
                        self.state.set(State::Closed);
                        return ReceiveOutcome::Data(std::mem::take(&mut self.buf));
                    }
                    inner.consume(take);
                    if take < got {
                        return ReceiveOutcome::Overflow(std::mem::take(&mut self.buf));
                    }
                }
            }
            pat @ (ReceivePattern::Line | ReceivePattern::LineKeep) => {
                let keep_eol = matches!(pat, ReceivePattern::LineKeep);
                self.buf.clear();
                loop {
                    let (need, allowed, has_nl) = {
                        let avail = match fill(deadline, inner).await {
                            Ok(b) => b,
                            Err(FillError::Io(e)) => {
                                self.state.set(State::Closed);
                                return ReceiveOutcome::Error(
                                    e.to_string(),
                                    std::mem::take(&mut self.buf),
                                );
                            }
                            Err(FillError::Timeout) => {
                                return ReceiveOutcome::Timeout(std::mem::take(&mut self.buf));
                            }
                        };
                        if avail.is_empty() {
                            // EOF：干净关闭返回 nil，残行按 closed 交回
                            self.state.set(State::Closed);
                            return if self.buf.is_empty() {
                                ReceiveOutcome::Eof
                            } else {
                                ReceiveOutcome::Closed(std::mem::take(&mut self.buf))
                            };
                        }
                        let nl = avail.iter().position(|&b| b == b'\n');
                        let want = nl.unwrap_or(avail.len());
                        // *L 连行尾 \n 一起收下（\r 本就在 want 范围内）
                        let need = want + usize::from(nl.is_some() && keep_eol);
                        let allowed =
                            cap.map_or(need, |c| need.min(c.saturating_sub(self.buf.len())));
                        self.buf.extend_from_slice(&avail[..allowed]);
                        (need, allowed, nl.is_some())
                    };
                    let done = has_nl && allowed == need;
                    // done 且 *l：行尾 \n 还在 avail 里，消费时带上
                    inner.consume(if done && !keep_eol { allowed + 1 } else { allowed });
                    if done {
                        if !keep_eol {
                            // 去掉行尾 \n 与可能的前置 \r（与 C++ ReadLine 一致）
                            while matches!(self.buf.last(), Some(b'\r' | b'\n')) {
                                self.buf.pop();
                            }
                        }
                        return ReceiveOutcome::Data(std::mem::take(&mut self.buf));
                    }
                    if allowed < need {
                        return ReceiveOutcome::Overflow(std::mem::take(&mut self.buf));
                    }
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

    /// receiveuntil(delim)：读到分隔符为止，返回分隔符之前的数据（分隔符被消费）。
    /// 分隔符可跨 BufReader 分块边界；EOF/超时/超限返回 nil + err + partial。
    async fn receiveuntil(&mut self, delim: String) -> ReceiveOutcome {
        let Some(inner) = self.inner.as_mut() else {
            return ReceiveOutcome::Closed(Vec::new());
        };
        if delim.is_empty() {
            return ReceiveOutcome::Error("empty delimiter".to_string(), Vec::new());
        }
        let delim = delim.into_bytes();
        let d = delim.len();
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let cap = self.max_buf;
        self.buf.clear();
        loop {
            // take：从本次 avail 消费的字节数；cut：Some 时 buf 截断到该位置（已见分隔符）
            let (take, cut) = {
                let avail = match fill(deadline, inner).await {
                    Ok(b) => b,
                    Err(FillError::Io(e)) => {
                        self.state.set(State::Closed);
                        return ReceiveOutcome::Error(e.to_string(), std::mem::take(&mut self.buf));
                    }
                    Err(FillError::Timeout) => {
                        return ReceiveOutcome::Timeout(std::mem::take(&mut self.buf));
                    }
                };
                if avail.is_empty() {
                    // EOF 前没等到分隔符：提前关闭
                    self.state.set(State::Closed);
                    return ReceiveOutcome::Closed(std::mem::take(&mut self.buf));
                }
                // 1) 跨边界匹配：分隔符起始于 buf 末尾 d-1 字节内，剩余部分在 avail
                let lo = self.buf.len().saturating_sub(d - 1);
                let mut found = None;
                for start in lo..self.buf.len() {
                    let have = self.buf.len() - start;
                    let need = d - have;
                    if avail.len() >= need
                        && self.buf[start..] == delim[..have]
                        && avail[..need] == delim[have..]
                    {
                        found = Some((start, need));
                        break;
                    }
                }
                if let Some((start, need)) = found {
                    self.buf.extend_from_slice(&avail[..need]);
                    (need, Some(start))
                } else if let Some(p) = find_subslice(avail, &delim) {
                    // 2) 分隔符完整落在 avail 内
                    self.buf.extend_from_slice(&avail[..p]);
                    (p + d, Some(self.buf.len()))
                } else {
                    // 3) 还没有分隔符：受上限约束地收下
                    let take = cap
                        .map_or(avail.len(), |c| avail.len().min(c.saturating_sub(self.buf.len())));
                    self.buf.extend_from_slice(&avail[..take]);
                    (take, None)
                }
            };
            inner.consume(take);
            if let Some(cut) = cut {
                self.buf.truncate(cut);
                return ReceiveOutcome::Data(std::mem::take(&mut self.buf));
            }
            if cap.is_some_and(|c| self.buf.len() >= c) {
                return ReceiveOutcome::Overflow(std::mem::take(&mut self.buf));
            }
        }
    }

    fn close(&mut self) {
        self.inner.take(); // drop → shutdown 读半
        self.state.set(State::Closed);
    }
}

enum FillError {
    Io(std::io::Error),
    Timeout,
}

/// 取 BufReader 内部缓冲的下一段（空切片 = EOF）；deadline 到点返回 Timeout。
/// 只借用不消费：调用方按需 extend 到自己的 buf 后再 consume。
async fn fill<'a>(
    deadline: Option<Instant>,
    inner: &'a mut BufReader<OwnedReadHalf>,
) -> Result<&'a [u8], FillError> {
    match with_deadline(deadline, inner.fill_buf()).await {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(e)) => Err(FillError::Io(e)),
        Err(()) => Err(FillError::Timeout),
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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
