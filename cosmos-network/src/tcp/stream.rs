//! TCP stream 模块：连接状态枚举 + 读写流分离。
//!
//! Lua: local Tcp = require("cosmos.network.tcp.stream")（C++ 的 cosmos.network.tcp）
//!      命名空间入口：local net = require("cosmos.network.tcp")  -- { stream, tls }
//!
//! 连接生命周期建模为 State 枚举（Connected/Closed）。connect 是阻塞式异步调用，
//! 返回时必然已连通或失败，因此没有可观察的 Connecting 中间态。
//! split() 把 TcpStream 拆成 reader/writer 两个独立 userdata（各占一个 RefCell），
//! 因此两个协程可以同时一个读一个写——共享的只有 `Rc<Cell<State>>` 连接状态。
//!
//! receive 语义对齐 OpenResty tcpsock:receive（'*' 前缀可省略，同 LuaSocket）：
//!   receive()      等价 "*l"（缺省按行读）
//!   receive("*a")  读到 EOF（EOF 是正常终止，返回全部数据）
//!   receive("*l")  读一行（去行尾 \n 与 \r），干净 EOF 返回 nil，提前关闭返回 nil,"closed",partial
//!   receive("*L")  读一行，保留行尾 \n（含可能的前置 \r）
//!   receive(n)     读精确 n 字节，提前关闭返回 nil,"closed",partial
//! receiveuntil(delim) 返回迭代器闭包：每次调用读到分隔符（不含，已消费），
//!   可跨 BufReader 分块边界匹配；迭代器独占读取流，原 reader 不可再用。
//! 三值协议：data, err, partial，err 取 "closed" / "timeout" / "buffer overflow"。
//! settimeout(ms) 对齐 OpenResty；ms <= 0 清除超时，超时不关闭连接。
//! setbufsize(n) 限制 *a / *l / *L / receiveuntil 的累积字节数；n <= 0 解除，
//!   超限返回 nil,"buffer overflow",partial，未读数据留在流里；receive(n) 不受限。
//! connect(host, port, timeout_ms?) 的第三参可选，缺省无上限。
//! write 用 write_all 保证整块写完（不会因部分写而返回短长度）。
//! 关闭后的 reader/writer 再调用不 panic：receive 返回 nil,"closed"，write 返回 nil,"closed"。

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::Instant;

use cosmos::userdata::{LuaBytes, Nil};

use super::tls::TlsIo;
use crate::error::classify_io;

/// 可选 deadline 包裹一个 future：Some 时到点返回 Err(())（调用方转成 "timeout"），
/// None 时无限等待。用于 settimeout 生效后的各读写点。
pub(crate) async fn with_deadline<F, T>(deadline: Option<Instant>, fut: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(d) => tokio::time::timeout_at(d, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}

/// settimeout 的毫秒参数：<= 0 表示清除超时（None）
pub(crate) fn timeout_from_ms(ms: i64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms as u64))
}

/// setbufsize 的字节参数：<= 0 表示不限（None）
pub(super) fn size_limit(n: i64) -> Option<usize> {
    (n > 0).then_some(n as usize)
}

/// 连接状态（Lua 侧 state() 返回对应的整数）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Connected = 0,
    Closed = 1,
}

/// split 后的读半：明文流或 TLS 流，统一实现 AsyncRead，
/// 让 reader 的 receive 状态机对两种连接只有一份实现
pub(super) enum ReadSide {
    Plain(OwnedReadHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedReadHalf),
    Tls(tokio::io::ReadHalf<TlsIo>),
}

impl AsyncRead for ReadSide {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ReadSide::Plain(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(unix)]
            ReadSide::Unix(r) => Pin::new(r).poll_read(cx, buf),
            ReadSide::Tls(r) => Pin::new(r).poll_read(cx, buf),
        }
    }
}

