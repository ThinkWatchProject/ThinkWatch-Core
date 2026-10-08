//! macOS 上的不递归监听：kqueue。
//!
//! **为什么不用 notify 在 macOS 上默认的 FSEvents。**FSEvents 只会盯一整棵树：
//! `RecursiveMode::NonRecursive` 是 notify 在我们进程里把深处的事件丢掉，而内核照样把
//! 底下每一次写都送过来（零延迟、按文件报），每一次都叫醒我们。桌面端要盯 `~` 本身
//! （`~/.claude.json` 在那儿），实测空闲时 `~` 底下每秒七到十四个事件，没有一个相关 ——
//! 「空闲时接近零」就这么没了。
//!
//! **也不用 notify 的 kqueue 后端**（`macos_kqueue`）：它不递归地盯一个目录时，目录里
//! 一有变化，就把目录里第一个它没盯过的条目当成「新建的」，并且**递归地**盯上它 ——
//! 盯着 `~` 的话，第一次有人在 `~` 里建个文件，它就去把 `~/Library` 整棵树挨个打开。
//!
//! 这里的做法：
//!
//! - **目录**：打开（`O_EVTONLY`：只为了收事件，不妨碍卸载）挂一个 vnode 事件。只有目录
//!   自己的条目增、删、改名（包括 rename 进来的原子保存）才叫醒我们，子目录里再怎么变
//!   都不会。醒了之后列一遍目录，只看算数的条目：多了、少了、换了一个（inode 变了）才算
//!   一次。
//! - **算数的普通文件**：也各挂一个。原地写（不经过 rename 的保存、追加、截断）不动目录，
//!   只有盯着文件本身才看得见。文件被换掉之后，手上那个 fd 指的还是旧的那个 inode ——
//!   它报一个删除或改名，目录那边重新列一遍，就地挂到新的上。
//! - **只看内容和存在性**：属性变了（`NOTE_ATTRIB`）只有大小或修改时间跟着变才算。截断
//!   只报这一个；改权限不算，读文件本来就不报。
//!
//! **fd 有名额。**每盯一个目录或文件占一个 fd，而从访达打开的应用默认只有 256 个。整个
//! 进程里所有监听合起来最多占软上限的一半（[`budget`]）；超了，这个监听整个换回 FSEvents ——
//! 费电，但一个事件都不漏。只盯一部分文件、悄悄漏掉它们的原地写，比费电更糟。
//!
//! **盯的是路径，不是那个 inode**，和 FSEvents 一样：盯着的目录自己被删掉、挪走时算一次
//! （`relevant` 问的是目录自己的路径），然后改盯它的上一层，等这个名字再出现 —— 整个目录
//! 被换掉（rename 一个新的过来）、删了又建，都接着盯新的那个，出现时也算一次。上一层也
//! 没了，才不再盯。

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use crate::{Relevant, WatchError};

/// 目录上要的事件：条目变了（`NOTE_WRITE`），和它自己没了、被挪走
const DIR_EVENTS: u32 = libc::NOTE_WRITE | GONE;

/// 文件上要的事件：内容变了、属性变了（[`Loop::stamp_moved`] 再筛），和它自己没了、被挪走
const FILE_EVENTS: u32 = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | GONE;

/// 手上这个 vnode 不再是那个路径上的东西了：删了、改名挪走了、文件系统卸载了
const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;

/// 叫事件循环停下的那个用户事件（`EVFILT_USER`；和 vnode 事件不在一个名字空间里）
const STOP: usize = 0;

/// fd 名额的硬顶：软上限再高，所有监听合起来也不占更多
const MAX_HELD: usize = 1024;

/// 整个进程里所有监听此刻占着的 fd
static HELD: AtomicUsize = AtomicUsize::new(0);

/// 所有监听合起来最多占多少个 fd：软上限的一半，最多 [`MAX_HELD`]。
///
/// **按进程算，不按监听算**：一个进程里可能同时有好几个（桌面端换一份监听时，新的起来了
/// 旧的才放掉）。每次都现问软上限：进程中途调高了，名额跟着多。
fn budget() -> usize {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit 只往传进去的结构体里写
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } == 0 {
        r.rlim_cur
    } else {
        256
    };
    usize::try_from(soft / 2).unwrap_or(MAX_HELD).min(MAX_HELD)
}

