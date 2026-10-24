//! 交接时的监听（升级不断线）：交出去的是同一个内核套接字，旧的停止 accept 只丢自己的
//! 句柄，新的照样在听；新的提交之前不跟配置换监听。
//!
//! 这里不跨进程：两个「core」是同一个进程里的两份 `serve_with`，套接字用复制出来的句柄
//! 交 —— 跨进程那一段（SCM_RIGHTS / WSADuplicateSocketW）是 `tw-handover` 的事，内核
//! 看到的东西是一样的：同一个套接字的两个描述符。

use std::net::SocketAddr;
use std::time::Duration;

use tw_config::{Client, Config};
use tw_gateway::listen::Handoff;

fn cfg(port: u16) -> Config {
    let mut c = Config {
        clients: vec![Client {
            name: "c".into(),
            key: "tw-k".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    c.listen.gateway.port = port;
    c
}

/// 网关答不答（没有上游时也会答一句错误：答了就是在听）
async fn answers(addr: SocketAddr) -> bool {
    reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .header("x-api-key", "tw-k")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

#[tokio::test]
async fn the_old_side_stops_accepting_and_the_shared_socket_keeps_listening() {
    let c = cfg(0);
    let old = tw_gateway::AppState::new(c.clone()).unwrap();
    let new = tw_gateway::AppState::new(c.clone()).unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();

    // 旧的在一个现成的监听上服务（起始那一套也可以是交过来的）
    let old_hand = Handoff::default();
    let serving_old = tokio::spawn(tw_gateway::listen::serve_with(
        old.clone(),
        c.listen.gateway.clone(),
        false,
        old_hand.clone(),
        vec![(addr, l)],
    ));
    old_hand.listening().await;
    assert!(answers(addr).await, "旧的没在听");
    assert_eq!(old.listening().primary(), Some(addr));

    // 交出去：复制出来的句柄，同一个地址
    let exported = old_hand.export().unwrap();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].0, addr);
    assert_eq!(exported[0].1.local_addr().unwrap(), addr);

    // 新的在交过来的上面服务，提交之前先不跟配置走
    let new_hand = Handoff::default();
    new_hand.hold();
    tokio::spawn(tw_gateway::listen::serve_with(
        new.clone(),
        c.listen.gateway.clone(),
        true,
        new_hand.clone(),
        exported,
    ));
    new_hand.listening().await;

    // 提交：旧的停止 accept。它的服务结束、句柄丢了，**套接字还在听**
    old_hand.stop_accepting();
    tokio::time::timeout(Duration::from_secs(5), serving_old)
        .await
        .expect("停止 accept 之后 serve_with 该返回")
        .unwrap()
        .unwrap();
    for _ in 0..20 {
        if old_hand.serving() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(old_hand.serving(), 0, "旧的服务没结束");
    assert!(old_hand.export().unwrap().is_empty(), "停了还交得出监听");
    for _ in 0..10 {
        assert!(answers(addr).await, "旧的丢了句柄，端口跟着关了");
    }
    new_hand.follow();
}

#[tokio::test]
async fn a_held_listener_does_not_follow_the_config_until_told() {
    let spare = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let elsewhere = spare.local_addr().unwrap().port();
    drop(spare);

    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    // 配置说的是另一个端口：交过来的地址和配置不一样
    let c = cfg(elsewhere);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let hand = Handoff::default();
    hand.hold();
    tokio::spawn(tw_gateway::listen::serve_with(
        state.clone(),
        c.listen.gateway.clone(),
        true,
        hand.clone(),
        vec![(addr, l)],
    ));
    hand.listening().await;
    // 叫它对一遍配置：提交之前不该动
    state.relisten();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        state.listening().primary(),
        Some(addr),
        "提交之前就换了监听"
    );

    // 提交之后跟配置走：马上换过去
    hand.follow();
    let moved = SocketAddr::from(([127, 0, 0, 1], elsewhere));
    for _ in 0..40 {
        if state.listening().primary() == Some(moved) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        state.listening().primary(),
        Some(moved),
        "跟配置走之后没换过去"
    );
    assert!(answers(moved).await);
}

#[tokio::test]
async fn a_released_listener_is_not_kept_alive_by_the_handoff() {
    // 换监听时退场的那个要真的放开端口：交接把手里攥着一份的话，「旧的先让出来」那一步
    // 就永远绑不上
    let p = {
        let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let mut c = cfg(p);
    let state = tw_gateway::AppState::new(c.clone()).unwrap();
    let hand = Handoff::default();
    tokio::spawn(tw_gateway::listen::serve_with(
        state.clone(),
        c.listen.gateway.clone(),
        true,
        hand.clone(),
        Vec::new(),
    ));
    hand.listening().await;
    let q = {
        let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    c.listen.gateway.port = q;
    state.reload(c).unwrap();
    for _ in 0..40 {
        if state.listening().primary().map(|a| a.port()) == Some(q) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state.listening().primary().map(|a| a.port()), Some(q));
    let exported = hand.export().unwrap();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].0.port(), q, "交出去的该是新的那个");
    drop(exported);
    // 旧端口放开了：绑得上
    let mut freed = false;
    for _ in 0..40 {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(freed, "退场的监听没放开端口");
}
