//! 把控制面契约导出成 TypeScript：
//!
//! ```sh
//! cargo run -p tw-api --features ts --example export_ts -- <目录>
//! ```
//!
//! 写出 `<目录>/tw-api.ts`。CI 靠它确认导出没坏；桌面端也可以直接调
//! `tw_api::ts::export_all`。
fn main() {
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("bindings"));
    match tw_api::ts::export_all(&dir) {
        Ok(p) => println!("{}", p.display()),
        Err(e) => {
            eprintln!("could not write the bindings into {}: {e}", dir.display());
            std::process::exit(1);
        }
    }
}