/// split 后的写半：对应 ReadSide
pub(super) enum WriteSide {
    Plain(OwnedWriteHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
    Tls(tokio::io::WriteHalf<TlsIo>),
}

impl AsyncWrite for WriteSide {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            WriteSide::Plain(w) => Pin::new(w).poll_write(cx, buf),
            #[cfg(unix)]
            WriteSide::Unix(w) => Pin::new(w).poll_write(cx, buf),
            WriteSide::Tls(w) => Pin::new(w).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            WriteSide::Plain(w) => Pin::new(w).poll_flush(cx),
            #[cfg(unix)]
            WriteSide::Unix(w) => Pin::new(w).poll_flush(cx),
            WriteSide::Tls(w) => Pin::new(w).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            WriteSide::Plain(w) => Pin::new(w).poll_shutdown(cx),
            #[cfg(unix)]
            WriteSide::Unix(w) => Pin::new(w).poll_shutdown(cx),
            WriteSide::Tls(w) => Pin::new(w).poll_shutdown(cx),
        }
    }
}

/// 连接入口：静态方法 Tcp.connect / Tcp.listen
pub(crate) struct Tcp;

#[cosmos::userdata(name = "network.tcp.stream")]
impl Tcp {
    /// 连接失败或超时返回 nil + 错误串（local conn, err = Tcp.connect(...)）；
    /// timeout_ms 可选：<= 0 或缺省表示不设连接超时
    async fn connect(host: String, port: i64, timeout_ms: Option<i64>) -> Result<TcpConn, String> {
        let deadline = timeout_ms
            .and_then(timeout_from_ms)
            .map(|d| Instant::now() + d);
        match with_deadline(deadline, TcpStream::connect((host.as_str(), port as u16))).await {
            Ok(Ok(stream)) => Ok(TcpConn {
                stream: Some(stream),
                state: Rc::new(Cell::new(State::Connected)),
                timeout: None,
                max_buf: None,
            }),
            Ok(Err(e)) => Err(classify_io(&e)),
            Err(()) => Err("timeout".to_string()),
        }
    }

    /// 监听 0.0.0.0:port；timeout_ms 作用于每次 accept
    async fn listen(port: i64, timeout_ms: Option<i64>) -> Result<TcpListener, String> {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16))
            .await
            .map_err(|e| classify_io(&e))?;
        Ok(TcpListener {
            listener: Some(listener),
            timeout: timeout_ms.and_then(timeout_from_ms),
        })
    }
}

/// TCP 监听器：accept 出普通 TcpConn（close 后 accept 返回 nil,"closed"）
pub(crate) struct TcpListener {
    listener: Option<tokio::net::TcpListener>,
    timeout: Option<Duration>,
}

#[cosmos::userdata(name = "network.tcp.stream.listener")]
impl TcpListener {
    /// 接受一个连接；timeout 到点返回 nil,"timeout"
    async fn accept(&mut self) -> Result<TcpConn, String> {
        let Some(listener) = self.listener.as_ref() else {
            return Err("closed".to_string());
        };
        let deadline = self.timeout.map(|d| Instant::now() + d);
        let (stream, _) = match with_deadline(deadline, listener.accept()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(classify_io(&e)),
            Err(()) => return Err("timeout".to_string()),
        };
        Ok(TcpConn {
            stream: Some(stream),
            state: Rc::new(Cell::new(State::Connected)),
            timeout: None,
            max_buf: None,
        })
    }

    fn close(&mut self) {
        self.listener.take();
    }
}

pub(crate) struct TcpConn {
    stream: Option<TcpStream>,
    state: Rc<Cell<State>>,
    /// 读写超时；split 时传递给 reader/writer（对齐 OpenResty settimeout）
    timeout: Option<Duration>,
    /// *a/*l/*L/receiveuntil 累积上限；split 时传递给 reader
    max_buf: Option<usize>,
}

