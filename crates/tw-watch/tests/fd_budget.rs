//! macOS 上 fd 名额紧的时候：kqueue 每盯一个目录、一份文件占一个 fd，而从访达打开的应用
//! 只有 256 个。名额不够就整个换回 FSEvents —— 费电，但一个改动都不漏。
//!
//! **单独一个测试进程、只有一个测试**：要调低整个进程的 fd 软上限、数整个进程开着的 fd，
//! 和别的测试放在一起会互相连累。
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::time::Duration;

const WINDOW: Duration = Duration::from_millis(200);

fn only_md(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "md")
}

/// 此刻整个进程开着几个 fd（列 `/dev/fd` 自己也占一个，每次都一样）
fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

/// 把 fd 软上限调到 `n`：监听最多能占一半
fn set_soft_limit(n: libc::rlim_t) {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: 两个调用都只读写传进去的结构体
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut r), 0);
        r.rlim_cur = n;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &r), 0);
    }
}

fn append(p: &Path, text: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// `n` 个目录，各有 `files` 份算数的文件
fn dirs(root: &Path, prefix: &str, n: usize, files: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let d = root.join(format!("{prefix}{i}"));
            std::fs::create_dir_all(&d).unwrap();
            for j in 0..files {
                std::fs::write(d.join(format!("{j}.md")), "0\n").unwrap();
            }
            d
        })
        .collect()
}

async fn got(rx: &mut tokio::sync::mpsc::Receiver<()>) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await,
        Ok(Some(()))
    )
}

/// 等前面那些改动的信号都收完、静下来
async fn settle(rx: &mut tokio::sync::mpsc::Receiver<()>) {
    while let Ok(Some(())) = tokio::time::timeout(WINDOW * 3, rx.recv()).await {}
}

#[test]
fn with_too_few_fds_it_falls_back_to_fsevents_and_misses_nothing() {
    // 名额 32
    set_soft_limit(64);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let before = open_fds();

        // 8 个目录、16 份文件，名额够：都用 kqueue 盯，用完都还回去
        let some = dirs(root.path(), "some", 8, 2);
        let (w, mut rx) = tw_watch::watch(&some, WINDOW, only_md).unwrap();
        assert_eq!(
            open_fds(),
            before + 1 + 8 + 16,
            "kqueue、8 个目录、16 份文件"
        );
        append(&some[3].join("1.md"), "1\n");
        assert!(got(&mut rx).await, "原地写");
        settle(&mut rx).await;

        // 又多出 10 份：超了名额，换回 FSEvents，手上的都放掉（只剩 kqueue 自己）
        for j in 2..12 {
            std::fs::write(some[0].join(format!("{j}.md")), "0\n").unwrap();
        }
        assert!(got(&mut rx).await, "超名额的那一批新文件");
        settle(&mut rx).await;
        assert_eq!(open_fds(), before + 1, "换回 FSEvents 之后还拿着 fd");
        // 换过去之后照样一个不漏：新文件的原地写、rename 保存
        append(&some[0].join("7.md"), "1\n");
        assert!(got(&mut rx).await, "换回 FSEvents 之后的原地写");
        settle(&mut rx).await;
        std::fs::write(some[5].join("x.tmp"), "2\n").unwrap();
        std::fs::rename(some[5].join("x.tmp"), some[5].join("0.md")).unwrap();
        assert!(got(&mut rx).await, "换回 FSEvents 之后的 rename 保存");
        drop(w);
        assert_eq!(open_fds(), before, "扔掉之后还拿着 fd");

        // 一开始就不够：40 个目录，直接用 FSEvents
        let many = dirs(root.path(), "many", 40, 1);
        let (w, mut rx) = tw_watch::watch(&many, WINDOW, only_md).unwrap();
        assert_eq!(open_fds(), before, "名额不够还占着 fd");
        settle(&mut rx).await;
        append(&many[39].join("0.md"), "1\n");
        assert!(got(&mut rx).await, "FSEvents 盯着的原地写");
        drop(w);

        // 名额还回来了：又能用 kqueue 盯
        let (_w, _rx) = tw_watch::watch(&some[..2], WINDOW, only_md).unwrap();
        assert!(open_fds() > before + 2, "名额没还回来");
    });
}
