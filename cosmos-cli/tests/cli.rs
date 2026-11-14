use std::io::Write;
#[cfg(feature = "network")]
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cosmos-cli"));
    command.env_remove("LUA_PATH").env_remove("LUA_CPATH");
    command
}

fn stdin(args: &[&str], source: &str) -> Output {
    stdin_command(command(), args, source)
}

fn stdin_command(mut command: Command, args: &[&str], source: &str) -> Output {
    let mut child = command
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
    assert!(text.contains("Prepend Lua module search templates"));
    assert!(text.contains("Prepend C module search templates"));
    assert!(text.contains("Lua script path (- for stdin)"));
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
        &["-", "--help", "--version", "-42", "\u{4e2d}\u{6587}", ""],
        r##"
        local a,b,c,d,e = ...
        assert(select("#", ...) == 5)
        assert(a == "--help" and b == "--version" and c == "-42" and d == "\228\184\173\230\150\135" and e == "")
        assert(cosmos.test == nil and cosmos.example == nil)
        assert(require('cosmos.fs') == cosmos.fs and cosmos.fs.path.join('a', 'b'))
        assert(require('cosmos.time.datetime') == cosmos.time.datetime)
        assert(cosmos.time.datetime.from_timestamp_ms(0):format('%Y') == '1970')
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
    let path_arguments = stdin(
        &["-", "--cpath", "./argument/?.so", "--path", "./argument/?.lua"],
        "local a,b,c,d = ...; assert(a == '--cpath' and b == './argument/?.so' and c == '--path' and d == './argument/?.lua')",
    );
    assert!(
        path_arguments.status.success(),
        "{}",
        String::from_utf8_lossy(&path_arguments.stderr)
    );
}

#[test]
fn search_paths_preserve_native_defaults_and_order_cli_before_environment() {
    for (field, option, environment, extension) in [
        ("path", "--path", "LUA_PATH", "lua"),
        ("cpath", "--cpath", "LUA_CPATH", "so"),
    ] {
        let read = format!("io.write(package.{field})");
        let defaults = stdin(&["-"], &read);
        assert!(defaults.status.success());
        let defaults = String::from_utf8(defaults.stdout).unwrap();
        assert!(!defaults.is_empty());
        #[cfg(unix)]
        assert!(defaults
            .split(';')
            .any(|path| path == format!("./?.{extension}")));

        let mut command = command();
        command.env(
            environment,
            format!("./env/?.{extension};;./env-last/?.{extension}"),
        );
        let result = stdin_command(
            command,
            &[
                option,
                &format!("./cli/?.{extension};./quo'te\\\u{76ee}\u{5f55}/?.{extension}"),
                option,
                &format!("./cli-last/?.{extension}"),
                "-",
            ],
            &read,
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            format!("./cli/?.{extension};./quo'te\\\u{76ee}\u{5f55}/?.{extension};./cli-last/?.{extension};./env/?.{extension};{defaults};./env-last/?.{extension}")
        );
    }
}

