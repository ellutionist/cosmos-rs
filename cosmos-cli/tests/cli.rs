use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cosmos-cli"))
}

fn stdin(args: &[&str], source: &str) -> Output {
    let mut child = command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_version_and_missing_script_are_handled_by_clap() {
    let help = command().arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("cosmos-cli") && text.contains("<SCRIPT>"));
    let version = command().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("cosmos-cli ", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(command().output().unwrap().status.code(), Some(2));
    assert_eq!(
        command()
            .arg("--unknown-option")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn stdin_runs_with_builtins_and_forwards_flag_like_unicode_and_empty_args() {
    let result = stdin(
        &["-", "--help", "--version", "-42", "中文", ""],
        r##"
        local a,b,c,d,e = ...
        assert(select("#", ...) == 5)
        assert(a == "--help" and b == "--version" and c == "-42" and d == "中文" and e == "")
        assert(cosmos.network.http.client.new())
        assert(cosmos.test == nil and cosmos.example == nil)
        cosmos.time.sleep(0)
        print("stdin arguments ok")
    "##,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8(result.stdout)
        .unwrap()
        .contains("stdin arguments ok"));
    let separated = stdin(
        &["-", "--", "--help"],
        "local a,b = ...; assert(select('#', ...) == 2 and a == '--' and b == '--help')",
    );
    assert!(
        separated.status.success(),
        "{}",
        String::from_utf8_lossy(&separated.stderr)
    );
    let leading_separator = stdin(&["--", "-", "--help"], "assert((...) == '--help')");
    assert!(
        leading_separator.status.success(),
        "{}",
        String::from_utf8_lossy(&leading_separator.stderr)
    );
}

#[test]
fn script_files_receive_arguments_and_load_network_lua_definitions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let result = command()
        .current_dir(root)
        .arg(root.join("examples/machine.lua"))
        .args(["host", "参数"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8(result.stdout)
        .unwrap()
        .contains("Machine Lua checks passed"));
}

#[test]
fn input_and_lua_failures_have_distinct_exit_codes() {
    let missing = command()
        .arg(format!("missing-script-{}.lua", std::process::id()))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let failed = stdin(&["-"], "error('cli regression')");
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8(failed.stderr)
        .unwrap()
        .contains("cli regression"));
    assert_eq!(stdin(&["-"], "local = invalid").status.code(), Some(1));
}
