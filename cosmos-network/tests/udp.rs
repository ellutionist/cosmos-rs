//! UDP 的 Lua 用例、IPv6、多线程宿主与挂起操作回收。

use std::time::Duration;

use cosmos::Machine;
use cosmos_network::Network;

#[test]
fn udp_datagrams_work_on_injected_runtimes_and_both_ip_families() {
    let ipv6 = std::net::UdpSocket::bind(("::1", 0)).is_ok();
    for multithread in [false, true] {
        let mut builder = if multithread {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        for host in ["127.0.0.1", "::1"] {
            if host == "::1" && !ipv6 {
                eprintln!("IPv6 loopback is unavailable; skipping IPv6 regression");
                continue;
            }
            let mut machine = Machine::new(runtime.handle().clone()).unwrap();
            machine.add_module(Network).unwrap();
            runtime.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    machine.exec_with_args(
                        "=udp.lua",
                        include_str!("../../examples/udp.lua"),
                        &[host.to_owned()],
                    ),
                )
                .await
                .unwrap()
                .unwrap();
            });
        }
    }
}

#[test]
fn pending_udp_receive_keeps_its_socket_alive_until_machine_drop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let peer = runtime
        .block_on(tokio::net::UdpSocket::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = peer.local_addr().unwrap().port();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime
        .block_on(machine.exec(&format!(
            r#"
        local socket = assert(cosmos.network.udp.connect("127.0.0.1", {port}, 1000))
        assert(socket:send("owned") == 5)
        socket:settimeout(0)
        local started = false
        cosmos.thread.spawn(function()
            started = true
            socket:receive()
        end)
        while not started do cosmos.time.sleep(0) end
        collectgarbage("collect")
    "#
        )))
        .unwrap();
    let mut data = [0; 16];
    let (count, address) = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut data))
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(&data[..count], b"owned");
    assert!(std::net::UdpSocket::bind(address).is_err());
    drop(machine);
    let rebound = std::net::UdpSocket::bind(address).unwrap();
    assert_eq!(rebound.local_addr().unwrap(), address);
}
