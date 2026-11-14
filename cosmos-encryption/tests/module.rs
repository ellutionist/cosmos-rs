//! 通过公开插件注入运行标准向量与 MySQL 认证所需的算法。
use cosmos::Machine;
use cosmos_encoding::Encoding;
use cosmos_encryption::Encryption;

#[test]
fn encryption_matches_standard_and_mariadb_vectors() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Encryption).unwrap();
    machine.add_module(Encoding).unwrap();
    let source = include_str!("../../examples/encryption.lua").replace(
        "examples/certs/rsa-public.pem",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/certs/rsa-public.pem"
        ),
    );
    runtime.block_on(machine.exec(&source)).unwrap();
}
