//! 文件 I/O 只在原生阻塞任务执行；日/大小轮转均在同一 writer 锁内完成。
use crate::fields::{self, Object};
use chrono::{DateTime, NaiveDate};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Clone, Copy)]
pub(super) enum Console {
    None,
    Stdout,
    Stderr,
    Host,
}
pub(super) struct Config {
    pub(super) path: Option<PathBuf>,
    pub(super) daily: bool,
    pub(super) max_bytes: u64,
    pub(super) keep_files: usize,
    pub(super) keep_days: i64,
    pub(super) console: Console,
}
pub(super) struct Record {
    pub(super) timestamp: String,
    pub(super) level: &'static str,
    pub(super) message: String,
    pub(super) fields: Object,
    pub(super) source: String,
    pub(super) line: u32,
}
impl Record {
    fn render(&self) -> Result<Vec<u8>, String> {
        let source = format!("{:?}", self.source);
        let message = format!("{:?}", self.message);
        let context = fields::text(&self.fields);
        let bytes = format!(
            "{} {} {}:{} {} {}\n",
            self.timestamp,
            self.level,
            &source[1..source.len() - 1],
            self.line,
            &message[1..message.len() - 1],
            context
        )
        .into_bytes();
        if bytes.len() > 256 * 1024 {
            return Err("encoded log record exceeds 256 KiB".into());
        }
        Ok(bytes)
    }
}
pub(super) struct Writer {
    config: Config,
    file: Option<File>,
    path: Option<PathBuf>,
    date: NaiveDate,
    bytes: u64,
    failure: Option<String>,
}
fn suffix(path: &Path, index: usize) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(format!(".{index}"));
    value.into()
}
fn dated(path: &Path, date: NaiveDate) -> Result<PathBuf, String> {
    let mut name = path
        .file_stem()
        .ok_or("log path must name a file")?
        .to_os_string();
    name.push(format!(".{date}"));
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    Ok(path.with_file_name(name))
}
fn io(error: std::io::Error) -> String {
    format!("log I/O failed: {error}")
}
fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(e)),
    }
}
impl Writer {
    pub(super) fn new(config: Config) -> Result<Self, String> {
        let mut writer = Self {
            config,
            file: None,
            path: None,
            date: crate::now().date_naive(),
            bytes: 0,
            failure: None,
        };
        writer.open(writer.date)?;
        Ok(writer)
    }
    fn open(&mut self, date: NaiveDate) -> Result<(), String> {
        self.date = date;
        let Some(base) = &self.config.path else {
            return Ok(());
        };
        let path = if self.config.daily {
            dated(base, date)?
        } else {
            base.clone()
        };
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(io)?;
        if let Ok(meta) = std::fs::metadata(&path) {
            if !meta.is_file() {
                return Err("log output must be a regular file".into());
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        if !file.metadata().map_err(io)?.is_file() {
            return Err("log output must be a regular file".into());
        }
        self.bytes = file.metadata().map_err(io)?.len();
        self.file = Some(file);
        self.path = Some(path);
        self.prune(date)
    }
    fn prune(&self, date: NaiveDate) -> Result<(), String> {
        if !self.config.daily || self.config.keep_days == 0 {
            return Ok(());
        }
        let Some(base) = &self.config.path else {
            return Ok(());
        };
        let cutoff = date
            .checked_sub_signed(chrono::TimeDelta::days(self.config.keep_days - 1))
            .ok_or("log retention date is out of range")?;
        let parent = base
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for entry in std::fs::read_dir(parent).map_err(io)? {
            let entry = entry.map_err(io)?;
            let path = entry.path();
            // 只删除能由当前文件模板生成的精确名字；不按用户输入的任意前缀批量删除。
            let Some(name) = path.file_name().map(|name| name.as_encoded_bytes()) else {
                continue;
            };
            for start in 0..name.len().saturating_sub(9) {
                if let Ok(text) = std::str::from_utf8(&name[start..start + 10]) {
                    if let Ok(day) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
                        if day < cutoff {
                            let expected = dated(base, day)?;
                            if path.file_name() == expected.file_name()
                                || (1..=64)
                                    .any(|n| path.file_name() == suffix(&expected, n).file_name())
                            {
                                remove(&path)?;
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn rotate(&mut self) -> Result<(), String> {
        self.file.take();
        let path = self.path.as_ref().ok_or("missing log output path")?;
        if self.config.keep_files == 0 {
            remove(path)?;
        } else {
            remove(&suffix(path, self.config.keep_files))?;
            for index in (1..self.config.keep_files).rev() {
                let from = suffix(path, index);
                if from.exists() {
                    std::fs::rename(from, suffix(path, index + 1)).map_err(io)?;
                }
            }
            std::fs::rename(path, suffix(path, 1)).map_err(io)?;
        }
        self.open(self.date)
    }
    pub(super) fn write(
        &mut self,
        record: Record,
        dispatch: tracing::Dispatch,
    ) -> Result<(), String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let bytes = record.render()?;
        let result = self.write_inner(record, dispatch, bytes);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }
    fn write_inner(
        &mut self,
        record: Record,
        dispatch: tracing::Dispatch,
        file_bytes: Vec<u8>,
    ) -> Result<(), String> {
        let date = DateTime::parse_from_rfc3339(&record.timestamp)
            .map_err(|_| "invalid log timestamp")?
            .date_naive();
        if self.config.daily && date != self.date {
            self.file.take();
            self.open(date)?;
        }
        if self.file.is_some() {
            if self.config.max_bytes > 0
                && self.bytes > 0
                && self.bytes.saturating_add(file_bytes.len() as u64) > self.config.max_bytes
            {
                self.rotate()?;
            }
            self.file
                .as_mut()
                .ok_or("log output is closed")?
                .write_all(&file_bytes)
                .map_err(io)?;
            self.bytes += file_bytes.len() as u64;
        }
        match self.config.console {
            Console::None => {}
            Console::Stdout => std::io::stdout()
                .lock()
                .write_all(&file_bytes)
                .map_err(io)?,
            Console::Stderr => std::io::stderr()
                .lock()
                .write_all(&file_bytes)
                .map_err(io)?,
            Console::Host => tracing::dispatcher::with_default(&dispatch, || {
                let fields = fields::text(&record.fields);
                let source = format!("{:?}", record.source);
                let text = format!("{:?}", record.message);
                let message = format!(
                    "{}:{} {}",
                    &source[1..source.len() - 1],
                    record.line,
                    &text[1..text.len() - 1]
                );
                match record.level {
                    "TRACE" => {
                        tracing::event!(target:"cosmos.lua",tracing::Level::TRACE,message=%message,lua_file=%record.source,lua_line=record.line,fields=%fields)
                    }
                    "DEBUG" => {
                        tracing::event!(target:"cosmos.lua",tracing::Level::DEBUG,message=%message,lua_file=%record.source,lua_line=record.line,fields=%fields)
                    }
                    "INFO" => {
                        tracing::event!(target:"cosmos.lua",tracing::Level::INFO,message=%message,lua_file=%record.source,lua_line=record.line,fields=%fields)
                    }
                    "WARN" => {
                        tracing::event!(target:"cosmos.lua",tracing::Level::WARN,message=%message,lua_file=%record.source,lua_line=record.line,fields=%fields)
                    }
                    _ => {
                        tracing::event!(target:"cosmos.lua",tracing::Level::ERROR,message=%message,lua_file=%record.source,lua_line=record.line,fields=%fields)
                    }
                }
            }),
        }
        Ok(())
    }
    pub(super) fn flush(&mut self, sync: bool) -> Result<(), String> {
        if let Some(file) = &mut self.file {
            file.flush().map_err(io)?;
            if sync {
                file.sync_data().map_err(io)?;
            }
        }
        match self.config.console {
            Console::Stdout => std::io::stdout().flush().map_err(io)?,
            Console::Stderr => std::io::stderr().flush().map_err(io)?,
            _ => {}
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(())
    }
    pub(super) fn close(&mut self) -> Result<(), String> {
        let result = self.flush(false);
        self.file.take();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_rotation_prunes_only_owned_files_and_never_uses_json() {
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().join("app.2000-01-01.log");
        let config = Config {
            path: Some(base.clone()),
            daily: true,
            max_bytes: 0,
            keep_files: 2,
            keep_days: 1,
            console: Console::None,
        };
        let mut writer = Writer::new(config).unwrap();
        let unrelated = scratch.path().join("other.2000-01-01.log");
        std::fs::write(&unrelated, b"keep").unwrap();
        for day in ["2024-01-01", "2024-01-03"] {
            writer
                .write(
                    Record {
                        timestamp: format!("{day}T00:00:00.000Z"),
                        level: "INFO",
                        message: "event".into(),
                        fields: Object::new(),
                        source: "daily.lua".into(),
                        line: 17,
                    },
                    tracing::Dispatch::none(),
                )
                .unwrap();
        }
        writer.close().unwrap();
        assert!(!dated(&base, NaiveDate::from_ymd_opt(2024, 1, 1).unwrap())
            .unwrap()
            .exists());
        let content = std::fs::read_to_string(
            dated(&base, NaiveDate::from_ymd_opt(2024, 1, 3).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(content.contains("daily.lua:17 event") && !content.starts_with('{'));
        assert_eq!(std::fs::read(unrelated).unwrap(), b"keep");
    }
    #[cfg(unix)]
    #[test]
    fn daily_retention_preserves_native_file_names() {
        use std::os::unix::ffi::OsStringExt;
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().join(std::ffi::OsString::from_vec({
            // Linux 允许任意非 NUL 字节；macOS 文件系统要求有效 UTF-8。
            #[cfg(target_os = "linux")]
            let bytes = b"log-\xff.log".to_vec();
            #[cfg(not(target_os = "linux"))]
            let bytes = "log-\u{4e2d}\u{6587}.log".as_bytes().to_vec();
            bytes
        }));
        let mut writer = Writer::new(Config {
            path: Some(base.clone()),
            daily: true,
            max_bytes: 0,
            keep_files: 0,
            keep_days: 1,
            console: Console::None,
        })
        .unwrap();
        for day in ["2024-01-01", "2024-01-02"] {
            writer
                .write(
                    Record {
                        timestamp: format!("{day}T00:00:00Z"),
                        level: "INFO",
                        message: "event".into(),
                        fields: Object::new(),
                        source: "test.lua".into(),
                        line: 1,
                    },
                    tracing::Dispatch::none(),
                )
                .unwrap();
        }
        writer.close().unwrap();
        assert!(!dated(&base, NaiveDate::from_ymd_opt(2024, 1, 1).unwrap())
            .unwrap()
            .exists());
    }
}
