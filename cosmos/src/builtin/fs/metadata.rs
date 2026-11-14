use std::fs::Metadata;
use std::time::{SystemTime, UNIX_EPOCH};

use super::MAX_INTEGER;

#[derive(crate::PushLua)]
pub(super) struct Info {
    kind: &'static str,
    size: f64,
    readonly: bool,
    modified_ms: Option<i64>,
    accessed_ms: Option<i64>,
    created_ms: Option<i64>,
    modified_ns: Option<String>,
    accessed_ns: Option<String>,
    created_ns: Option<String>,
    mode: Option<f64>,
}
fn millis(time: std::io::Result<SystemTime>) -> Option<i64> {
    let value = match time.ok()?.duration_since(UNIX_EPOCH) {
        Ok(value) => i128::try_from(value.as_millis()).ok()?,
        Err(error) => {
            let d = error.duration();
            -i128::try_from(d.as_millis()).ok()? - i128::from(d.subsec_nanos() % 1_000_000 != 0)
        }
    };
    if value.unsigned_abs() > MAX_INTEGER as u128 {
        return None;
    }
    value.try_into().ok()
}
fn nanos(time: std::io::Result<SystemTime>) -> Option<String> {
    let value = match time.ok()?.duration_since(UNIX_EPOCH) {
        Ok(value) => i128::try_from(value.as_nanos()).ok()?,
        Err(error) => -i128::try_from(error.duration().as_nanos()).ok()?,
    };
    Some(value.to_string())
}
impl Info {
    pub(super) fn new(value: Metadata) -> std::io::Result<Self> {
        if value.len() > MAX_INTEGER as u64 {
            return Err(std::io::Error::other(
                "file size exceeds exact Lua integer range",
            ));
        }
        let kind = if value.is_file() {
            "file"
        } else if value.is_dir() {
            "directory"
        } else if value.file_type().is_symlink() {
            "symlink"
        } else {
            "other"
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some((value.permissions().mode() & 0o7777) as f64)
        };
        #[cfg(not(unix))]
        let mode = None;
        Ok(Self {
            kind,
            size: value.len() as f64,
            readonly: value.permissions().readonly(),
            modified_ms: millis(value.modified()),
            accessed_ms: millis(value.accessed()),
            created_ms: millis(value.created()),
            modified_ns: nanos(value.modified()),
            accessed_ns: nanos(value.accessed()),
            created_ns: nanos(value.created()),
            mode,
        })
    }
}
