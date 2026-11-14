#![warn(unreachable_pub)]

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use cosmos::Machine;
#[cfg(any(feature = "mysql", feature = "redis"))]
use cosmos_database::Database;
#[cfg(feature = "encoding")]
use cosmos_encoding::Encoding;
#[cfg(feature = "encryption")]
use cosmos_encryption::Encryption;
#[cfg(feature = "image")]
use cosmos_image::Image;
#[cfg(feature = "logging")]
use cosmos_logging::Logging;
#[cfg(feature = "network")]
use cosmos_network::Network;
#[cfg(feature = "system")]
use cosmos_system::System;
#[cfg(feature = "text")]
use cosmos_text::Text;

#[derive(Parser)]
#[command(
    name = "cosmos-cli",
    version,
    about = "Run Lua scripts with a cosmos Machine"
)]
struct Args {
    /// 前置 Lua 模块搜索模板（如 ./lib/?.lua;./lib/?/init.lua），可重复指定
    #[arg(long, value_name = "PATH", help = "Prepend Lua module search templates (e.g. ./lib/?.lua;./lib/?/init.lua); may be repeated", long_help = None)]
    path: Vec<String>,

    /// 前置 C 扩展搜索模板（如 ./lib/?.so），可重复指定或用分号分隔
    #[arg(long, value_name = "PATH", help = "Prepend C module search templates (e.g. ./lib/?.so); repeat or separate with semicolons", long_help = None)]
    cpath: Vec<String>,

    /// Lua 脚本路径（- 表示标准输入），后续参数通过 ... 传给脚本
    #[arg(
        value_names = ["SCRIPT", "ARG"],
        help = "Lua script path (- for stdin); remaining arguments are passed to the script via ...",
        long_help = None,
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
        eprintln!("script arguments must be valid UTF-8 strings");
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
            eprintln!("failed to read {}: {error}", script.display());
            return ExitCode::from(2);
        }
    };
    match run(&script, &source, &script_args, &args.path, &args.cpath) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("execution failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    script: &Path,
    source: &str,
    script_args: &[String],
    path: &[String],
    cpath: &[String],
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to create Tokio runtime: {error}"))?;
    let mut machine = Machine::new(runtime.handle().clone())?;
    machine.prepend_path(&path.join(";"))?;
    machine.prepend_cpath(&cpath.join(";"))?;
    #[cfg(feature = "encoding")]
    machine.add_module(Encoding)?;
    #[cfg(feature = "image")]
    machine.add_module(Image)?;
    #[cfg(feature = "text")]
    machine.add_module(Text)?;
    #[cfg(feature = "encryption")]
    machine.add_module(Encryption)?;
    #[cfg(feature = "network")]
    machine.add_module(Network)?;
    #[cfg(feature = "system")]
    machine.add_module(System)?;
    #[cfg(feature = "logging")]
    machine.add_module(Logging)?;
    #[cfg(any(feature = "mysql", feature = "redis"))]
    machine.add_module(Database)?;

    let name = if script == Path::new("-") {
        "=stdin".to_string()
    } else {
        format!("@{}", script.display())
    };
    runtime.block_on(machine.exec_with_args(&name, source, script_args))
}
