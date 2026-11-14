//! HTTP body：本地读取源与 Send 字节通道分离，Lua 回调始终留在 LocalSet。
use std::cell::Cell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::Duration;

use super::{Headers, LuaBytes};
use crate::deadline::with_deadline;
use bytes::{Bytes, BytesMut};
use cosmos::userdata::{FileReader, LuaFunction, Nil};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as HyperBody, Frame, Incoming, SizeHint};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::AbortHandle;
use tokio::time::Instant;

pub(super) const CHUNK: usize = 64 * 1024;
const MAX_READ: usize = 1024 * 1024;
const MAX_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(super) fn integer(value: f64, maximum: f64, name: &str) -> Result<u64, String> {
    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=maximum).contains(&value) {
        return Err(format!(
            "invalid {name} (expected an integer in 0..={maximum})"
        ));
    }
    Ok(value as u64)
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct FileOptions {
    offset: Option<f64>,
    length: Option<f64>,
    close_file: bool,
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct CallbackOptions {
    length: Option<f64>,
}
#[derive(cosmos::FromLua)]
enum Chunk {
    #[lua(type = "string")]
    Data(LuaBytes),
    #[lua(type = "nil")]
    End(Nil, Option<String>),
    #[lua(type = "none")]
    Nothing,
}

#[derive(Clone, cosmos::FromLua, cosmos::PushLua)]
pub(super) enum BodyValue {
    #[lua(type = "string")]
    Bytes(LuaBytes),
    #[lua(type = "userdata")]
    Stream(Body),
}
impl Default for BodyValue {
    fn default() -> Self {
        Self::Bytes(LuaBytes::default())
    }
}
impl BodyValue {
    pub(super) fn cancel(&self) {
        if let Self::Stream(body) = self {
            body.stop();
        }
    }
    pub(super) fn repeat(&self) -> Result<Self, String> {
        match self {
            Self::Bytes(bytes) => Ok(Self::Bytes(LuaBytes(bytes.0.clone()))),
            Self::Stream(_) => {
                Err("streaming request body cannot be replayed after redirect".into())
            }
        }
    }
    pub(super) fn length(&self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes.0.len() as u64),
            Self::Stream(body) => body.0.expected,
        }
    }
    pub(super) fn into_wire(self) -> Result<Outgoing, String> {
        match self {
            Self::Bytes(bytes) => Ok(Outgoing::Full(Full::new(bytes.0))),
            Self::Stream(body) => body.wire(),
        }
    }
}

