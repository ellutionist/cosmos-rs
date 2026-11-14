use std::fs::{File as StdFile, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

use super::handle::Handle;
use super::metadata::Info;
use super::{blocking, integer, MAX_BYTES, MAX_INTEGER};

#[derive(Default, crate::FromLua)]
#[lua(default)]
pub(super) struct Flags {
    read: Option<bool>,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}
#[derive(crate::FromLua)]
pub(super) enum Mode {
    #[lua(type = "string")]
    Named(String),
    #[lua(type = "table")]
    Options(Flags),
}
#[derive(Clone)]
pub(crate) struct File(Handle<State>);
struct State {
    file: StdFile,
    locked: bool,
}
impl File {
    pub(super) async fn open(path: PathBuf, mode: Option<Mode>) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        match mode.unwrap_or(Mode::Named("r".into())) {
            Mode::Named(mode) => match mode.as_str() {
                "r" => {
                    options.read(true);
                }
                "r+" => {
                    options.read(true).write(true);
                }
                "w" => {
                    options.write(true).create(true).truncate(true);
                }
                "w+" => {
                    options.read(true).write(true).create(true).truncate(true);
                }
                "a" => {
                    options.append(true).create(true);
                }
                "a+" => {
                    options.read(true).append(true).create(true);
                }
                "wx" => {
                    options.write(true).create_new(true);
                }
                "w+x" => {
                    options.read(true).write(true).create_new(true);
                }
                _ => return Err("invalid file mode".into()),
            },
            Mode::Options(flags) => {
                options
                    .read(flags.read.unwrap_or(!flags.write && !flags.append))
                    .write(flags.write)
                    .append(flags.append)
                    .truncate(flags.truncate)
                    .create(flags.create)
                    .create_new(flags.create_new);
            }
        }
        let file = blocking(move || options.open(path)).await?;
        Ok(Self(Handle::new(State {
            file,
            locked: false,
        })))
    }
}
impl File {
    pub(crate) fn abort(&self) {
        self.0.abort();
    }
    pub(crate) async fn stream_info(&self) -> Result<(u64, Option<std::time::SystemTime>), String> {
        self.0
            .run(|state| {
                let metadata = state.file.metadata()?;
                if !metadata.is_file() {
                    return Err(io::Error::other("file source requires a regular file"));
                }
                Ok((metadata.len(), metadata.modified().ok()))
            })
            .await
    }
    pub(crate) async fn read_at(&self, offset: u64, size: usize) -> Result<Vec<u8>, String> {
        if size > 16 * 1024 * 1024 {
            return Err("file read size exceeds 16 MiB".into());
        }
        self.0
            .run(move |state| {
                let mut data = vec![0; size];
                #[cfg(unix)]
                let count = {
                    use std::os::unix::fs::FileExt;
                    state.file.read_at(&mut data, offset)?
                };
                #[cfg(not(unix))]
                let count = {
                    // 其他平台在同一文件调度锁内恢复游标，不让桥接读取改变应用的游标。
                    let original = state.file.stream_position()?;
                    state.file.seek(SeekFrom::Start(offset))?;
                    let result = state.file.read(&mut data);
                    state.file.seek(SeekFrom::Start(original))?;
                    result?
                };
                data.truncate(count);
                Ok(data)
            })
            .await
    }
}
#[crate::userdata(name = "fs.file")]
impl File {
    /// EOF 为 nil；read(0) 为一个空字符串，不探测 EOF。
    async fn read(self, size: Option<f64>) -> Result<Option<Vec<u8>>, String> {
        let size = integer(
            size.unwrap_or(8192.0),
            0.0,
            16.0 * 1024.0 * 1024.0,
            "read size",
        )? as usize;
        self.0
            .run(move |state| {
                let mut data = vec![0; size];
                let count = state.file.read(&mut data)?;
                data.truncate(count);
                Ok(if count == 0 && size != 0 {
                    None
                } else {
                    Some(data)
                })
            })
            .await
    }
    /// 成功写完全部输入；与原生文件一致，失败前可能已写入部分内容。
    async fn write(self, data: Vec<u8>) -> Result<f64, String> {
        if data.len() > MAX_BYTES {
            return Err("write exceeds 64 MiB limit; write in chunks".into());
        }
        self.0
            .run(move |state| {
                state.file.write_all(&data)?;
                Ok(data.len() as f64)
            })
            .await
    }
    async fn seek(self, offset: f64, whence: Option<String>) -> Result<f64, String> {
        let offset = integer(offset, -MAX_INTEGER, MAX_INTEGER, "seek offset")?;
        let position = match whence.as_deref().unwrap_or("set") {
            "set" if offset >= 0.0 => SeekFrom::Start(offset as u64),
            "cur" => SeekFrom::Current(offset as i64),
            "end" => SeekFrom::End(offset as i64),
            _ => return Err("invalid seek origin or negative absolute offset".into()),
        };
        self.0
            .run(move |state| {
                let value = state.file.seek(position)?;
                if value > MAX_INTEGER as u64 {
                    return Err(io::Error::other("position exceeds exact Lua integer range"));
                }
                Ok(value as f64)
            })
            .await
    }
    async fn truncate(self, size: f64) -> Result<bool, String> {
        let size = integer(size, 0.0, MAX_INTEGER, "file size")? as u64;
        self.0
            .run(move |state| {
                state.file.set_len(size)?;
                Ok(true)
            })
            .await
    }
    async fn metadata(self) -> Result<Info, String> {
        self.0.run(|state| Info::new(state.file.metadata()?)).await
    }
    async fn flush(self) -> Result<bool, String> {
        self.0
            .run(|state| {
                state.file.flush()?;
                Ok(true)
            })
            .await
    }
    async fn sync(self, data_only: Option<bool>) -> Result<bool, String> {
        self.0
            .run(move |state| {
                if data_only.unwrap_or(false) {
                    state.file.sync_data()?;
                } else {
                    state.file.sync_all()?;
                }
                Ok(true)
            })
            .await
    }
    /// 文件锁是操作系统建议锁；0 立即尝试，缺省无限等。
    async fn lock(self, mode: Option<String>, timeout_ms: Option<f64>) -> Result<bool, String> {
        let shared = match mode.as_deref().unwrap_or("exclusive") {
            "shared" => true,
            "exclusive" => false,
            _ => return Err("invalid file lock mode".into()),
        };
        let duration = timeout_ms
            .map(|ms| {
                if !ms.is_finite() || !(0.0..=1e12).contains(&ms) {
                    return Err("invalid lock timeout (ms)".to_string());
                }
                Ok(Duration::from_secs_f64(ms / 1000.0))
            })
            .transpose()?;
        let duration = match (duration, self.0.timeout()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let deadline = duration.map(|d| tokio::time::Instant::now() + d);
        loop {
            let acquired = self
                .0
                .run(move |state| {
                    if state.locked {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "file handle is already locked",
                        ));
                    }
                    let result = if shared {
                        state.file.try_lock_shared()
                    } else {
                        state.file.try_lock()
                    };
                    match result {
                        Ok(()) => {
                            state.locked = true;
                            Ok(true)
                        }
                        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
                        Err(std::fs::TryLockError::Error(error)) => Err(error),
                    }
                })
                .await?;
            if acquired {
                return Ok(true);
            }
            let pause = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if left.is_zero() {
                        return Err("timeout".into());
                    }
                    left.min(Duration::from_millis(10))
                }
                None => Duration::from_millis(10),
            };
            tokio::time::sleep(pause).await;
        }
    }
    async fn unlock(self) -> Result<bool, String> {
        self.0
            .run(|state| {
                if state.locked {
                    state.file.unlock()?;
                    state.locked = false;
                }
                Ok(true)
            })
            .await
    }
    fn settimeout(&self, ms: f64) -> Result<bool, String> {
        self.0.settimeout(ms)
    }
    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
    async fn close(self) -> Result<bool, String> {
        self.0.close().await
    }
}
