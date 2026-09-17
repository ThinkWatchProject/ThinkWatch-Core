use tw_gateway::{L1Result, Step};

fn steps(r: &L1Result) -> Vec<Step> {
    r.segments.iter().map(|s| s.stage.step).collect()
}

#[tokio::test]
#[ignore = "打真实网络，手动跑：cargo test -p tw-gateway --test l1_real -- --ignored --nocapture"]
async fn a_real_https_endpoint_completes_all_three_segments() {
    let r = tw_gateway::l1("https://api.anthropic.com", None).await;
    println!("{r:#?}");
    assert!(r.ok, "{r:?}");
    assert_eq!(steps(&r), [Step::Dns, Step::Tcp, Step::Tls]);
}

#[tokio::test]
#[ignore = "打真实网络"]
async fn an_expired_certificate_fails_at_tls_not_at_tcp() {
    // 证书问题和网络不通是两件事，指错方向的代价是查半天网络。
    let r = tw_gateway::l1("https://expired.badssl.com", None).await;
    println!("{r:#?}");
    assert!(!r.ok);
    assert_eq!(r.failed.map(|s| s.step), Some(Step::Tls), "{r:?}");
    // TCP 是通的 —— 段列表要能证明这一点
    assert_eq!(steps(&r), [Step::Dns, Step::Tcp]);
}