#[cosmos::userdata(name = "network.tcp.stream.conn")]
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
    fn split(&mut self) -> Result<(TcpReader, TcpWriter), String> {
        let stream = self
            .stream
            .take()
            .ok_or("connection is closed or already split")?;
        Ok(Duplex::tcp(stream, self.state.clone(), self.timeout, self.max_buf).split())
    }

    fn close(&mut self) {
        self.stream.take(); // drop → 关连接
        self.state.set(State::Closed);
    }
}

/// receive 的读取模式（OpenResty tcpsock:receive 的参数，'*' 前缀可省略）
pub(crate) enum ReceivePattern {
    All,         // "*a"
    Line,        // "*l"：去掉行尾 \n 与 \r
    LineKeep,    // "*L"：保留行尾 \n
    Size(usize), // 数字：精确 n 字节
}

/// receive 的结果：data, err, partial 三值协议（data/partial 为原始字节）。
/// 每个变体的字段即 Lua 侧的返回值（按序压栈，由 derive 生成）：
///   Data        → data
///   Eof         → nil
///   Closed/…    → nil, reason, partial
#[derive(cosmos::PushLua)]
pub(crate) enum ReceiveOutcome {
    Data(Vec<u8>),
    Eof(Nil),
    Closed(Nil, &'static str, Vec<u8>),
    Timeout(Nil, &'static str, Vec<u8>),
    Overflow(Nil, &'static str, Vec<u8>),
    Error(Nil, String, Vec<u8>),
}

pub(crate) struct TcpReader {
    pub(super) inner: Option<BufReader<ReadSide>>,
    pub(super) state: Rc<Cell<State>>,
    /// 内置读缓冲：跨 fill 复用，返回时 mem::take 零拷贝移出
    pub(super) buf: Vec<u8>,
    pub(super) timeout: Option<Duration>,
    /// *a/*l/*L/receiveuntil 的累积上限；None 不限
    pub(super) max_buf: Option<usize>,
}

#[cosmos::userdata(name = "network.tcp.stream.reader")]
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
            return ReceiveOutcome::Closed(Nil, "closed", Vec::new());
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
                                    Nil,
                                    classify_io(&e),
                                    std::mem::take(&mut self.buf),
                                );
                            }
                            Err(FillError::Timeout) => {
                                return ReceiveOutcome::Timeout(
                                    Nil,
                                    "timeout",
                                    std::mem::take(&mut self.buf),
                                );
                            }
                        };
                        let got = avail.len();
                        let take = cap.map_or(got, |c| got.min(c.saturating_sub(self.buf.len())));
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
                        return ReceiveOutcome::Overflow(
                            Nil,
                            "buffer overflow",
                            std::mem::take(&mut self.buf),
                        );
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
                                    Nil,
                                    classify_io(&e),
                                    std::mem::take(&mut self.buf),
                                );
                            }
                            Err(FillError::Timeout) => {
                                return ReceiveOutcome::Timeout(
                                    Nil,
                                    "timeout",
                                    std::mem::take(&mut self.buf),
                                );
                            }
                        };
                        if avail.is_empty() {
                            // EOF：干净关闭返回 nil，残行按 closed 交回
                            self.state.set(State::Closed);
                            return if self.buf.is_empty() {
                                ReceiveOutcome::Eof(Nil)
                            } else {
                                ReceiveOutcome::Closed(Nil, "closed", std::mem::take(&mut self.buf))
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
                    inner.consume(if done && !keep_eol {
                        allowed + 1
                    } else {
                        allowed
                    });
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
                        return ReceiveOutcome::Overflow(
                            Nil,
                            "buffer overflow",
                            std::mem::take(&mut self.buf),
                        );
                    }
                }
            }
            ReceivePattern::Size(n) => {
                self.buf.resize(n, 0); // 用时扩展
                let mut filled = 0usize;
                while filled < n {
                    match read_chunk(deadline, inner, &mut self.buf[filled..]).await {
                        Ok(0) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Closed(
                                Nil,
                                "closed",
                                std::mem::take(&mut self.buf),
                            );
                        }
                        Ok(nread) => filled += nread,
                        Err(ChunkError::Io(e)) => {
                            self.state.set(State::Closed);
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Error(
                                Nil,
                                classify_io(&e),
                                std::mem::take(&mut self.buf),
                            );
                        }
                        Err(ChunkError::Timeout) => {
                            self.buf.truncate(filled);
                            return ReceiveOutcome::Timeout(
                                Nil,
                                "timeout",
                                std::mem::take(&mut self.buf),
                            );
                        }
                    }
                }
                ReceiveOutcome::Data(std::mem::take(&mut self.buf))
            }
        }
    }

    /// receiveuntil(delim)：返回迭代器闭包，每次调用读到分隔符为止，
    /// 返回分隔符之前的数据（分隔符被消费）。读取状态随迭代器独占：
    /// 本 reader 之后不可再 receive；timeout/setbufsize 在此刻快照。
    fn receiveuntil(&mut self, delim: String) -> Result<UntilIter, String> {
        if delim.is_empty() {
            return Err("empty delimiter".to_string());
        }
        let Some(inner) = self.inner.take() else {
            return Err("closed".to_string());
        };
        Ok(UntilIter(TcpUntil {
            inner: Some(inner),
            state: self.state.clone(),
            delim: delim.into_bytes(),
            buf: Vec::new(),
            timeout: self.timeout,
            max_buf: self.max_buf,
        }))
    }

    fn close(&mut self) {
        self.inner.take(); // drop → shutdown 读半
        self.state.set(State::Closed);
    }
}

