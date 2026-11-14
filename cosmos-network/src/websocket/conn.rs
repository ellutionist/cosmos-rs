//! WebSocket 连接：后台完整收帧、独立写半与关闭唤醒。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, Payload, WebSocket, WebSocketWrite};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::WriteHalf;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use cosmos::userdata::LuaFunction;

use crate::deadline::{timeout_from_ms, with_deadline};
use crate::http::server::HttpRequest;
use crate::http::LuaBytes;

type Writer = WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>;
type WsMessage = (&'static str, LuaBytes);

fn payload_bytes(payload: Payload<'_>) -> Bytes {
    match payload {
        Payload::Borrowed(b) => Bytes::copy_from_slice(b),
        Payload::BorrowedMut(b) => Bytes::copy_from_slice(b),
        Payload::Owned(v) => Bytes::from(v),
        Payload::Bytes(b) => b.freeze(),
    }
}

/// 读任务不持有 Inner，最后一个 Lua/Rust 句柄销毁时能立即取消任务。
#[derive(Clone)]
pub(crate) struct WsConn(Rc<Inner>);

struct Inner {
    shared: Rc<Shared>,
    incoming: Mutex<mpsc::Receiver<WsMessage>>,
    timeout: Cell<Option<Duration>>,
    reader: AbortHandle,
}

struct Shared {
    writer: Mutex<Option<Writer>>,
    closed: Cell<bool>,
    error: RefCell<String>,
    stopped: Notify,
}

impl Shared {
    fn stop(&self, error: String) {
        if !self.closed.replace(true) {
            *self.error.borrow_mut() = error;
            self.stopped.notify_waiters();
        }
        if let Ok(mut writer) = self.writer.try_lock() {
            writer.take();
        }
    }

    fn error(&self) -> String {
        self.error.borrow().clone()
    }

    async fn stopped(&self) {
        let notified = self.stopped.notified();
        if !self.closed.get() {
            notified.await;
        }
    }

    async fn write(&self, frame: Frame<'_>, timeout: Option<Duration>) -> Result<(), String> {
        let operation = async {
            let mut slot = self.writer.lock().await;
            if self.closed.get() {
                return Err(self.error());
            }
            let mut writer = slot.take().ok_or_else(|| self.error())?;
            // 写入一旦开始，被取消时连接不可再复用，避免后续帧接在半帧之后。
            let mut cancellation = WriteCancellation {
                shared: self,
                armed: true,
            };
            let result = writer
                .write_frame(frame)
                .await
                .map_err(|err| err.to_string());
            if result.is_ok() && !self.closed.get() {
                *slot = Some(writer);
                cancellation.armed = false;
            } else if let Err(error) = &result {
                self.stop(error.clone());
            }
            result
        };
        tokio::select! {
            biased;
            _ = self.stopped() => Err(self.error()),
            result = with_deadline(timeout.map(|d| Instant::now() + d), operation) => {
                result.map_err(|_| "timeout".to_string())?
            }
        }
    }
}

struct WriteCancellation<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Drop for WriteCancellation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.stop("closed".into());
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop("closed".into());
        self.reader.abort();
    }
}

impl WsConn {
    pub(super) fn new(
        mut ws: WebSocket<TokioIo<Upgraded>>,
        max_message: usize,
        timeout: Option<Duration>,
    ) -> Self {
        ws.set_auto_pong(true);
        ws.set_auto_close(true);
        ws.set_max_message_size(max_message);
        let (reader, writer) = ws.split(tokio::io::split);
        let shared = Rc::new(Shared {
            writer: Mutex::new(Some(writer)),
            closed: Cell::new(false),
            error: RefCell::new("closed".into()),
            stopped: Notify::new(),
        });
        // 有界队列反压读任务；用户超时/取消只影响取消息，不取消底层帧解析。
        let (sender, incoming) = mpsc::channel(8);
        let transport = shared.clone();
        let task = tokio::task::spawn_local(async move {
            let mut reader = FragmentCollectorRead::new(reader);
            let mut send_control = |frame| {
                let transport = transport.clone();
                async move {
                    transport
                        .write(frame, Some(Duration::from_secs(5)))
                        .await
                        .map_err(std::io::Error::other)
                }
            };
            loop {
                let result = tokio::select! {
                    biased;
                    _ = transport.stopped() => break,
                    result = reader.read_frame(&mut send_control) => result,
                };
                let message = match result {
                    Ok(frame) => match frame.opcode {
                        OpCode::Text | OpCode::Binary => {
                            let op = if frame.opcode == OpCode::Text {
                                "text"
                            } else {
                                "binary"
                            };
                            (op, LuaBytes(payload_bytes(frame.payload)))
                        }
                        OpCode::Close => {
                            transport.stop("closed".into());
                            break;
                        }
                        _ => continue,
                    },
                    Err(error) => {
                        transport.stop(error.to_string());
                        break;
                    }
                };
                tokio::select! {
                    biased;
                    _ = transport.stopped() => break,
                    result = sender.send(message) => if result.is_err() { break; },
                }
            }
        });
        Self(Rc::new(Inner {
            shared,
            incoming: Mutex::new(incoming),
            timeout: Cell::new(timeout),
            reader: task.abort_handle(),
        }))
    }
}