#[test]
fn require_loads_lua_files_and_init_files_from_cli_environment_and_defaults() {
    let root = std::env::temp_dir().join(format!(
        "cosmos-lua-path-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for (directory, origin) in [
        (root.clone(), "default"),
        (root.join("environment"), "environment"),
        (root.join("first'\u{4e2d}\u{6587}"), "first"),
        (root.join("second"), "second"),
    ] {
        std::fs::create_dir_all(directory.join("probe")).unwrap();
        std::fs::create_dir_all(directory.join("bundle")).unwrap();
        std::fs::write(
            directory.join("probe/value.lua"),
            format!("return '{origin}'"),
        )
        .unwrap();
        std::fs::write(
            directory.join("bundle/init.lua"),
            format!("return '{origin}'"),
        )
        .unwrap();
    }
    let environment = format!("{}/environment/?.lua;;", root.display());
    let first = format!("{}/first'\u{4e2d}\u{6587}/?.lua", root.display());
    let second = format!("{}/second/?.lua", root.display());
    let init = format!("{}/first'\u{4e2d}\u{6587}/?/init.lua", root.display());
    let missing = format!("{}/missing/?.lua", root.display());
    let mut results = Vec::new();
    for (paths, environment, module, expected) in [
        (vec![], environment.as_str(), "probe.value", "environment"),
        (
            vec![first.as_str(), second.as_str()],
            environment.as_str(),
            "probe.value",
            "first",
        ),
        (
            vec![second.as_str(), first.as_str()],
            environment.as_str(),
            "probe.value",
            "second",
        ),
        (
            vec![missing.as_str()],
            environment.as_str(),
            "probe.value",
            "environment",
        ),
        (vec![missing.as_str()], ";;", "probe.value", "default"),
        (vec![init.as_str()], environment.as_str(), "bundle", "first"),
    ] {
        let mut command = command();
        command.current_dir(&root).env("LUA_PATH", environment);
        for path in paths {
            command.args(["--path", path]);
        }
        results.push((
            stdin_command(command, &["-"], &format!("print(require('{module}'))")),
            expected,
        ));
    }
    std::fs::remove_dir_all(root).unwrap();
    for (result, expected) in results {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), expected);
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn executable_exports_lua_c_api_for_native_modules() {
    let result = stdin(
        &["-"],
        r#"
        local ffi = require("ffi")
        ffi.cdef[[
            int lua_gettop(void *lua_state);
            int luaL_checkoption(void *lua_state, int index, const char *default_value, const char *const options[]);
            char *luaL_prepbuffer(void *buffer);
            int luaopen_string(void *lua_state);
            int luaJIT_setmode(void *lua_state, int index, int mode);
        ]]
        for _, name in ipairs({"lua_gettop", "luaL_checkoption", "luaL_prepbuffer", "luaopen_string", "luaJIT_setmode"}) do
            assert(ffi.C[name] ~= nil, name .. " is not exported by the CLI")
        end
        "#,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn network_registration_follows_feature() {
    let source = if cfg!(feature = "network") {
        r#"
        local HttpClient = require("cosmos.network.http.client")
        assert(HttpClient == cosmos.network.http.client)
        assert(HttpClient.new())
        local Udp = require("cosmos.network.udp")
        assert(Udp == cosmos.network.udp)
        local socket = assert(Udp.bind("127.0.0.1", 0, 1000))
        socket:close()
        "#
    } else {
        r#"
        assert(cosmos.network == nil)
        assert(not pcall(require, "cosmos.network.http.client"))
        assert(not pcall(require, "cosmos.network.udp"))
        "#
    };
    let result = stdin(&["-"], source);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn codec_and_encryption_registration_follow_features() {
    let source = format!(
        r#"
        if {encoding} then
            local encoding = require("cosmos.encoding")
            assert(encoding == cosmos.encoding)
            assert(encoding.hex == require("cosmos.encoding.hex"))
            assert(encoding.hex.encode("a\0b") == "610062")
            local json = require("cosmos.encoding.json")
            assert(json == encoding.json and json.null == cosmos.null)
            assert(json.encode(json.decode('[]')) == '[]')
            assert(json.encode(json.decode('[1.2300]', {{lossless_numbers=true}})) == '[1.2300]')
            local toml = require("cosmos.encoding.toml")
            local yaml = require("cosmos.encoding.yaml")
            assert(toml == encoding.toml and yaml == encoding.yaml)
            assert(toml.decode("enabled=true").enabled)
            assert(yaml.decode("value: null").value == cosmos.null)
            local xml = require("cosmos.encoding.xml")
            assert(xml == encoding.xml and xml.document == require("cosmos.encoding.xml.document"))
            assert(xml.node == require("cosmos.encoding.xml.node"))
            assert(xml.parse("<x>value</x>"):root():text() == "value")
            assert(toml.number == json.number and yaml.number == json.number)
        else
            assert(cosmos.encoding == nil)
            assert(not pcall(require, "cosmos.encoding"))
            assert(not pcall(require, "cosmos.encoding.json"))
            assert(not pcall(require, "cosmos.encoding.toml"))
            assert(not pcall(require, "cosmos.encoding.yaml"))
            assert(not pcall(require, "cosmos.encoding.xml"))
        end
        if {text} then
            local text=require("cosmos.text")
            assert(text==cosmos.text and text.regex==require("cosmos.text.regex"))
            assert(text.unicode.upper("text")=="TEXT")
            assert(text.regex.new("a+"):is_match("aaa"))
            assert(text.chinese.convert("ASCII","t2s")=="ASCII")
        else
            assert(cosmos.text==nil and not pcall(require,"cosmos.text"))
            assert(not pcall(require,"cosmos.text.regex"))
        end
        if {image} then
            local image=require("cosmos.image")
            assert(image==cosmos.image)
            local value=assert(image.new(1,1))
            assert(image.decode(assert(value:encode("png"))):width()==1)
        else
            assert(cosmos.image==nil and not pcall(require,"cosmos.image"))
        end
        if {encryption} then
            local encryption = require("cosmos.encryption")
            assert(encryption == cosmos.encryption)
            assert(#encryption.sha256("a\0b") == 32)
            assert(encryption.rsa == require("cosmos.encryption.rsa"))
            assert(encryption.ed25519 == require("cosmos.encryption.ed25519"))
            for _, name in ipairs({{"digest", "hmac", "password", "random"}}) do
                assert(encryption[name] == require("cosmos.encryption." .. name))
            end
            assert(encryption.random.bytes(0) == "")
            assert(#encryption.digest.sum("sha256", "binary\0input") == 32)
        else
            assert(cosmos.encryption == nil)
            assert(not pcall(require, "cosmos.encryption"))
        end
    "#,
        text = cfg!(feature = "text"),
        image = cfg!(feature = "image"),
        encoding = cfg!(feature = "encoding"),
        encryption = cfg!(feature = "encryption")
    );
    let result = stdin(&["-"], &source);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(feature = "network")]
#[test]
fn script_files_receive_arguments_and_load_network_lua_definitions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let result = command()
        .current_dir(root)
        .arg(root.join("examples/machine.lua"))
        .args(["host", "\u{53c2}\u{6570}"])
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
fn database_registration_follows_feature() {
    let source = format!(
        r#"
        if {mysql} then
            local mysql = require('cosmos.database.mysql')
            assert(require('cosmos.database').mysql == mysql)
            assert(mysql.parse_url('mysql://localhost/example').database == 'example')
            assert(mysql.quote_sql_str("a\0b") == "X'610062'")
        else
            assert(not pcall(require, 'cosmos.database.mysql'))
        end
        if {redis} then
            local redis = require('cosmos.database.redis')
            assert(require('cosmos.database').redis == redis)
            assert(redis.parse_url('redis://localhost/2').db == 2)
            assert(redis.null == cosmos.null)
        else
            assert(not pcall(require, 'cosmos.database.redis'))
        end
        if not {mysql} and not {redis} then assert(cosmos.database == nil) end
        "#,
        mysql = cfg!(feature = "mysql"),
        redis = cfg!(feature = "redis"),
    );
    let result = stdin(&["-"], &source);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
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
