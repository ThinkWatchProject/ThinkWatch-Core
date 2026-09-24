//! 桌面端拿这里的规则去扫用户机器上客户端的配置（`tools::rules::scan_rules`、
//! `hidden`）。那个扫描器有一条写死的纪律：**只报告，不改也不删任何文件**。
//! 它自己的代码在桌面端守着这一条，它用到的这两份在这里守着。
//!
//! 一个安全扫描器长出删除能力的那天，会是从某个「顺手」的 PR 开始的。

#[test]
fn what_the_client_scan_uses_cannot_write_or_delete_anything() {
    let here = env!("CARGO_MANIFEST_DIR");
    for f in ["src/hidden.rs", "src/tools/rules.rs"] {
        let src = std::fs::read_to_string(format!("{here}/{f}")).unwrap();
        // 测试里当然要造文件。看的是产品代码那一半
        let src = src.split("#[cfg(test)]").next().unwrap();
        for bad in ["remove_file", "remove_dir", "fs::write", "OpenOptions"] {
            assert!(
                !src.contains(bad),
                "{f} 里出现了 {bad} —— 扫描用的规则不该会写或删任何东西"
            );
        }
    }
}
