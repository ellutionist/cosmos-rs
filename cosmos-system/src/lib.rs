//! 不在安装插件时订阅信号，不接管宿主退出策略或 Runtime。
#![warn(unreachable_pub)]
mod signal;
use cosmos::{Machine, Module};
/// 安装 cosmos.system.signal 和 cosmos.system.process。
pub struct System;
impl Module for System {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<signal::Subscription>();
        machine.register::<Process>();
        Ok(())
    }
}
struct Process;
fn bytes(value: &std::ffi::OsStr) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(value.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        value
            .to_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| "process path cannot be represented as UTF-8".into())
    }
}
#[cosmos::userdata(name = "system.process")]
impl Process {
    fn pid() -> u32 {
        std::process::id()
    }
    fn executable() -> Result<Vec<u8>, String> {
        bytes(
            std::env::current_exe()
                .map_err(|e| e.to_string())?
                .as_os_str(),
        )
    }
}
