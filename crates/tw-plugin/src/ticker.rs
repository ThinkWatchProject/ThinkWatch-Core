//! 推进纪元的后台线程。
//!
//! 每个沙箱的截止纪元都设成「当前 + 1」：线程每推一格，正在跑的沙箱就进一次
//! 回调，回调量这个线程真用了多少 CPU，超了才打断（见 `sandbox.rs`）。所以这里
//! 的节拍只决定**多久查一次**，不决定预算本身：macOS 上 `sleep(1ms)` 常睡到
//! 1.5 ms，结果只是超出预算后最多再多跑一格，预算照样按实际的 CPU 时间算。
//!
//! 没有沙箱在跑的时候它停着（park），不在桌面上每秒白醒一千次。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, Thread};
use std::time::Duration;

use wasmtime::Engine;

const TICK: Duration = Duration::from_millis(1);

pub(crate) struct Ticker {
    shared: Arc<Shared>,
    thread: Thread,
}

struct Shared {
    active: AtomicUsize,
    stop: AtomicBool,
}

/// 只要有一个守卫活着，线程就在走
pub(crate) struct Running<'a>(&'a Ticker);

impl Ticker {
    pub(crate) fn start(engine: Engine) -> std::io::Result<Ticker> {
        let shared = Arc::new(Shared {
            active: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let s = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("tw-plugin-epoch".into())
            .spawn(move || run(engine, s))?;
        Ok(Ticker {
            shared,
            thread: handle.thread().clone(),
        })
    }

    pub(crate) fn enter(&self) -> Running<'_> {
        if self.shared.active.fetch_add(1, Ordering::SeqCst) == 0 {
            self.thread.unpark();
        }
        Running(self)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.shared.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn run(engine: Engine, s: Arc<Shared>) {
    loop {
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        if s.active.load(Ordering::SeqCst) == 0 {
            // unpark 先到也没关系：park 会立刻返回，下一圈重新看
            thread::park();
            continue;
        }
        thread::sleep(TICK);
        engine.increment_epoch();
    }
}