/// receiveuntil 迭代器的状态：独占 reader 的流，每次 next() 读一段
pub(crate) struct TcpUntil {
    inner: Option<BufReader<ReadSide>>,
    state: Rc<Cell<State>>,
    delim: Vec<u8>,
    /// 单次调用内的累积缓冲（分隔符可能跨 BufReader 分块边界）
    buf: Vec<u8>,
    timeout: Option<Duration>,
    max_buf: Option<usize>,
}

// 空 impl：只为拿到 UserData（metatable + __gc）与 require 入口，
// 读取逻辑在下面的普通 impl 里（Lua 侧通过闭包调用，不暴露方法）
#[cosmos::userdata(name = "network.tcp.stream.until")]
impl TcpUntil {}

/// 迭代器返回值：压栈时构造 TcpUntil userdata，再包成 Lua 闭包（upvalue = userdata）。
/// Lua 用法：local iter = reader:receiveuntil("--"); local data, err, partial = iter()
pub(crate) struct UntilIter(pub(crate) TcpUntil);

impl TcpUntil {
    /// 读到分隔符或出错为止；分隔符可跨分块边界。
    /// EOF/超时/超限返回 nil + err + partial（partial 为已读到的数据）。
    pub(crate) async fn next(&mut self) -> ReceiveOutcome {
        let Some(inner) = self.inner.as_mut() else {
            return ReceiveOutcome::Closed(Nil, "closed", Vec::new());
        };
        let d = self.delim.len();
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
                        return ReceiveOutcome::Error(
                            Nil,
                            classify_io(&e),
                            std::mem::take(&mut self.buf),
                        );
                    }
                    Err(FillError::Timeout) => {
                        return ReceiveOutcome::Timeout(
                            Nil,
                            "timeout",
                            std::mem::take(&mut self.buf),
                        );
                    }
                };
                if avail.is_empty() {
                    // EOF 前没等到分隔符：提前关闭
                    self.state.set(State::Closed);
                    return ReceiveOutcome::Closed(Nil, "closed", std::mem::take(&mut self.buf));
                }
                // 1) 跨边界匹配：分隔符起始于 buf 末尾 d-1 字节内，剩余部分在 avail
                let lo = self.buf.len().saturating_sub(d - 1);
                let mut found = None;
                for start in lo..self.buf.len() {
                    let have = self.buf.len() - start;
                    let need = d - have;
                    if avail.len() >= need
                        && self.buf[start..] == self.delim[..have]
                        && avail[..need] == self.delim[have..]
                    {
                        found = Some((start, need));
                        break;
                    }
                }
                if let Some((start, need)) = found {
                    self.buf.extend_from_slice(&avail[..need]);
                    (need, Some(start))
                } else if let Some(p) = find_subslice(avail, &self.delim) {
                    // 2) 分隔符完整落在 avail 内
                    self.buf.extend_from_slice(&avail[..p]);
                    (p + d, Some(self.buf.len()))
                } else {
                    // 3) 还没有分隔符：受上限约束地收下（上限按次生效）
                    let take = cap.map_or(avail.len(), |c| {
                        avail.len().min(c.saturating_sub(self.buf.len()))
                    });
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
                return ReceiveOutcome::Overflow(
                    Nil,
                    "buffer overflow",
                    std::mem::take(&mut self.buf),
                );
            }
        }
    }
}