/// 一个占着名额的、只为了收事件打开的 fd。放掉它，fd 关上（挂在上面的事件跟着摘掉），
/// 名额还回去。
struct Held(OwnedFd);

impl Held {
    /// 打开 `path`。名额用完了是 `Ok(None)`。
    fn open(path: &Path) -> io::Result<Option<Held>> {
        // 先占名额再开 fd。手写比较交换：`fetch_update` 在新版标准库里改了名
        let mut n = HELD.load(Ordering::Acquire);
        loop {
            if n >= budget() {
                return Ok(None);
            }
            match HELD.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => n = now,
            }
        }
        let release = || HELD.fetch_sub(1, Ordering::AcqRel);
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            release();
            return Err(io::ErrorKind::InvalidInput.into());
        };
        // O_NONBLOCK：万一打开的是个管道（条目刚被换成了别的东西），也不会卡在这里
        // SAFETY: c 是以 NUL 结尾的路径；返回的 fd 由下面的 OwnedFd 接管
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_EVTONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            release();
            return Err(e);
        }
        // SAFETY: fd 是刚打开的、只有这里拿着
        Ok(Some(Held(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// fstat：用的是它的类型、(dev, ino)、大小和修改时间
    fn stat(&self) -> io::Result<libc::stat> {
        // SAFETY: stat 是纯输出参数，全零是合法的初值
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd 活着；fstat 只往 st 里写
        if unsafe { libc::fstat(self.raw(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一个活着的 kqueue 监听；名额不够时是退回去的 FSEvents。
pub struct Watch {
    _backend: Backend,
}

/// 拿着它就是在盯；扔掉就停
enum Backend {
    Kqueue {
        _running: Running,
    },
    Notify {
        _fsevents: notify::RecommendedWatcher,
    },
}

/// 跑着的事件循环：kqueue 本身，和跑循环的线程
struct Running {
    kq: Arc<OwnedFd>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// 事件循环醒过几次（测试数它：深处的改动不该叫醒它）
    #[cfg(test)]
    wakeups: Arc<AtomicUsize>,
}

impl Drop for Running {
    /// **等线程收完再返回**：它手上的 fd 这时都已经关了、名额都还了，调用方紧接着再起一个
    /// 监听不会撞上名额
    fn drop(&mut self) {
        let _ = control(
            self.kq.as_raw_fd(),
            &[event(STOP, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER)],
        );
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Watch {
    /// 事件循环到现在醒过几次。退回 FSEvents 的是 `None`
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> Option<usize> {
        match &self._backend {
            Backend::Kqueue { _running: r } => Some(r.wakeups.load(Ordering::Relaxed)),
            Backend::Notify { .. } => None,
        }
    }
}

/// 盯住 `dirs`：见模块文档。每有一处算数的改动，往 `raw_tx` 里发一个。
///
/// **返回时就已经盯上了**：目录和文件都挂好、列过一遍，之后的改动一个都不会漏。
pub(crate) fn watch(
    dirs: &[PathBuf],
    relevant: Relevant,
    raw_tx: Sender<()>,
) -> Result<Watch, WatchError> {
    let start_error = |path: &Path, e: io::Error| WatchError::Start {
        path: path.to_path_buf(),
        source: notify::Error::io(e),
    };
    let first = dirs.first().map(PathBuf::as_path).unwrap_or(Path::new(""));
    // SAFETY: kqueue() 不碰内存；返回的 fd 由 OwnedFd 接管
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(start_error(first, io::Error::last_os_error()));
    }
    // SAFETY: kq 是刚建的、只有这里拿着
    let kq = Arc::new(unsafe { OwnedFd::from_raw_fd(kq) });
    // fork 出去的子进程不该拿着它
    // SAFETY: kq 活着；FD_CLOEXEC 只改这个 fd 自己的标志
    unsafe { libc::fcntl(kq.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    control(
        kq.as_raw_fd(),
        &[event(
            STOP,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
        )],
    )
    .map_err(|e| start_error(first, e))?;

    let mut l = Loop {
        kq: kq.clone(),
        dirs: dirs
            .iter()
            .map(|p| Dir {
                path: p.clone(),
                fd: None,
                parent: None,
                seen: HashMap::new(),
            })
            .collect(),
        files: HashMap::new(),
        by_fd: HashMap::new(),
        relevant: Arc::new(Mutex::new(relevant)),
        capped: false,
        #[cfg(test)]
        wakeups: Arc::new(AtomicUsize::new(0)),
    };
    let mut over = false;
    for (i, path) in dirs.iter().enumerate() {
        match l.open_dir(i) {
            Ok(true) => {}
            Ok(false) => {
                over = true;
                break;
            }
            Err(e) => return Err(start_error(path, e)),
        }
    }
    // 先挂好再列：列的时候已经有改动的话，事件也已经排在那儿了
    if !over {
        over = (0..l.dirs.len()).any(|i| l.rescan(i).is_err());
    }
    if over {
        // 一开始就不够：手上的放掉，整个用 FSEvents
        let relevant = l.handover();
        drop(l);
        return Ok(Watch {
            _backend: Backend::Notify {
                _fsevents: crate::by_notify(dirs, relevant, raw_tx)?,
            },
        });
    }

    #[cfg(test)]
    let wakeups = l.wakeups.clone();
    let thread = std::thread::Builder::new()
        .name("tw-watch kqueue".into())
        .spawn(move || l.run(&raw_tx))
        .map_err(|e| start_error(first, e))?;
    Ok(Watch {
        _backend: Backend::Kqueue {
            _running: Running {
                kq,
                thread: Some(thread),
                #[cfg(test)]
                wakeups,
            },
        },
    })
}

/// 一个盯着的目录
struct Dir {
    path: PathBuf,
    /// 没了、挪走了，那里还没有新的时是 None：这时盯着 `parent`
    fd: Option<Held>,
    /// 它没了的时候盯着的上一层：等这个名字再出现
    parent: Option<Held>,
    /// 上次列出来的、算数的条目：名字 → (inode, 是什么)
    seen: HashMap<OsString, (u64, Kind)>,
}

/// 一个条目是什么（readdir 给的，不另外 stat）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Symlink,
    Other,
}

/// 一个盯着的文件（目录里算数的普通文件，或者指向普通文件的符号链接）
struct File {
    fd: Held,
    /// 它在哪个目录里（[`Loop::dirs`] 的下标）
    dir: usize,
    /// 挂上时它是哪个文件（跟完符号链接）
    dev: u64,
    ino: u64,
    /// (大小, 修改时间的秒, 纳秒)：属性变了时拿来比，见 [`Loop::stamp_moved`]
    stamp: (i64, i64, i64),
}

/// 一个 fd 挂的是谁
enum Target {
    Dir(usize),
    /// 第几个目录没了之后盯着的上一层
    Parent(usize),
    File(PathBuf),
}

/// 名额用完了：整个换回 FSEvents
struct OverBudget;

/// 事件循环为什么停下
enum Exit {
    /// 监听被扔掉了，或者去抖那一头没了
    Stop,
    /// 名额用完了
    OverBudget,
}

struct Loop {
    kq: Arc<OwnedFd>,
    dirs: Vec<Dir>,
    files: HashMap<PathBuf, File>,
    by_fd: HashMap<RawFd, Target>,
    /// 退回 FSEvents 时要交给它，交不成还得自己接着用：所以是共用的
    relevant: Arc<Mutex<Relevant>>,
    /// 退不回 FSEvents（它起不来）：名额不够的就不盯，能盯多少盯多少
    capped: bool,
    #[cfg(test)]
    wakeups: Arc<AtomicUsize>,
}

impl Loop {
    fn run(mut self, raw_tx: &Sender<()>) {
        loop {
            if let Exit::Stop = self.serve(raw_tx) {
                return;
            }
            // **先起 FSEvents，再放手上的**：中间那一下不会有改动没人看。此刻没了的目录不盯
            // （FSEvents 盯不存在的路径会报错，整个起不来）
            let alive: Vec<PathBuf> = self
                .dirs
                .iter()
                .map(|d| d.path.clone())
                .filter(|p| p.is_dir())
                .collect();
            // 名额用完的那一批改动没算完：当作有过一次
            let _ = raw_tx.send(());
            match crate::by_notify(&alive, self.handover(), raw_tx.clone()) {
                Ok(fsevents) => {
                    self.files.clear();
                    self.by_fd.clear();
                    self.dirs.clear();
                    self.wait_for_stop();
                    drop(fsevents);
                    return;
                }
                // 起不来就接着用 kqueue，盯得住多少算多少
                Err(_) => self.capped = true,
            }
        }
    }

    /// 交给 FSEvents 的那一份 `relevant`：和这里用的是同一个
    fn handover(&self) -> Relevant {
        let r = self.relevant.clone();
        Box::new(move |p| (r.lock().unwrap_or_else(PoisonError::into_inner))(p))
    }

    /// `p` 算不算数
    fn relevant(&self, p: &Path) -> bool {
        (self.relevant.lock().unwrap_or_else(PoisonError::into_inner))(p)
    }

    /// 收事件，直到被叫停或者名额用完
    fn serve(&mut self, raw_tx: &Sender<()>) -> Exit {
        let mut buf = [event(0, 0, 0, 0); 64];
        loop {
            let n = match wait(self.kq.as_raw_fd(), &mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Exit::Stop,
            };
            #[cfg(test)]
            self.wakeups.fetch_add(1, Ordering::Relaxed);
            match self.handle(&buf[..n]) {
                Ok(false) => {}
                Ok(true) => {
                    if raw_tx.send(()).is_err() {
                        return Exit::Stop;
                    }
                }
                Err(exit) => return exit,
            }
        }
    }

    /// 处理一批事件，有算数的改动是 `Ok(true)`。
    ///
    /// **分两遍**：第一遍只记下要做什么，第二遍才关 fd、重新打开。关掉的 fd 号马上会被
    /// 新打开的复用，一边处理一边开关的话，同一批里排在后面的、关于旧文件的事件会被
    /// 算到新文件头上。
    fn handle(&mut self, events: &[libc::kevent]) -> Result<bool, Exit> {
        let mut changed = false;
        let mut rescan = BTreeSet::new();
        let mut gone_dirs = Vec::new();
        let mut gone_parents = Vec::new();
        let mut gone_files = Vec::new();
        for ev in events {
            if ev.filter == libc::EVFILT_USER {
                return Err(Exit::Stop);
            }
            let Ok(fd) = RawFd::try_from(ev.ident) else {
                continue;
            };
            match self.by_fd.get(&fd) {
                Some(Target::Dir(i)) => {
                    let i = *i;
                    if ev.fflags & GONE != 0 {
                        changed |= self.relevant(&self.dirs[i].path);
                        gone_dirs.push(i);
                    }
                    rescan.insert(i);
                }
                // 上一层的条目变了：等的那个名字可能出现了
                Some(Target::Parent(i)) => {
                    let i = *i;
                    if ev.fflags & GONE != 0 {
                        gone_parents.push(i);
                    }
                    rescan.insert(i);
                }
                Some(Target::File(path)) => {
                    let path = path.clone();
                    let Some(f) = self.files.get_mut(&path) else {
                        continue;
                    };
                    // 写过就算；大小和修改时间每次都记下新的，之后单报属性时才比得准
                    let wrote = ev.fflags & (libc::NOTE_WRITE | libc::NOTE_EXTEND) != 0;
                    let moved =
                        ev.fflags & (libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB) != 0
                            && Self::stamp_moved(f);
                    changed |= wrote || moved;
                    if ev.fflags & GONE != 0 {
                        changed = true;
                        rescan.insert(f.dir);
                        gone_files.push(path);
                    }
                }
                None => {}
            }
        }
        for path in gone_files {
            self.drop_file(&path);
        }
        for i in gone_dirs {
            self.drop_dir(i);
        }
        for i in gone_parents {
            self.drop_parent(i);
        }
        for i in rescan {
            changed |= self.rescan(i).map_err(|OverBudget| Exit::OverBudget)?;
        }
        Ok(changed)
    }

    /// 属性变了的文件，大小或修改时间是不是也变了（变了就记下新的）。截断只报属性；
    /// 改权限、改属主也报属性，但它们不算
    fn stamp_moved(f: &mut File) -> bool {
        let Ok(st) = f.fd.stat() else {
            return false;
        };
        let now = stamp(&st);
        let moved = now != f.stamp;
        f.stamp = now;
        moved
    }

    /// 打开第 `i` 个目录、挂上事件。名额用完了是 `Ok(false)`。
    fn open_dir(&mut self, i: usize) -> io::Result<bool> {
        let Some(fd) = Held::open(&self.dirs[i].path)? else {
            return Ok(false);
        };
        if fd.stat()?.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        arm(self.kq.as_raw_fd(), fd.raw(), DIR_EVENTS)?;
        self.by_fd.insert(fd.raw(), Target::Dir(i));
        self.dirs[i].fd = Some(fd);
        Ok(true)
    }

    /// 第 `i` 个目录自己没了、被挪走：手上那个 fd 不再是那个路径上的东西，放掉
    fn drop_dir(&mut self, i: usize) {
        if let Some(fd) = self.dirs[i].fd.take() {
            self.by_fd.remove(&fd.raw());
        }
    }

    /// 第 `i` 个目录的上一层也没了、被挪走：不再等
    fn drop_parent(&mut self, i: usize) {
        if let Some(fd) = self.dirs[i].parent.take() {
            self.by_fd.remove(&fd.raw());
        }
    }

    /// 没了的第 `i` 个目录：按原来的路径再打开。**先盯上一层，再试着打开** —— 反过来的话，
    /// 两步之间换上来的那个就没人看见了。打开了就不用再盯上一层；打不开就等上一层报信
    fn revive(&mut self, i: usize) -> Result<(), OverBudget> {
        let over = |capped: bool| if capped { Ok(()) } else { Err(OverBudget) };
        if self.dirs[i].parent.is_none()
            && let Some(up) = self.dirs[i].path.parent().map(Path::to_path_buf)
            && !up.as_os_str().is_empty()
        {
            match Held::open(&up) {
                Ok(Some(fd)) => {
                    if arm(self.kq.as_raw_fd(), fd.raw(), DIR_EVENTS).is_ok() {
                        self.by_fd.insert(fd.raw(), Target::Parent(i));
                        self.dirs[i].parent = Some(fd);
                    }
                }
                Ok(None) => over(self.capped)?,
                // 上一层也没了：不再盯
                Err(_) => {}
            }
        }
        match self.open_dir(i) {
            Ok(true) => self.drop_parent(i),
            Ok(false) => over(self.capped)?,
            Err(_) => {}
        }
        Ok(())
    }

    /// 重新列第 `i` 个目录：算数的条目多了、少了、换了，是 `Ok(true)`。顺带把要盯的文件
    /// 对上：新的挂上，换掉的重新挂，不在了的放掉。
    ///
    /// 目录自己没了的，先按原来的路径再打开（[`Self::revive`]）：又有了就接着盯它，它的
    /// 出现也算一次；没有的话，算数的条目一个都不剩了。
    fn rescan(&mut self, i: usize) -> Result<bool, OverBudget> {
        let mut changed = false;
        if self.dirs[i].fd.is_none() {
            self.revive(i)?;
            if self.dirs[i].fd.is_some() {
                changed |= self.relevant(&self.dirs[i].path);
            }
        }
        let now = match &self.dirs[i].fd {
            Some(_) => self.list(&self.dirs[i].path),
            None => HashMap::new(),
        };
        changed |= now != self.dirs[i].seen;
        self.dirs[i].seen = now;
        self.sync_files(i)?;
        Ok(changed)
    }

    /// 目录里算数的条目
    fn list(&self, dir: &Path) -> HashMap<OsString, (u64, Kind)> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return HashMap::new();
        };
        rd.flatten()
            .filter(|e| self.relevant(&dir.join(e.file_name())))
            .map(|e| {
                // readdir 给的类型；给不出来时 std 自己补一次 lstat
                let kind = match e.file_type() {
                    Ok(t) if t.is_file() => Kind::File,
                    Ok(t) if t.is_symlink() => Kind::Symlink,
                    _ => Kind::Other,
                };
                (e.file_name(), (e.ino(), kind))
            })
            .collect()
    }

    /// 把第 `i` 个目录里要盯的文件对上它此刻的样子（[`Dir::seen`]）
    fn sync_files(&mut self, i: usize) -> Result<(), OverBudget> {
        let dir = &self.dirs[i];
        let want: Vec<(PathBuf, u64, Kind)> = dir
            .seen
            .iter()
            .filter(|(_, (_, kind))| *kind != Kind::Other)
            .map(|(name, (ino, kind))| (dir.path.join(name), *ino, *kind))
            .collect();
        let stale: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(p, f)| f.dir == i && !want.iter().any(|(w, ..)| w == *p))
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            self.drop_file(&p);
        }
        for (path, ino, kind) in want {
            if let Some(f) = self.files.get(&path) {
                // 还是挂着的那一个就不动。符号链接按它指向的那个文件比
                let same = match kind {
                    Kind::Symlink => {
                        std::fs::metadata(&path).is_ok_and(|m| (m.dev(), m.ino()) == (f.dev, f.ino))
                    }
                    _ => ino == f.ino,
                };
                if same {
                    continue;
                }
                self.drop_file(&path);
            }
            // 符号链接指向的不是普通文件（目录、设备、不存在）就不盯
            if kind == Kind::Symlink && !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
            self.add_file(i, path)?;
        }
        Ok(())
    }

    /// 挂上一个文件。打不开（刚好又没了、没有权限）就算了：它的增删改名目录那边照样看得见
    fn add_file(&mut self, dir: usize, path: PathBuf) -> Result<(), OverBudget> {
        let fd = match Held::open(&path) {
            Ok(Some(fd)) => fd,
            Ok(None) if self.capped => return Ok(()),
            Ok(None) => return Err(OverBudget),
            Err(_) => return Ok(()),
        };
        let Ok(st) = fd.stat() else { return Ok(()) };
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Ok(());
        }
        if arm(self.kq.as_raw_fd(), fd.raw(), FILE_EVENTS).is_err() {
            return Ok(());
        }
        self.by_fd.insert(fd.raw(), Target::File(path.clone()));
        self.files.insert(
            path,
            File {
                dir,
                dev: st.st_dev as u64,
                ino: st.st_ino,
                stamp: stamp(&st),
                fd,
            },
        );
        Ok(())
    }

    fn drop_file(&mut self, path: &Path) {
        if let Some(f) = self.files.remove(path) {
            self.by_fd.remove(&f.fd.raw());
        }
    }

    /// 只等叫停的那个事件
    fn wait_for_stop(&self) {
        let mut buf = [event(0, 0, 0, 0); 4];
        loop {
            match wait(self.kq.as_raw_fd(), &mut buf) {
                Ok(n) if buf[..n].iter().any(|e| e.filter == libc::EVFILT_USER) => return,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

/// (大小, 修改时间的秒, 纳秒)
fn stamp(st: &libc::stat) -> (i64, i64, i64) {
    (st.st_size, st.st_mtime, st.st_mtime_nsec)
}

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

/// 在 `fd` 上挂 vnode 事件。`EV_CLEAR`：报过一次就清掉，不会同一件事一直报
fn arm(kq: RawFd, fd: RawFd, fflags: u32) -> io::Result<()> {
    let ident = usize::try_from(fd).map_err(|_| io::Error::from_raw_os_error(libc::EBADF))?;
    control(
        kq,
        &[event(
            ident,
            libc::EVFILT_VNODE,
            libc::EV_ADD | libc::EV_CLEAR,
            fflags,
        )],
    )
}

/// 改 kqueue 里挂着的事件，不收
fn control(kq: RawFd, changes: &[libc::kevent]) -> io::Result<()> {
    let n = libc::c_int::try_from(changes.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: changes 活着、长度如实；不收事件，输出缓冲区是空的
    let r = unsafe {
        libc::kevent(
            kq,
            changes.as_ptr(),
            n,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 等到有事件为止（不设超时），收进 `buf`，交回收到几个
fn wait(kq: RawFd, buf: &mut [libc::kevent]) -> io::Result<usize> {
    let cap = libc::c_int::try_from(buf.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: buf 活着、可写、长度如实；不改挂着的事件
    let n = unsafe {
        libc::kevent(
            kq,
            std::ptr::null(),
            0,
            buf.as_mut_ptr(),
            cap,
            std::ptr::null(),
        )
    };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use crate::tests::{Signal, WINDOW, append, only_md, recv, save_by_rename, scratch, settle};

    /// 事件循环醒过几次
    fn wakeups(w: &crate::Watch) -> usize {
        w._inner.wakeups().expect("退回了 FSEvents")
    }

    /// **这一条就是用 kqueue 的原因。**子目录里再怎么写，事件循环一次都不醒 —— FSEvents
    /// 在这里每一次写都会叫醒我们，再在进程里丢掉。而盯着的目录自己的条目变了，照样醒
    #[tokio::test]
    async fn changes_deeper_down_do_not_even_wake_it() {
        let d = scratch();
        let deep = d.path().join("history/2026/10");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(d.path().join("a.md"), "0\n").unwrap();
        let (w, mut rx) = crate::watch(&[d.path().to_path_buf()], WINDOW, only_md).unwrap();
        for i in 0..200 {
            std::fs::write(deep.join(format!("{i}.md")), "x\n").unwrap();
        }
        std::fs::create_dir_all(deep.join("more/and/more")).unwrap();
        std::fs::remove_dir_all(d.path().join("history/2026")).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet
        );
        assert_eq!(wakeups(&w), 0, "深处的改动叫醒了事件循环");

        // 这一层的条目变了才醒；不算数的条目醒了也不发信号
        std::fs::write(d.path().join("x.log"), "x\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet
        );
        assert!(wakeups(&w) > 0, "这一层的改动没叫醒它");
    }

    /// 属性变了只有大小、修改时间跟着变才算：改权限不算，`touch` 算
    #[tokio::test]
    async fn only_size_and_modification_time_count_among_attributes() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = scratch();
        let p = d.path().join("a.md");
        std::fs::write(&p, "0\n").unwrap();
        let (_w, mut rx) = crate::watch(&[d.path().to_path_buf()], WINDOW, only_md).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet,
            "改权限"
        );
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "touch"
        );
    }

    /// 符号链接指过去的文件也盯：原地写算数；它被换掉之后，重新挂到换上来的那一个
    /// （`~/.claude.json` 指进 dotfiles 仓库的那种布局）
    #[tokio::test]
    async fn a_symlinked_file_is_followed_to_where_it_points() {
        let d = scratch();
        let (watched, elsewhere) = (d.path().join("home"), d.path().join("dotfiles"));
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let real = elsewhere.join("a.md");
        std::fs::write(&real, "0\n").unwrap();
        std::os::unix::fs::symlink(&real, watched.join("a.md")).unwrap();
        let (_w, mut rx) = crate::watch(std::slice::from_ref(&watched), WINDOW, only_md).unwrap();

        append(&real, "1\n");
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "追加"
        );
        settle(&mut rx).await;
        save_by_rename(&real, "2\n");
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "换文件"
        );
        settle(&mut rx).await;
        append(&real, "3\n");
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "换过之后的追加"
        );
    }

    /// 盯着的目录被整个换掉（rename 一个新目录过来）、删了过一阵又建出来：目录自己算数的话
    /// 各算一次，之后接着盯新的那个。盯的是路径，和 FSEvents 一样
    #[tokio::test]
    async fn a_watched_directory_that_is_replaced_or_recreated_is_followed() {
        let d = scratch();
        let dir = d.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();
        let me = dir.clone();
        let (_w, mut rx) = crate::watch(std::slice::from_ref(&dir), WINDOW, move |p: &Path| {
            p == me || only_md(p)
        })
        .unwrap();

        let next = d.path().join("skills.new");
        std::fs::create_dir_all(&next).unwrap();
        std::fs::rename(&dir, d.path().join("skills.old")).unwrap();
        std::fs::rename(&next, &dir).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "目录被挪走"
        );
        settle(&mut rx).await;
        std::fs::write(dir.join("a.md"), "0\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "换上来的目录没盯上"
        );
        settle(&mut rx).await;
        // 挪走的那个不再盯
        std::fs::write(d.path().join("skills.old/b.md"), "0\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_millis(600)).await,
            Signal::Quiet
        );

        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "删除"
        );
        settle(&mut rx).await;
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "再建"
        );
        settle(&mut rx).await;
        std::fs::write(dir.join("c.md"), "0\n").unwrap();
        assert_eq!(
            recv(&mut rx, Duration::from_secs(3)).await,
            Signal::Got,
            "再建出来的目录没盯上"
        );
    }
}
