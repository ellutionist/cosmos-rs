//! 通用文本能力；业务匹配、名称清洗和规范化顺序由应用组合。
#![warn(unreachable_pub)]
mod bridge;
mod chinese;
mod regex;
mod unicode;
mod work;
use cosmos::userdata::LuaBytes;
use cosmos::{Machine, Module};
const MAX_INPUT: usize = 8 * 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
/// 向 Machine 安装 cosmos.text 下的正则、Unicode 与繁简转换模块。
pub struct Text;
impl Module for Text {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        regex::register(machine);
        machine.register::<unicode::Unicode>();
        machine.register::<chinese::Chinese>();
        Ok(())
    }
}
fn input(data: &LuaBytes) -> Result<(), String> {
    if data.len() > MAX_INPUT {
        return Err("text input exceeds 8 MiB".into());
    }
    Ok(())
}
fn utf8(data: Vec<u8>) -> Result<String, String> {
    String::from_utf8(data).map_err(|_| "text must be valid UTF-8".into())
}
fn append(out: &mut String, value: &str) -> Result<(), String> {
    if value.len() > MAX_OUTPUT.saturating_sub(out.len()) {
        return Err("text output exceeds 16 MiB".into());
    }
    out.push_str(value);
    Ok(())
}
fn integer(value: f64, min: usize, max: usize, name: &str) -> Result<usize, String> {
    if !value.is_finite() || value.fract() != 0.0 || value < min as f64 || value > max as f64 {
        return Err(format!(
            "invalid {name} (expected an integer in {min}..={max})"
        ));
    }
    Ok(value as usize)
}