// 发送准备或请求被取消时，连带关闭其一次性输入源。
pub(super) struct BodyCleanup(pub(super) Option<BodyValue>);
impl Drop for BodyCleanup {
    fn drop(&mut self) {
        if let Some(body) = &self.0 {
            body.cancel();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Body(Rc<Inner>);
struct Inner {
    state: Mutex<State>,
    closed: Cell<bool>,
    cancelled: Arc<AtomicBool>,
    eof: Cell<bool>,
    used: Cell<bool>,
    started: Cell<bool>,
    stopped: Notify,
    expected: Option<u64>,
    deadline: Option<Instant>,
    timeout: Cell<Option<Duration>>,
    limit: Option<u64>,
    outgoing: Option<Box<BodyValue>>,
}
struct State {
    source: Option<Source>,
    pending: Bytes,
    received: u64,
    trailers: Headers,
}
enum Source {
    File {
        file: FileReader,
        offset: u64,
        left: u64,
        close_file: bool,
    },
    Bytes(Bytes),
    Callback(LuaFunction),
    Incoming(Incoming),
}
impl Drop for Source {
    fn drop(&mut self) {
        if let Self::File {
            file,
            close_file: true,
            ..
        } = self
        {
            file.close();
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(outgoing) = &self.outgoing {
            outgoing.cancel();
        }
    }
}
struct ReadLease<'a> {
    state: tokio::sync::MutexGuard<'a, State>,
    body: &'a Body,
    armed: bool,
}
impl Drop for ReadLease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.source.take();
            self.state.pending = Bytes::new();
            self.body.0.closed.set(true);
            self.body.0.cancelled.store(true, Ordering::Release);
            self.body.0.stopped.notify_waiters();
            if let Some(outgoing) = &self.body.0.outgoing {
                outgoing.cancel();
            }
        }
    }
}
impl Body {
    fn new(
        source: Source,
        expected: Option<u64>,
        deadline: Option<Instant>,
        limit: Option<u64>,
        outgoing: Option<BodyValue>,
    ) -> Self {
        Self(Rc::new(Inner {
            state: Mutex::new(State {
                source: Some(source),
                pending: Bytes::new(),
                received: 0,
                trailers: Headers::default(),
            }),
            closed: Cell::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            eof: Cell::new(false),
            used: Cell::new(false),
            started: Cell::new(false),
            stopped: Notify::new(),
            expected,
            deadline,
            timeout: Cell::new(None),
            limit,
            outgoing: outgoing.map(Box::new),
        }))
    }
    pub(super) fn bytes(data: Bytes) -> Self {
        let length = data.len() as u64;
        Self::new(Source::Bytes(data), Some(length), None, None, None)
    }
    pub(super) fn incoming(
        source: Incoming,
        deadline: Option<Instant>,
        limit: Option<u64>,
        outgoing: Option<BodyValue>,
    ) -> Self {
        let expected = source.size_hint().exact();
        Self::new(
            Source::Incoming(source),
            expected,
            deadline,
            limit,
            outgoing,
        )
    }
    pub(super) fn file_region(
        file: FileReader,
        size: u64,
        offset: u64,
        length: Option<u64>,
        close_file: bool,
    ) -> Result<Self, String> {
        if size > MAX_INTEGER as u64 || offset > size {
            return Err("file body offset or size is out of range".into());
        }
        let length = length.unwrap_or(size - offset);
        if length > size - offset {
            return Err("file body range exceeds file size".into());
        }
        Ok(Self::new(
            Source::File {
                file,
                offset,
                left: length,
                close_file,
            },
            Some(length),
            None,
            None,
            None,
        ))
    }
    pub(super) fn stop(&self) {
        self.0.closed.set(true);
        if !self.0.eof.get() {
            self.0.cancelled.store(true, Ordering::Release);
        }
        self.0.stopped.notify_waiters();
        if let Ok(mut state) = self.0.state.try_lock() {
            state.source.take();
            state.pending = Bytes::new();
        }
        if let Some(outgoing) = &self.0.outgoing {
            outgoing.cancel();
        }
    }
    async fn stopped(&self) {
        let ready = self.0.stopped.notified();
        if !self.0.closed.get() {
            ready.await;
        }
    }
    async fn next(&self, max: usize) -> Result<Option<Bytes>, String> {
        let state = self.0.state.lock().await;
        let mut lease = ReadLease {
            state,
            body: self,
            armed: true,
        };
        if self.0.eof.get() {
            lease.armed = false;
            return Ok(None);
        }
        if self.0.closed.get() {
            return Err("closed".into());
        }
        loop {
            if !lease.state.pending.is_empty() {
                let take = max.min(lease.state.pending.len());
                let data = lease.state.pending.split_to(take);
                lease.armed = false;
                return Ok(Some(data));
            }
            if self.0.expected == Some(lease.state.received)
                && !matches!(lease.state.source, Some(Source::Incoming(_)))
            {
                self.0.eof.set(true);
                lease.state.source.take();
                lease.armed = false;
                return Ok(None);
            }
            let max = self
                .0
                .expected
                .map(|total| (total - lease.state.received).min(max as u64) as usize)
                .unwrap_or(max)
                .max(1);
            let chunk = match lease.state.source.as_mut().ok_or("closed")? {
                Source::Bytes(bytes) => {
                    if bytes.is_empty() {
                        None
                    } else {
                        Some(bytes.split_to(max.min(bytes.len())))
                    }
                }
                Source::File {
                    file, offset, left, ..
                } => {
                    if *left == 0 {
                        None
                    } else {
                        let data = file.read_at(*offset, max.min(*left as usize)).await?;
                        *offset += data.len() as u64;
                        *left -= data.len() as u64;
                        if data.is_empty() {
                            None
                        } else {
                            Some(Bytes::from(data))
                        }
                    }
                }
                Source::Callback(callback) => match callback.call::<_, Chunk>(max as i64).await? {
                    Chunk::Data(data) => {
                        if data.0.is_empty() {
                            return Err(
                                "body callback must return data or nil, not an empty chunk".into(),
                            );
                        }
                        if data.0.len() > max {
                            return Err("body callback chunk exceeds requested size".into());
                        }
                        Some(data.0)
                    }
                    Chunk::End(_, Some(error)) => return Err(error),
                    Chunk::End(_, None) | Chunk::Nothing => None,
                },
                Source::Incoming(incoming) => match incoming.frame().await {
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(data) => Some(data),
                        Err(frame) => {
                            if let Ok(trailers) = frame.into_trailers() {
                                lease.state.trailers = Headers(
                                    trailers
                                        .iter()
                                        .map(|(k, v)| {
                                            (
                                                k.as_str().to_owned(),
                                                String::from_utf8_lossy(v.as_bytes()).into_owned(),
                                            )
                                        })
                                        .collect(),
                                );
                            }
                            continue;
                        }
                    },
                    Some(Err(error)) => return Err(format!("http body: {error}")),
                    None => None,
                },
            };
            let Some(chunk) = chunk else {
                if self
                    .0
                    .expected
                    .is_some_and(|length| length != lease.state.received)
                {
                    return Err("HTTP body ended before declared content-length".into());
                }
                self.0.eof.set(true);
                lease.state.source.take();
                lease.armed = false;
                if let Some(outgoing) = &self.0.outgoing {
                    outgoing.cancel();
                }
                return Ok(None);
            };
            lease.state.received = lease
                .state
                .received
                .checked_add(chunk.len() as u64)
                .ok_or("HTTP body length overflow")?;
            if self
                .0
                .limit
                .is_some_and(|limit| lease.state.received > limit)
            {
                return Err("HTTP body exceeds max_body".into());
            }
            if self
                .0
                .expected
                .is_some_and(|length| lease.state.received > length)
            {
                return Err("HTTP body exceeds declared content-length".into());
            }
            lease.state.pending = chunk;
        }
    }
    async fn read_inner(&self, size: usize) -> Result<Option<Bytes>, String> {
        let deadline = match (
            self.0.deadline,
            self.0.timeout.get().map(|d| Instant::now() + d),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.stop();
            return Err("timeout".into());
        }
        let result = tokio::select! {
            biased;
            _ = self.stopped() => Err("closed".to_string()),
            result = with_deadline(deadline, self.next(size)) => match result { Ok(result)=>result, Err(())=>Err("timeout".into()) },
        };
        if result.is_err() {
            self.stop();
        }
        result
    }
    pub(super) async fn collect(&self, limit: Option<u64>) -> Result<Bytes, String> {
        if self.0.used.get() {
            return Err("HTTP body is already consumed".into());
        }
        self.0.started.set(true);
        let mut result = BytesMut::new();
        while let Some(data) = self.read_inner(CHUNK).await? {
            if limit.is_some_and(|limit| result.len() as u64 + data.len() as u64 > limit) {
                self.stop();
                return Err("HTTP body exceeds collection limit".into());
            }
            result.extend_from_slice(&data);
        }
        Ok(result.freeze())
    }
    fn wire(self) -> Result<Outgoing, String> {
        if self.0.started.get() || self.0.used.replace(true) || self.0.closed.get() {
            return Err("HTTP body is already consumed or closed".into());
        }
        if self.0.expected == Some(0) {
            self.0.eof.set(true);
            self.stop();
            return Ok(Outgoing::empty());
        }
        let (sender, receiver) = mpsc::channel(1);
        let expected = self.0.expected;
        let cancelled = self.0.cancelled.clone();
        let producer = tokio::task::spawn_local(async move {
            struct Cleanup(Body);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    self.0.stop();
                }
            }
            let cleanup = Cleanup(self);
            loop {
                let slot = tokio::select! {
                    biased;
                    _=cleanup.0.stopped()=>break,
                    slot=sender.reserve()=>match slot { Ok(slot)=>slot,Err(_)=>break },
                };
                match cleanup.0.read_inner(CHUNK).await {
                    Ok(Some(data)) => slot.send(Ok(Frame::data(data))),
                    Ok(None) => break,
                    Err(error) => {
                        slot.send(Err(io::Error::other(error)));
                        break;
                    }
                }
            }
        });
        Ok(Outgoing::Stream(Pipe {
            receiver,
            producer: producer.abort_handle(),
            remaining: expected,
            cancelled,
            failed: false,
        }))
    }
}
#[cosmos::userdata(name = "network.http.body")]
impl Body {
    async fn from_file(file: FileReader, options: Option<FileOptions>) -> Result<Body, String> {
        let options = options.unwrap_or_default();
        let offset = integer(options.offset.unwrap_or(0.0), MAX_INTEGER, "file offset")?;
        let length = options
            .length
            .map(|n| integer(n, MAX_INTEGER, "file length"))
            .transpose()?;
        let (size, _) = file.info().await?;
        Self::file_region(file, size, offset, length, options.close_file)
    }
    fn from_callback(
        callback: LuaFunction,
        options: Option<CallbackOptions>,
    ) -> Result<Body, String> {
        let expected = options
            .unwrap_or_default()
            .length
            .map(|n| integer(n, MAX_INTEGER, "body length"))
            .transpose()?;
        Ok(Self::new(
            Source::Callback(callback),
            expected,
            None,
            None,
            None,
        ))
    }
    async fn read(self, size: Option<f64>) -> Result<Option<LuaBytes>, String> {
        if self.0.used.get() {
            return Err("HTTP body is already consumed".into());
        }
        let size = integer(size.unwrap_or(CHUNK as f64), MAX_READ as f64, "read size")? as usize;
        if size == 0 {
            return Err("body read size must be positive".into());
        }
        self.0.started.set(true);
        self.read_inner(size).await.map(|v| v.map(LuaBytes))
    }
    async fn read_all(self, limit: Option<f64>) -> Result<LuaBytes, String> {
        if self.0.used.get() {
            return Err("HTTP body is already consumed".into());
        }
        let limit = integer(
            limit.unwrap_or(64.0 * 1024.0 * 1024.0),
            64.0 * 1024.0 * 1024.0,
            "collection limit",
        )?;
        self.0.started.set(true);
        self.collect(Some(limit)).await.map(LuaBytes)
    }
    async fn trailers(self) -> Result<Headers, String> {
        if !self.0.eof.get() {
            return Err("consume the body before reading trailers".into());
        }
        Ok(self.0.state.lock().await.trailers.clone())
    }
    fn settimeout(&self, ms: f64) -> Result<bool, String> {
        if !ms.is_finite() || !(0.0..=1e12).contains(&ms) {
            return Err("invalid body timeout (ms)".into());
        }
        self.0
            .timeout
            .set((ms > 0.0).then(|| Duration::from_secs_f64(ms / 1000.0)));
        Ok(true)
    }
    fn close(&self) -> bool {
        self.stop();
        true
    }
    fn is_closed(&self) -> bool {
        self.0.closed.get() || self.0.eof.get()
    }
}