#[cosmos::userdata(name = "network.websocket.conn")]
impl WsConn {
    fn state(&self) -> i64 {
        i64::from(self.0.shared.closed.get())
    }

    /// 毫秒；<= 0 清除超时。仅影响之后开始的操作。
    fn settimeout(&self, ms: i64) {
        self.0.timeout.set(timeout_from_ms(ms));
    }

    async fn send(self, data: Vec<u8>) -> Result<(), String> {
        if std::str::from_utf8(&data).is_err() {
            return Err("send: payload is not valid UTF-8 (use send_binary)".into());
        }
        self.0
            .shared
            .write(Frame::text(Payload::Owned(data)), self.0.timeout.get())
            .await
    }

    async fn send_binary(self, data: Vec<u8>) -> Result<(), String> {
        self.0
            .shared
            .write(Frame::binary(Payload::Owned(data)), self.0.timeout.get())
            .await
    }

    async fn ping(self, data: Option<Vec<u8>>) -> Result<(), String> {
        let payload = data.unwrap_or_default();
        if payload.len() > 125 {
            return Err("ping: payload exceeds 125 bytes".into());
        }
        self.0
            .shared
            .write(
                Frame::new(true, OpCode::Ping, None, Payload::Owned(payload)),
                self.0.timeout.get(),
            )
            .await
    }

    /// 多个接收者按取锁顺序消费；取消/超时不会丢失解析中的消息。
    async fn receive(self) -> Result<WsMessage, String> {
        let operation = async {
            let mut receiver = self.0.incoming.lock().await;
            tokio::select! {
                biased;
                message = receiver.recv() => message.ok_or_else(|| self.0.shared.error()),
                _ = self.0.shared.stopped() => Err(self.0.shared.error()),
            }
        };
        with_deadline(self.0.timeout.get().map(|d| Instant::now() + d), operation)
            .await
            .map_err(|_| "timeout".to_string())?
    }

    /// 幂等关闭；始终唤醒挂起操作，即使 close 帧发送失败。
    async fn close(self, code: Option<i64>, reason: Option<Vec<u8>>) -> Result<(), String> {
        let code = match code {
            None => 1000u16,
            Some(c) => u16::try_from(c)
                .ok()
                .filter(|c| (1000..5000).contains(c))
                .ok_or_else(|| format!("close: invalid code {c}"))?,
        };
        let reason = reason.unwrap_or_default();
        if reason.len() > 123 || std::str::from_utf8(&reason).is_err() {
            return Err("close: reason must be valid UTF-8 and at most 123 bytes".into());
        }
        if self.0.shared.closed.get() {
            return Ok(());
        }
        let result = self
            .0
            .shared
            .write(
                Frame::close(code, &reason),
                self.0.timeout.get().or(Some(Duration::from_secs(5))),
            )
            .await;
        self.0.shared.stop("closed".into());
        result
    }
}

/// 服务端 handler 结束或取消后，连接及其后台读任务均应结束。
pub(crate) async fn run_conn(
    ws: WebSocket<TokioIo<Upgraded>>,
    handler: LuaFunction,
    request: HttpRequest,
    max_message: usize,
) {
    struct StopOnDrop(Rc<Shared>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.stop("closed".into());
        }
    }
    let conn = WsConn::new(ws, max_message, None);
    let _stop = StopOnDrop(conn.0.shared.clone());
    if let Err(err) = handler.call::<_, ()>((conn.clone(), request)).await {
        eprintln!("websocket handler error: {err}");
    }
    let _ = conn.close(None, None).await;
}
