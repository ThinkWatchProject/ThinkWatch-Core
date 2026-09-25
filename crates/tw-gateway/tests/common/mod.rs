//! 几个测试文件共用的小工具。

use std::net::SocketAddr;

/// 一个空着的端口号：**系统不会自己把它分出去**，也不会再给第二个测试。
///
/// 号要在谁都没绑它之前就定下来时用它：写进配置（换端口、同一个端口换地址），
/// 或者要它一直没人听（连不上的上游）。只要网关起来就行的地方用不着它：
/// `tw_gateway::serve` 自己绑 0，把真的地址交回来。
///
/// **不能绑 0 拿号再放掉。**放掉的号回到系统的临时端口段（Linux 默认
/// 32768–60999，macOS 和 Windows 49152–65535），并行的测试绑 0、发起连接都从
/// 那一段取号：要绑它的，网关绑之前号就被拿走，报一个和代码无关的
/// `gw.listen.port_taken`；要它没人听的，别的测试在上面起了服务，「连不上」就
/// 连上了。这里从那一段下面挑，和 core 挑远程控制端口同一段。
///
/// **号用 UDP 占着，直到进程退出。**UDP 和 TCP 是两套端口：占着 UDP 的这个号，
/// TCP 的照样绑得上；而别的用例、同时在跑的别的测试进程（两个工作目录各跑一遍）
/// 来拿同一个号时，系统说它已经被占了。只在进程里记账的话，后一种管不到。
pub fn spare_port() -> u16 {
    use std::sync::{Mutex, PoisonError};

    static HELD: Mutex<Vec<std::net::UdpSocket>> = Mutex::new(Vec::new());
    for _ in 0..1000 {
        let port = rand::random_range(tw_config::REMOTE_PORT_RANGE);
        let Ok(hold) = std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port))) else {
            continue;
        };
        if free(port, [127, 0, 0, 1]) && free(port, [0, 0, 0, 0]) {
            HELD.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(hold);
            return port;
        }
    }
    panic!("no spare port left in {:?}", tw_config::REMOTE_PORT_RANGE);
}

/// 这个端口的 TCP 此刻绑不绑得上。别的程序正占着的（开发机上可能真有一个 core
/// 在这一段里听）、系统留作他用的（Windows 会成段地保留端口）算绑不上；别的错误
/// 说明这台机器出了别的问题，直接报出来。
fn free(port: u16, ip: [u8; 4]) -> bool {
    use std::io::ErrorKind;
    match std::net::TcpListener::bind(SocketAddr::from((ip, port))) {
        Ok(_) => true,
        Err(e) if matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied) => false,
        Err(e) => panic!("binding port {port} to try it failed: {e}"),
    }
}
