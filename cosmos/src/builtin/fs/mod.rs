//! builtin 文件系统；路径和文件内容使用原始 Lua 字节串。
mod dir;
mod file;
mod handle;
mod metadata;
mod path;

use crate::ffi::lua_State;
use crate::userdata::register;
use dir::Directory;
use file::{File, Mode};
use metadata::Info;
use std::io::{self, Read, Write};
use std::path::PathBuf;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_INTEGER: f64 = 9_007_199_254_740_991.0;

fn integer(value: f64, min: f64, max: f64, name: &str) -> Result<f64, String> {
    if !value.is_finite() || value.fract() != 0.0 || !(min..=max).contains(&value) {
        return Err(format!(
            "invalid {name} (expected an integer in {min}..={max})"
        ));
    }
    Ok(value)
}
fn decode_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    if bytes.contains(&0) {
        return Err("path must not contain NUL".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(bytes).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| "path must be valid UTF-8 on this platform".into())
    }
}
fn encode_path(path: PathBuf) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(path.into_os_string().into_vec())
    }
    #[cfg(not(unix))]
    {
        path.into_os_string()
            .into_string()
            .map(String::into_bytes)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "path is not valid UTF-8 on this platform",
                )
            })
    }
}
fn io_error(error: io::Error) -> String {
    let kind = error.kind();
    // 部分 Unix/网络挂载的 ENOTSUP 未被标准库归类为 Unsupported。
    #[cfg(unix)]
    let kind = if error.raw_os_error().is_some_and(|code| {
        code == libc::ENOTSUP || code == libc::EOPNOTSUPP || code == libc::ENOSYS
    }) {
        io::ErrorKind::Unsupported
    } else {
        kind
    };
    let class = match kind {
        io::ErrorKind::NotFound => "not found",
        io::ErrorKind::AlreadyExists => "already exists",
        io::ErrorKind::PermissionDenied => "permission denied",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::WouldBlock => "would block",
        io::ErrorKind::CrossesDevices => "cross-device",
        io::ErrorKind::NotADirectory => "not a directory",
        io::ErrorKind::DirectoryNotEmpty => "directory not empty",
        _ => "io",
    };
    format!("{class}: {error}")
}
async fn blocking<R: Send + 'static>(
    operation: impl FnOnce() -> io::Result<R> + Send + 'static,
) -> Result<R, String> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| format!("I/O task failed: {error}"))?
        .map_err(io_error)
}
struct Fs;
#[derive(Default, crate::FromLua)]
#[lua(default)]
struct AtomicOptions {
    no_replace: bool,
    sync: bool,
}
#[crate::userdata(name = "fs")]
impl Fs {
    async fn open(path: Vec<u8>, mode: Option<Mode>) -> Result<File, String> {
        File::open(decode_path(path)?, mode).await
    }
    async fn read_dir(path: Vec<u8>) -> Result<Directory, String> {
        Directory::open(decode_path(path)?).await
    }
    async fn read_file(path: Vec<u8>, limit: Option<f64>) -> Result<Vec<u8>, String> {
        let path = decode_path(path)?;
        let limit = integer(
            limit.unwrap_or(MAX_BYTES as f64),
            0.0,
            MAX_BYTES as f64,
            "read limit",
        )? as u64;
        blocking(move || {
            let file = std::fs::File::open(path)?;
            let mut data = Vec::new();
            file.take(limit + 1).read_to_end(&mut data)?;
            if data.len() as u64 > limit {
                return Err(io::Error::other("read limit exceeded; read in chunks"));
            }
            Ok(data)
        })
        .await
    }
    async fn write_file(path: Vec<u8>, data: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        if data.len() > MAX_BYTES {
            return Err("write exceeds 64 MiB limit; write in chunks".into());
        }
        blocking(move || {
            std::fs::write(path, data)?;
            Ok(true)
        })
        .await
    }
    async fn write_atomic(
        path: Vec<u8>,
        data: Vec<u8>,
        options: Option<AtomicOptions>,
    ) -> Result<bool, String> {
        let path = decode_path(path)?;
        let options = options.unwrap_or_default();
        if data.len() > MAX_BYTES {
            return Err("write exceeds 64 MiB limit; write in chunks".into());
        }
        blocking(move || {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            let permissions = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
                Ok(_) => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(&data)?;
            if let Some(permissions) = permissions {
                // SMB 可能继承正确权限但不允许 chmod，不重复设置相同权限。
                if temporary.as_file().metadata()?.permissions() != permissions {
                    temporary.as_file().set_permissions(permissions)?;
                }
            }
            if options.sync {
                temporary.as_file().sync_all()?;
            }
            let persisted = if options.no_replace {
                temporary.persist_noclobber(&path)
            } else {
                temporary.persist(&path)
            };
            persisted.map_err(|error| error.error)?;
            #[cfg(unix)]
            if options.sync {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(true)
        })
        .await
    }
    async fn try_exists(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || path.try_exists()).await
    }
    /// 显式修改 Unix 权限；调用方决定暂存文件与目标文件的权限策略。
    async fn chmod(path: Vec<u8>, mode: f64) -> Result<bool, String> {
        let path = decode_path(path)?;
        let mode = integer(mode, 0.0, 4095.0, "permission mode")? as u32;
        blocking(move || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
                Ok(true)
            }
            #[cfg(not(unix))]
            {
                let _ = (path, mode);
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Unix permissions are unavailable",
                ))
            }
        })
        .await
    }
    async fn metadata(path: Vec<u8>) -> Result<Info, String> {
        let path = decode_path(path)?;
        blocking(move || Info::new(std::fs::metadata(path)?)).await
    }
    async fn symlink_metadata(path: Vec<u8>) -> Result<Info, String> {
        let path = decode_path(path)?;
        blocking(move || Info::new(std::fs::symlink_metadata(path)?)).await
    }
    async fn canonicalize(path: Vec<u8>) -> Result<Vec<u8>, String> {
        let path = decode_path(path)?;
        blocking(move || std::fs::canonicalize(path).and_then(encode_path)).await
    }
    async fn create_dir(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || {
            std::fs::create_dir(path)?;
            Ok(true)
        })
        .await
    }
    async fn create_dir_all(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || {
            std::fs::create_dir_all(path)?;
            Ok(true)
        })
        .await
    }
    async fn remove_file(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || {
            std::fs::remove_file(path)?;
            Ok(true)
        })
        .await
    }
    async fn remove_dir(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || {
            std::fs::remove_dir(path)?;
            Ok(true)
        })
        .await
    }
    async fn remove_dir_all(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        blocking(move || {
            std::fs::remove_dir_all(path)?;
            Ok(true)
        })
        .await
    }
    async fn rename(from: Vec<u8>, to: Vec<u8>) -> Result<bool, String> {
        let from = decode_path(from)?;
        let to = decode_path(to)?;
        blocking(move || {
            std::fs::rename(from, to)?;
            Ok(true)
        })
        .await
    }
    /// 支持的平台使用系统原子不覆盖重命名，不以先 exists 再 rename 冒充原子操作。
    async fn rename_new(from: Vec<u8>, to: Vec<u8>) -> Result<bool, String> {
        let from = decode_path(from)?;
        let to = decode_path(to)?;
        blocking(move || {
            #[cfg(any(
                target_os = "linux",
                target_vendor = "apple",
                target_os = "android",
                target_os = "redox"
            ))]
            {
                rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    from,
                    rustix::fs::CWD,
                    to,
                    rustix::fs::RenameFlags::NOREPLACE,
                )?;
            }
            #[cfg(not(any(
                target_os = "linux",
                target_vendor = "apple",
                target_os = "android",
                target_os = "redox"
            )))]
            {
                let _ = (from, to);
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic no-replace rename is unavailable",
                ));
            }
            #[allow(unreachable_code)]
            Ok(true)
        })
        .await
    }
    async fn copy(from: Vec<u8>, to: Vec<u8>) -> Result<f64, String> {
        let from = decode_path(from)?;
        let to = decode_path(to)?;
        blocking(move || {
            let size = std::fs::metadata(&from)?.len();
            if size > MAX_INTEGER as u64 {
                return Err(io::Error::other(
                    "file size exceeds exact Lua integer range",
                ));
            }
            let copied = std::fs::copy(from, to)?;
            if copied > MAX_INTEGER as u64 {
                return Err(io::Error::other(
                    "copied size exceeds exact Lua integer range",
                ));
            }
            Ok(copied as f64)
        })
        .await
    }
    async fn read_link(path: Vec<u8>) -> Result<Vec<u8>, String> {
        let path = decode_path(path)?;
        blocking(move || std::fs::read_link(path).and_then(encode_path)).await
    }
}
/// 仅初始化时调用；类型声明路径自动拼装父模块表。
pub(super) unsafe fn register_types(lua_state: *mut lua_State) {
    register::<Fs>(lua_state);
    register::<File>(lua_state);
    register::<Directory>(lua_state);
    register::<path::Path>(lua_state);
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn io_error_classification_supports_migration_fallbacks() {
        assert!(
            super::io_error(std::io::Error::from_raw_os_error(libc::EXDEV))
                .starts_with("cross-device:")
        );
        assert!(
            super::io_error(std::io::Error::from_raw_os_error(libc::ENOENT))
                .starts_with("not found:")
        );
        assert!(
            super::io_error(std::io::Error::from_raw_os_error(libc::ENOTSUP))
                .starts_with("unsupported:")
        );
    }
}
