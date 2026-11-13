//! WebSocket 连接：帧收发、关闭和服务端升级后的回调执行。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use fastwebsockets::{FragmentCollector, Frame, OpCode, Payload, WebSocket};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::time::Instant;

use cosmos::userdata::LuaFunction;

use crate::http::server::HttpRequest;
use crate::http::LuaBytes;
use crate::tcp::stream::{timeout_from_ms, with_deadline};

/// 握手完成后的连接统一类型（服务端 Upgraded / 客户端 Upgraded 相同）
type WsStream = FragmentCollector<TokioIo<Upgraded>>;

fn payload_bytes(payload: Payload<'_>) -> Bytes {
    match payload {
        Payload::Borrowed(b) => Bytes::copy_from_slice(b),
        Payload::BorrowedMut(b) => Bytes::copy_from_slice(b),
        Payload::Owned(v) => Bytes::from(v),
        Payload::Bytes(b) => b.freeze(),
    }
}

/// receive 的成功结果：("text"|"binary", 数据)，由元组 PushLua 作为两个返回值压栈
type WsMessage = (&'static str, LuaBytes);

/// 连接对象：帧流放在共享槽里，服务端在 handler 协程结束后要能强制关闭，
/// 因此 run_conn 与 userdata 持有同一个 Rc
pub(crate) struct WsConn {
    ws: Rc<RefCell<Option<WsStream>>>,
    timeout: Option<Duration>,
}

impl WsConn {
    /// 客户端握手与服务端升级共用连接初始化。
    pub(super) fn new(
        mut ws: WebSocket<TokioIo<Upgraded>>,
        max_message: usize,
        timeout: Option<Duration>,
    ) -> Self {
        ws.set_auto_pong(true);
        ws.set_auto_close(true);
        ws.set_max_message_size(max_message);
        Self {
            ws: Rc::new(RefCell::new(Some(FragmentCollector::new(ws)))),
            timeout,
        }
    }

    fn take(&self) -> Result<WsStream, String> {
        self.ws
            .borrow_mut()
            .take()
            .ok_or_else(|| "closed".to_string())
    }

    fn put(&self, ws: WsStream) {
        *self.ws.borrow_mut() = Some(ws);
    }

    fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|d| Instant::now() + d)
    }

    /// 写帧：写超时/出错后流状态不可信，不再放回（连接视为关闭）
    async fn write_frame(&self, frame: Frame<'_>) -> Result<(), String> {
        let mut ws = self.take()?;
        let deadline = self.deadline();
        match with_deadline(deadline, ws.write_frame(frame)).await {
            Ok(Ok(())) => {
                self.put(ws);
                Ok(())
            }
            Ok(Err(err)) => Err(err.to_string()),
            Err(()) => Err("timeout".to_string()),
        }
    }
}

#[cosmos::userdata(name = "network.websocket.conn")]
impl WsConn {
    /// 0 打开 / 1 已关闭
    fn state(&self) -> i64 {
        if self.ws.borrow().is_none() {
            1
        } else {
            0
        }
    }

    /// 毫秒；<= 0 清除超时。只影响之后的 send/send_binary/receive/ping/close
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 文本帧；Lua 字符串必须能按 UTF-8 解码（否则用 send_binary）
    async fn send(&mut self, data: Vec<u8>) -> Result<(), String> {
        if std::str::from_utf8(&data).is_err() {
            return Err("send: payload is not valid UTF-8 (use send_binary)".to_string());
        }
        self.write_frame(Frame::text(Payload::Owned(data))).await
    }

    async fn send_binary(&mut self, data: Vec<u8>) -> Result<(), String> {
        self.write_frame(Frame::binary(Payload::Owned(data))).await
    }

    /// 控制帧 payload 上限 125 字节
    async fn ping(&mut self, data: Option<Vec<u8>>) -> Result<(), String> {
        let payload = data.unwrap_or_default();
        if payload.len() > 125 {
            return Err("ping: payload exceeds 125 bytes".to_string());
        }
        self.write_frame(Frame::new(
            true,
            OpCode::Ping,
            None,
            Payload::Owned(payload),
        ))
        .await
    }

    /// 收一条完整消息（分片已合并）：("text"|"binary", data)；
    /// 对端关闭返回 nil,"closed"；超时返回 nil,"timeout"（连接仍可继续用）
    async fn receive(&mut self) -> Result<WsMessage, String> {
        let mut ws = self.take()?;
        let deadline = self.deadline();
        loop {
            match with_deadline(deadline, ws.read_frame()).await {
                Ok(Ok(frame)) => match frame.opcode {
                    OpCode::Text | OpCode::Binary => {
                        let op = if frame.opcode == OpCode::Text {
                            "text"
                        } else {
                            "binary"
                        };
                        let data = payload_bytes(frame.payload);
                        self.put(ws);
                        return Ok((op, LuaBytes(data)));
                    }
                    // 对端 close：auto_close 已回过 close 帧，连接结束
                    OpCode::Close => return Err("closed".to_string()),
                    // auto_pong 吃掉 ping，这里只可能见到对端的 pong；忽略继续收
                    _ => continue,
                },
                // 读出错（含帧过大、协议错误）：流状态不可信，直接丢弃
                Ok(Err(err)) => return Err(err.to_string()),
                Err(()) => {
                    self.put(ws);
                    return Err("timeout".to_string());
                }
            }
        }
    }

    /// 发 close 帧并结束连接；不给 code 时用 1000
    async fn close(&mut self, code: Option<i64>, reason: Option<Vec<u8>>) -> Result<(), String> {
        let code = match code {
            None => 1000u16,
            Some(c) => u16::try_from(c)
                .ok()
                .filter(|c| (1000..5000).contains(c))
                .ok_or_else(|| format!("close: invalid code {c}"))?,
        };
        let reason = reason.unwrap_or_default();
        self.write_frame(Frame::close(code, &reason)).await?;
        // close 之后写半已关闭：丢弃流，state() 变 1
        self.ws.borrow_mut().take();
        Ok(())
    }
}

/// 服务端：101 握手完成后的帧循环（在独立任务里跑一个 Lua 协程）
pub(crate) async fn run_conn(
    ws: WebSocket<TokioIo<Upgraded>>,
    handler: LuaFunction,
    request: HttpRequest,
    max_message: usize,
) {
    let conn = WsConn::new(ws, max_message, None);
    let shared = conn.ws.clone();
    if let Err(err) = handler.call::<_, ()>((conn, request)).await {
        eprintln!("websocket handler error: {err}");
    }
    // handler 返回（或被取消）后由服务端主动 close；对端可能已关，忽略错误
    let pending = shared.borrow_mut().take();
    if let Some(mut ws) = pending {
        let _ = ws.write_frame(Frame::close(1000, b"")).await;
    }
}