enum FillError {
    Io(std::io::Error),
    Timeout,
}

/// 取 BufReader 内部缓冲的下一段（空切片 = EOF）；deadline 到点返回 Timeout。
/// 只借用不消费：调用方按需 extend 到自己的 buf 后再 consume。
async fn fill(
    deadline: Option<Instant>,
    inner: &mut BufReader<ReadSide>,
) -> Result<&[u8], FillError> {
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
    inner: &mut BufReader<ReadSide>,
    buf: &mut [u8],
) -> Result<usize, ChunkError> {
    match with_deadline(deadline, inner.read(buf)).await {
        Ok(Ok(n)) => Ok(n),
        Ok(Err(e)) => Err(ChunkError::Io(e)),
        Err(()) => Err(ChunkError::Timeout),
    }
}

pub(crate) struct TcpWriter {
    pub(super) inner: Option<WriteSide>,
    pub(super) state: Rc<Cell<State>>,
    pub(super) timeout: Option<Duration>,
}

/// 成对拥有读写器，供旧 split 接口与顺序协议 socket 共用读取状态机。
/// 字段保持封闭，TLS 升级只能成对取回底层流，并完整移交预读字节。
pub(crate) struct Duplex {
    reader: TcpReader,
    writer: TcpWriter,
}

impl Duplex {
    fn new(
        read: ReadSide,
        write: WriteSide,
        state: Rc<Cell<State>>,
        timeout: Option<Duration>,
        max_buf: Option<usize>,
    ) -> Self {
        Self {
            reader: TcpReader {
                inner: Some(BufReader::new(read)),
                state: state.clone(),
                buf: Vec::new(),
                timeout,
                max_buf,
            },
            writer: TcpWriter {
                inner: Some(write),
                state,
                timeout,
            },
        }
    }

    pub(crate) fn tcp(
        stream: TcpStream,
        state: Rc<Cell<State>>,
        timeout: Option<Duration>,
        max_buf: Option<usize>,
    ) -> Self {
        let (read, write) = stream.into_split();
        Self::new(
            ReadSide::Plain(read),
            WriteSide::Plain(write),
            state,
            timeout,
            max_buf,
        )
    }

    #[cfg(unix)]
    pub(crate) fn unix(
        stream: tokio::net::UnixStream,
        state: Rc<Cell<State>>,
        timeout: Option<Duration>,
        max_buf: Option<usize>,
    ) -> Self {
        let (read, write) = stream.into_split();
        Self::new(
            ReadSide::Unix(read),
            WriteSide::Unix(write),
            state,
            timeout,
            max_buf,
        )
    }

