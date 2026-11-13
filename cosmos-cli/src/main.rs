#![warn(unreachable_pub)]

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use cosmos::Machine;
use cosmos_network::Network;

#[derive(Parser)]
#[command(
    name = "cosmos-cli",
    version,
    about = "使用 cosmos Machine 运行 Lua 脚本"
)]
struct Args {
    /// Lua 脚本路径（- 表示标准输入），后续参数通过 ... 传给脚本
    #[arg(
        value_names = ["SCRIPT", "ARG"],
        required = true,
        num_args = 1..,
        trailing_var_arg = true
    )]
    input: Vec<OsString>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let script = PathBuf::from(&args.input[0]);
    let script_args = args.input[1..]
        .iter()
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>();
    let Some(script_args) = script_args else {
        eprintln!("脚本参数必须是有效的 UTF-8 字符串");
        return ExitCode::from(2);
    };
    let source = if script == Path::new("-") {
        io::read_to_string(io::stdin())
    } else {
        std::fs::read_to_string(&script)
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => {
            eprintln!("无法读取 {}: {error}", script.display());
            return ExitCode::from(2);
        }
    };
    match run(&script, &source, &script_args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("执行失败: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(script: &Path, source: &str, script_args: &[String]) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("无法创建 Tokio runtime: {error}"))?;
    let mut machine = Machine::new(runtime.handle().clone())?;
    machine.add_module(Network)?;

    let name = if script == Path::new("-") {
        "=stdin".to_string()
    } else {
        format!("@{}", script.display())
    };
    runtime.block_on(machine.exec_with_args(&name, source, script_args))
}
