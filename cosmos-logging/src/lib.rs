//! 独立 logger 通过有界写入背压组合；不安装进程全局 subscriber。
#![warn(unreachable_pub)]
mod fields;
mod writer;
use cosmos::userdata::{CallerLocation, LuaBytes};
use cosmos::{Machine, Module};
use fields::Fields;
use fields::Object;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};
use tokio::sync::{Mutex, Semaphore};
use writer::{Config, Console, Record, Writer};
/// 安装 cosmos.log；宿主 tracing 集成由 logger 的 host 输出显式选择。
pub struct Logging;
impl Module for Logging {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Logger>();
        Ok(())
    }
}
fn now() -> chrono::DateTime<chrono::Utc> {
    std::time::SystemTime::now().into()
}
const LEVELS: [&str; 6] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "OFF"];
fn level(name: &str) -> Result<u8, String> {
    match name {
        "trace" => Ok(0),
        "debug" => Ok(1),
        "info" => Ok(2),
        "warn" => Ok(3),
        "error" => Ok(4),
        "off" => Ok(5),
        _ => Err("log level must be trace, debug, info, warn, error or off".into()),
    }
}
fn integer(value: f64, max: u64, name: &str) -> Result<u64, String> {
    if !value.is_finite() || value.fract() != 0.0 || value < 0.0 || value > max as f64 {
        return Err(format!("invalid log {name}"));
    }
    Ok(value as u64)
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    file: Option<LuaBytes>,
    level: Option<String>,
    format: Option<String>,
    console: Option<String>,
    console_format: Option<String>,
    rotation: Option<String>,
    max_bytes: Option<f64>,
    keep_files: Option<f64>,
    keep_days: Option<f64>,
}
fn path(value: LuaBytes) -> Result<PathBuf, String> {
    if value.is_empty() || value.len() > 4096 || value.contains(&0) {
        return Err("invalid log file path".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(value.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(&value)
            .map(PathBuf::from)
            .map_err(|_| "log file path must be UTF-8".into())
    }
}
struct State {
    writer: Arc<Mutex<Writer>>,
    queue: Arc<Semaphore>,
    level: AtomicU8,
    closed: AtomicBool,
}
#[derive(Clone)]
struct Logger {
    state: Arc<State>,
    context: Arc<Object>,
}
async fn run<R: Send + 'static>(
    job: impl FnOnce() -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let task = tokio::task::spawn_blocking(job);
    struct Abort(tokio::task::AbortHandle);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(task.abort_handle());
    task.await.map_err(|e| format!("log worker failed: {e}"))?
}
impl Logger {
    async fn emit(
        self,
        severity: u8,
        message: LuaBytes,
        fields: Option<Fields>,
        caller: CallerLocation,
    ) -> Result<bool, String> {
        if self.state.closed.load(Ordering::Relaxed) {
            return Err("logger is closed".into());
        }
        if severity < self.state.level.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if message.len() > 65536 {
            return Err("log message exceeds 64 KiB".into());
        }
        let message = std::str::from_utf8(&message)
            .map_err(|_| "log message must be UTF-8")?
            .to_owned();
        let mut merged = (*self.context).clone();
        merged.extend(fields.unwrap_or_default().0?);
        let merged = fields::checked(merged)?;
        let ticket = self
            .state
            .queue
            .clone()
            .try_acquire_owned()
            .map_err(|_| "log queue is full")?;
        let mut writer = self.state.writer.clone().lock_owned().await;
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let record = Record {
            timestamp: now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            level: LEVELS[severity as usize],
            message,
            fields: merged,
            source: caller.source().to_owned(),
            line: caller.line(),
        };
        run(move || {
            let _ticket = ticket;
            writer.write(record, dispatch)?;
            Ok(true)
        })
        .await
    }
}
#[cosmos::userdata(name = "log")]
impl Logger {
    async fn new(options: Option<Options>) -> Result<Self, String> {
        let opts = options.unwrap_or_default();
        let filter = level(opts.level.as_deref().unwrap_or("info"))?;
        if opts.format.as_deref().is_some_and(|s| s != "text")
            || opts.console_format.as_deref().is_some_and(|s| s != "text")
        {
            return Err("logs support text output only".into());
        }
        let console = match opts.console.as_deref().unwrap_or("stderr") {
            "stderr" => Console::Stderr,
            "stdout" => Console::Stdout,
            "none" => Console::None,
            "host" => Console::Host,
            _ => return Err("log console must be stderr, stdout, none or host".into()),
        };
        if matches!(console, Console::Host | Console::None) && opts.console_format.is_some() {
            return Err("console_format requires stdout or stderr".into());
        }
        if opts.file.is_none()
            && (opts.rotation.is_some()
                || opts.max_bytes.is_some()
                || opts.keep_files.is_some()
                || opts.keep_days.is_some())
        {
            return Err("log rotation options require a file".into());
        }
        if opts.file.is_none() && matches!(console, Console::None) {
            return Err("logger requires an output".into());
        }
        let daily = match opts.rotation.as_deref().unwrap_or("daily") {
            "daily" => true,
            "none" => false,
            _ => return Err("log rotation must be daily or none".into()),
        };
        if !daily && opts.keep_days.is_some() {
            return Err("keep_days requires daily rotation".into());
        }
        let config = Config {
            path: opts.file.map(path).transpose()?,
            daily,
            console,
            max_bytes: integer(
                opts.max_bytes.unwrap_or(10.0 * 1024.0 * 1024.0),
                1024 * 1024 * 1024,
                "max_bytes",
            )?,
            keep_files: integer(opts.keep_files.unwrap_or(5.0), 64, "keep_files")? as usize,
            keep_days: integer(opts.keep_days.unwrap_or(7.0), 3650, "keep_days")? as i64,
        };
        let writer = run(move || Writer::new(config)).await?;
        Ok(Self {
            state: Arc::new(State {
                writer: Arc::new(Mutex::new(writer)),
                queue: Arc::new(Semaphore::new(128)),
                level: AtomicU8::new(filter),
                closed: AtomicBool::new(false),
            }),
            context: Arc::new(Object::new()),
        })
    }
    fn child(&self, context: Fields) -> Result<Self, String> {
        let mut fields = (*self.context).clone();
        fields.extend(context.0?);
        Ok(Self {
            state: self.state.clone(),
            context: Arc::new(fields::checked(fields)?),
        })
    }
    fn enabled(&self, severity: &str) -> Result<bool, String> {
        let severity = level(severity)?;
        Ok(severity < 5
            && severity >= self.state.level.load(Ordering::Relaxed)
            && !self.state.closed.load(Ordering::Relaxed))
    }
    fn set_level(&self, severity: &str) -> Result<(), String> {
        self.state.level.store(level(severity)?, Ordering::Relaxed);
        Ok(())
    }
    async fn write(
        self,
        severity: String,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        let severity = level(&severity)?;
        if severity == 5 {
            return Err("off is a filter level, not an event level".into());
        }
        self.emit(severity, message, fields, caller).await
    }
    async fn trace(
        self,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        self.emit(0, message, fields, caller).await
    }
    async fn debug(
        self,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        self.emit(1, message, fields, caller).await
    }
    async fn info(
        self,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        self.emit(2, message, fields, caller).await
    }
    async fn warn(
        self,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        self.emit(3, message, fields, caller).await
    }
    async fn error(
        self,
        message: LuaBytes,
        fields: Option<Fields>,
        #[lua(caller)] caller: CallerLocation,
    ) -> Result<bool, String> {
        self.emit(4, message, fields, caller).await
    }
    async fn flush(self, sync: Option<bool>) -> Result<bool, String> {
        let ticket = self
            .state
            .queue
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "logger is closed")?;
        let mut writer = self.state.writer.clone().lock_owned().await;
        run(move || {
            let _ticket = ticket;
            writer.flush(sync.unwrap_or(false))?;
            Ok(true)
        })
        .await
    }
    async fn close(self) -> Result<bool, String> {
        self.state.closed.store(true, Ordering::Relaxed);
        let mut writer = self.state.writer.clone().lock_owned().await;
        run(move || {
            writer.close()?;
            Ok(true)
        })
        .await
    }
}