    pub(crate) fn tls(
        stream: TlsIo,
        state: Rc<Cell<State>>,
        timeout: Option<Duration>,
        max_buf: Option<usize>,
    ) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self::new(
            ReadSide::Tls(read),
            WriteSide::Tls(write),
            state,
            timeout,
            max_buf,
        )
    }

    pub(super) fn split(self) -> (TcpReader, TcpWriter) {
        (self.reader, self.writer)
    }

    pub(crate) fn connected(&self) -> bool {
        self.reader.state.get() == State::Connected
            && self.reader.inner.is_some()
            && self.writer.inner.is_some()
    }

    pub(crate) fn is_tls(&self) -> bool {
        matches!(
            self.reader.inner.as_ref().map(BufReader::get_ref),
            Some(ReadSide::Tls(_))
        )
    }

    pub(crate) fn settimeout(&mut self, ms: i64) {
        self.reader.settimeout(ms);
        self.writer.settimeout(ms);
    }

    pub(crate) fn setbufsize(&mut self, size: i64) {
        self.reader.setbufsize(size);
    }

    pub(crate) async fn receive(&mut self, pattern: Option<ReceivePattern>) -> ReceiveOutcome {
        self.reader.receive(pattern).await
    }

    pub(crate) async fn send(&mut self, data: LuaBytes) -> Result<i64, String> {
        self.writer.write(data).await
    }

    pub(crate) fn abort(&mut self) {
        self.reader.close();
        self.writer.inner.take();
    }

    pub(crate) async fn close(mut self) {
        self.reader.close();
        self.writer.close().await;
    }

    pub(crate) fn into_plain(mut self) -> Result<super::transport::PlainIo, String> {
        let reader = self.reader.inner.take().ok_or("closed")?;
        let prefix = reader.buffer().to_vec();
        let read = reader.into_inner();
        let write = self.writer.inner.take().ok_or("closed")?;
        self.reader.state.set(State::Closed);
        let io = match (read, write) {
            (ReadSide::Plain(read), WriteSide::Plain(write)) => super::transport::PlainIo::tcp(
                read.reunite(write)
                    .map_err(|_| "mismatched stream halves")?,
            ),
            #[cfg(unix)]
            (ReadSide::Unix(read), WriteSide::Unix(write)) => super::transport::PlainIo::unix(
                read.reunite(write)
                    .map_err(|_| "mismatched stream halves")?,
            ),
            _ => return Err("TLS upgrade requires a plain connection".into()),
        };
        Ok(io.with_prefix(prefix))
    }
}

#[cosmos::userdata(name = "network.tcp.stream.writer")]
impl TcpWriter {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 设置写超时（毫秒）；ms <= 0 表示不超时
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 整块写完（write_all），返回写入字节数；出错/超时返回 nil + 错误串。
    /// data 用 LuaBytes：短串拷贝、长串锚定（见 cosmos::userdata::LuaBytes）
    async fn write(&mut self, data: LuaBytes) -> Result<i64, String> {
        let Some(inner) = self.inner.as_mut() else {
            return Err("closed".to_string());
        };
        let bytes = &*data;
        let deadline = self.timeout.map(|d| Instant::now() + d);
        // TLS 可以先接收全部明文、仍保留待发送密文；请求完成前必须 flush，
        // 否则小发送缓冲的连接可能与等待响应的读取相互阻塞。
        let write = async {
            inner.write_all(bytes).await?;
            inner.flush().await
        };
        match with_deadline(deadline, write).await {
            Ok(Ok(())) => Ok(bytes.len() as i64),
            Ok(Err(e)) => {
                self.state.set(State::Closed);
                Err(classify_io(&e))
            }
            // 超时不断连接：write_all 未完成的部分可能已部分写出
            Err(()) => Err("timeout".to_string()),
        }
    }

    /// 半关闭写端：明文发 FIN，TLS 发 close_notify（等对端收尾后才返回）
    async fn close(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let deadline = self.timeout.map(|d| Instant::now() + d);
            let _ = with_deadline(deadline, inner.shutdown()).await;
        }
        self.state.set(State::Closed);
    }
}
