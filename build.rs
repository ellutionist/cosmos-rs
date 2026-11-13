fn main() {
    // LuaJIT 默认 external unwind（lua_error/lua_yield 抛 DWARF 异常而非 longjmp，
    // 见 LuaJIT Makefile 对 Darwin 的硬编码及 Linux 的自动检测）。
    // 该异常穿越 Rust 的 extern "C" 帧会被 Rust 运行时拦截成
    // "panic in a function that cannot unwind" → abort。
    //
    // 经 STATIC_CC 注入 -DLUAJIT_NO_UNWIND 强制 longjmp 模式：
    // - Makefile 的 TARGET_CC = $(STATIC_CC)，所有 C/asm 编译都带此宏
    // - Makefile:236 用 TARGET_CC 预处理 lj_arch.h 做架构检测，
    //   检测结果带 "LJ_NO_UNWIND 1" 后会自动加 DASM_AFLAGS NO_UNWIND
    //   （lj_arch.h 中 LUAJIT_NO_UNWIND 优先于 LUAJIT_UNWIND_EXTERNAL）
    let compiler = cc::Build::new().get_compiler();
    let mut static_cc = format!(
        "{} {}",
        compiler.path().display(),
        compiler.cflags_env().to_str().unwrap()
    );
    static_cc.push_str(" -DLUAJIT_NO_UNWIND");
    std::env::set_var("STATIC_CC", static_cc);

    let mut build = luajit_src::Build::new();
    let artifacts = build.build();
    artifacts.print_cargo_metadata();
}
