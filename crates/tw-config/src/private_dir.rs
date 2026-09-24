//! 建数据目录，**只给自己看**。
//!
//! # 两个平台各管什么
//!
//! 配置里是明文 API key（这份配置不做 keychain，见 `credential`）。unix 上
//! 护着它的首先是文件自己的 `0600`（建的时候就带着，见 `store::write_atomic`）；
//! 目录新建时给 `0700`，别人连文件名（请求正文的日期目录、历史版本）也看不到。
//! **已经存在的目录不去动** —— 用户自己放宽过的，是他的决定。
//!
//! Windows 上没有 mode 位：文件的权限是从**目录**继承来的。所以那里目录的
//! ACL 就是那份密钥的全部保护。
//!
//! # 为什么是「建的时候设好」，不是「建完再查」
//!
//! 查出来只能拒绝启动，问题还在原地，用户得自己去改 ACL —— 而要查得准，
//! 就得走 DACL、逐条比对 Everyone / Users / Authenticated Users 三个知名
//! SID、再解析访问掩码，比这里多一倍还不止。建的时候设好把问题解决掉。
//!
//! 代价：**对已经存在的目录不生效**。Windows 版还没发过，所以眼下没有存量；
//! 真要补，那是另加一次启动时的检查。

use std::path::Path;

/// 建出数据目录。已经在了就什么都不做。
///
/// 默认位置（`%APPDATA%` 在用户配置文件下）继承来的 ACL 本来就够；这里
/// 管的是用户把 `THINKWATCH_HOME` 指到 `D:\ThinkWatch` 那种地方的情形 ——
/// 那里的默认 ACL 通常给 `Users` 读权限。
pub fn create(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    // 上级按常规建。**密钥在叶子目录里**，而叶子目录上一个受保护的 DACL
    // 已经挡住了读它内容的人 —— 上级宽松只是让人知道有这么个目录。
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    imp::create_private(dir)
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    /// unix：建成 `0700`。`mode` 是在 `mkdir(2)` 那一刻给的，没有先宽后收的窗口。
    pub fn create_private(dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            // 查过不在、建的时候却在了：别的进程抢先一步，和 `create_dir_all` 一样不算错
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
            r => r,
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn create_private(dir: &Path) -> std::io::Result<()> {
        let sid = current_user_sid()?;
        // `D:P` —— 一份**受保护的** DACL：不继承父目录那些条目，否则
        // `D:\` 上那条给 `Users` 的就跟着进来了，而那正是要挡的东西。
        // `OICI` 让里面新建的文件和子目录跟着继承这两条。
        // `FA` 完全控制：当前用户，以及 SYSTEM（`SY`，不给它的话备份、
        // 索引这类系统服务会在这个目录上报错）。
        let sddl = format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)");
        with_security_attributes(&sddl, |sa| {
            let path = wide(dir);
            // SAFETY: 路径是以 NUL 结尾的 UTF-16，`sa` 在这次调用期间有效。
            let ok = unsafe { CreateDirectoryW(path.as_ptr(), sa) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }

    fn wide(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// 当前用户的 SID，写成 `S-1-5-21-…` 那种字符串。
    ///
    /// **问令牌要，不写死任何东西。**SDDL 里有 `CO`（Creator Owner）这类
    /// 简写，但它们是给可继承条目当占位符用的，放在对象自己的 DACL 上
    /// 不代表「建它的那个人」。
    fn current_user_sid() -> std::io::Result<String> {
        struct Token(HANDLE);
        impl Drop for Token {
            fn drop(&mut self) {
                // SAFETY: 拿到过的句柄，只关一次
                unsafe { CloseHandle(self.0) };
            }
        }

        let mut raw: HANDLE = std::ptr::null_mut();
        // SAFETY: 出参是本地变量
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = Token(raw);

        // 先问要多大，再按那个大小要一次
        let mut len: u32 = 0;
        // SAFETY: 传空缓冲区只为问尺寸；失败是预期的（ERROR_INSUFFICIENT_BUFFER）
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // `TOKEN_USER` 后面跟着 SID 的字节，所以缓冲区要按它对齐 ——
        // `Vec<u8>` 只保证 1 字节对齐，而我们要把它当成那个结构体读。
        let words = (len as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words.max(1)];
        // SAFETY: 缓冲区至少 `len` 字节，且对齐得下 TOKEN_USER
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                len,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: 调用成功，缓冲区开头是一个 TOKEN_USER
        let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
        let mut s: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` 指向刚拿到的那份数据，出参是本地变量
        if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: 转换成功，`s` 是一段以 NUL 结尾的 UTF-16，由 LocalFree 释放
        let out = unsafe {
            let mut n = 0usize;
            while *s.add(n) != 0 {
                n += 1;
            }
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
            LocalFree(s as *mut std::ffi::c_void);
            out
        };
        Ok(out)
    }

    /// 把一段 SDDL 变成 `SECURITY_ATTRIBUTES`，用完释放。
    fn with_security_attributes<T>(
        sddl: &str,
        f: impl FnOnce(*const SECURITY_ATTRIBUTES) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `text` 是以 NUL 结尾的 UTF-16；出参是本地变量
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let r = f(&sa);
        // SAFETY: `sd` 由上面那次转换分配，只释放一次
        unsafe { LocalFree(sd) };
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_creates_the_directory() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("data");
        create(&dir).unwrap();
        assert!(dir.is_dir());
    }

    /// 已经在了就不动它 —— 再建一次不该报错，也不该把里面的东西弄没。
    #[test]
    fn creating_one_that_is_already_there_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("data");
        create(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "version: 1").unwrap();
        create(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
            "version: 1"
        );
    }

    /// 上级不在也要能建出来。`THINKWATCH_HOME` 指到一个还不存在的深路径
    /// 是完全正常的用法。
    #[test]
    fn missing_parents_are_created_too() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("a/b/c");
        create(&dir).unwrap();
        assert!(dir.is_dir());
    }

    /// unix 上新建的数据目录只给自己：别人连里面的文件名也列不出来。
    #[cfg(unix)]
    #[test]
    fn a_new_directory_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("a/data");
        create(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "数据目录的权限是 {mode:o}");
    }
}
