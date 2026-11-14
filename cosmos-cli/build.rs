#![warn(unreachable_pub)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let (option, prefix) = match target_os.as_str() {
        "macos" => ("-Wl,-exported_symbol,", "_"),
        "linux" => ("-Wl,--export-dynamic-symbol=", ""),
        _ => return,
    };

    // C 扩展会在运行时查找这些 API，最终 CLI 必须保留并导出未被 Rust 直接引用的函数。
    // 配置限于本二进制，不改变 cosmos 的 Rust 可见性，也不传播到其他 Machine 宿主。
    for pattern in ["lua_*", "luaL_*", "luaopen_*", "luaJIT_*"] {
        println!("cargo:rustc-link-arg-bin=cosmos-cli={option}{prefix}{pattern}");
    }
}
