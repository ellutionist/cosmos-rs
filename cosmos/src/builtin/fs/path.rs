//! 路径仅做词法操作；normalize 不代替检查符号链接后的 canonicalize。
use std::path::{Component, PathBuf};

use super::{decode_path, encode_path, io_error};
pub(super) struct Path;
#[crate::userdata(name = "fs.path")]
impl Path {
    fn join(base: Vec<u8>, child: Vec<u8>) -> Result<Vec<u8>, String> {
        encode_path(decode_path(base)?.join(decode_path(child)?)).map_err(io_error)
    }
    fn parent(path: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        decode_path(path)?
            .parent()
            .map(|p| encode_path(p.to_owned()))
            .transpose()
            .map_err(io_error)
    }
    fn filename(path: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        decode_path(path)?
            .file_name()
            .map(|p| encode_path(PathBuf::from(p)))
            .transpose()
            .map_err(io_error)
    }
    fn stem(path: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        decode_path(path)?
            .file_stem()
            .map(|p| encode_path(PathBuf::from(p)))
            .transpose()
            .map_err(io_error)
    }
    fn extension(path: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        decode_path(path)?
            .extension()
            .map(|p| encode_path(PathBuf::from(p)))
            .transpose()
            .map_err(io_error)
    }
    fn is_absolute(path: Vec<u8>) -> Result<bool, String> {
        Ok(decode_path(path)?.is_absolute())
    }
    fn starts_with(path: Vec<u8>, prefix: Vec<u8>) -> Result<bool, String> {
        Ok(decode_path(path)?.starts_with(decode_path(prefix)?))
    }
    fn strip_prefix(path: Vec<u8>, prefix: Vec<u8>) -> Result<Vec<u8>, String> {
        let path = decode_path(path)?;
        let prefix = decode_path(prefix)?;
        let relative = path
            .strip_prefix(prefix)
            .map_err(|_| "path does not start with prefix")?;
        encode_path(relative.to_owned()).map_err(io_error)
    }
    fn is_normal_relative(path: Vec<u8>) -> Result<bool, String> {
        let path = decode_path(path)?;
        Ok(!path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))))
    }
    fn normalize(path: Vec<u8>) -> Result<Vec<u8>, String> {
        let path = decode_path(path)?;
        let mut result = PathBuf::new();
        for part in path.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    if result.file_name().is_some_and(|p| p != "..") {
                        result.pop();
                    } else if !result.has_root() {
                        result.push(part.as_os_str());
                    }
                }
                _ => result.push(part.as_os_str()),
            }
        }
        if result.as_os_str().is_empty() {
            result.push(".");
        }
        encode_path(result).map_err(io_error)
    }
}