pub(super) enum Outgoing {
    Full(Full<Bytes>),
    Stream(Pipe),
}
pub(super) struct Pipe {
    receiver: mpsc::Receiver<Result<Frame<Bytes>, io::Error>>,
    producer: AbortHandle,
    remaining: Option<u64>,
    cancelled: Arc<AtomicBool>,
    failed: bool,
}
impl Drop for Pipe {
    fn drop(&mut self) {
        self.producer.abort();
    }
}
impl Outgoing {
    pub(super) fn empty() -> Self {
        Self::Full(Full::new(Bytes::new()))
    }
}
impl HyperBody for Outgoing {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match self.get_mut() {
            Self::Full(body) => Pin::new(body)
                .poll_frame(cx)
                .map(|v| v.map(|v| v.map_err(|err| match err {}))),
            Self::Stream(pipe) => {
                if pipe.failed {
                    return Poll::Ready(None);
                }
                if pipe.cancelled.load(Ordering::Acquire) {
                    pipe.failed = true;
                    let error = match pipe.receiver.try_recv() {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("HTTP body aborted"),
                    };
                    return Poll::Ready(Some(Err(error)));
                }
                match pipe.receiver.poll_recv(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        if let Some(data) = frame.data_ref() {
                            if let Some(left) = &mut pipe.remaining {
                                *left = left.saturating_sub(data.len() as u64);
                            }
                        }
                        Poll::Ready(Some(Ok(frame)))
                    }
                    other => other,
                }
            }
        }
    }
    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(body) => body.size_hint(),
            Self::Stream(pipe) => {
                let mut hint = SizeHint::new();
                if let Some(left) = pipe.remaining {
                    hint.set_exact(left);
                }
                hint
            }
        }
    }
    fn is_end_stream(&self) -> bool {
        match self {
            Self::Full(body) => body.is_end_stream(),
            Self::Stream(pipe) => pipe.receiver.is_closed() && pipe.receiver.is_empty(),
        }
    }
}
