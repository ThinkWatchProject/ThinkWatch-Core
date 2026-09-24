//! `twcore upgrade`：把这个二进制换成 GitHub Release 上的另一版。
//!
//! **只给自己管自己的那种安装用** —— 服务器上 `scripts/install.sh` 装的、手工
//! 放进 PATH 的。桌面应用包里的那一份由应用自己更新：换掉它，应用和 core 就
//! 不是同一个 commit 出来的了（控制面协议是两边一起编的），所以在包里一律拒绝。
//!
//! 一次升级的顺序，每一步失败都停在原地、什么都没动：
//!
//! 1. 问 Release（最新的，或 `--version` 指定的那一版），和自己的版本比；
//! 2. 能不能写可执行文件所在的目录 —— **下载之前就问**，免得下了二十兆才说要 sudo；
//! 3. 下载本平台的二进制和它的 `.sha256`，核对；
//! 4. 写进同目录的临时文件，跑一次 `--version`，报的得是要装的那一版；
//! 5. 改名盖过去。unix 上 rename 是原子的：任何时刻那个路径上要么是旧的、要么
//!    是新的，正在跑的进程拿着旧的 inode 照常跑。
//!
//! 配置和数据一概不碰。换完之后正在跑的服务还是旧版，重启它由人决定（或 `--restart`）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "ThinkWatchProject/ThinkWatch-Core";
const API: &str = "https://api.github.com";
/// install.sh 装的那个 unit
const UNIT: &str = "twcore";

pub struct Opts {
    pub check: bool,
    pub restart: bool,
    pub version: Option<String>,
}

/// 三段数字的版本号。**不认预发布后缀** —— 这个项目不发预发布版，认了反而
/// 要回答「0.47.0-rc1 比 0.47.0 新吗」这种用不上的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl std::str::FromStr for Version {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let t = s.trim();
        let t = t.strip_prefix('v').unwrap_or(t);
        let parts: Vec<&str> = t.split('.').collect();
        let n = |p: &str| p.parse::<u64>().ok();
        match parts.as_slice() {
            [a, b, c] => match (n(a), n(b), n(c)) {
                (Some(a), Some(b), Some(c)) => Ok(Version(a, b, c)),
                _ => bail!("`{s}` is not a version such as 0.47.0"),
            },
            _ => bail!("`{s}` is not a version such as 0.47.0"),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// 这个平台在 Release 里对应哪个构建。**和 release.yml 发的那几个一一对应**，
/// 没发的平台（Intel Mac）返回 `None`。
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    })
}

/// Release 里的文件名：裸二进制，Windows 带 `.exe`。
pub fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("twcore-{target}.exe")
    } else {
        format!("twcore-{target}")
    }
}

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    pub fn version(&self) -> Result<Version> {
        self.tag_name.parse()
    }

    /// 二进制和它的校验文件的下载地址。**两个缺一个都不装**：没有校验和的
    /// 二进制，装上去的是什么没人说得清。
    pub fn pick(&self, asset: &str) -> Result<(&str, &str)> {
        let url = |name: &str| {
            self.assets
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.browser_download_url.as_str())
        };
        let sha = format!("{asset}.sha256");
        match (url(asset), url(&sha)) {
            (Some(b), Some(s)) => Ok((b, s)),
            (None, _) => bail!(
                "release {} has no {asset}, so there is no build for this machine in it",
                self.tag_name
            ),
            (Some(_), None) => bail!(
                "release {} has {asset} but not {sha}, so it cannot be verified",
                self.tag_name
            ),
        }
    }
}

/// `<sha256>  <文件名>` 里的那串十六进制，和下载到的字节比。
pub fn verify(bytes: &[u8], sha_file: &str) -> Result<()> {
    let want = sha_file
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow!("the checksum file does not start with a SHA-256"))?;
    let got: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != want {
        bail!("the download's SHA-256 is {got}, and the release says {want}; nothing was changed");
    }
    Ok(())
}

/// 这个可执行文件是不是桌面应用包里的那一份。
///
/// 三个平台的包各长各的样，认的是路径上的形状：macOS 的 `.app`；Linux 的
/// AppImage 挂载点（`/tmp/.mount_…`）和它里面的 `ThinkWatch Lite` 目录；Windows 的
/// 安装目录 `ThinkWatch Lite`。
pub fn in_app_bundle(exe: &Path) -> bool {
    exe.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.ends_with(".app") || s == "ThinkWatch Lite" || s.starts_with(".mount_")
    })
}

/// 在目标旁边写一个临时文件，**同一个目录** —— rename 只在同一个文件系统上是原子的。
///
/// 扩展名留在最后（`.twcore.upgrade-123.exe`）：Windows 按扩展名认可执行文件，
/// 下面那次 `--version` 要能跑起来。
fn staging_path(exe: &Path) -> PathBuf {
    let stem = exe
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "twcore".into());
    let ext = exe
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    exe.with_file_name(format!(".{stem}.upgrade-{}{ext}", std::process::id()))
}

/// 能不能在这个目录里写。**下载之前问**，答案是「要 sudo」时不必先下二十兆。
pub fn check_writable(exe: &Path) -> Result<()> {
    let probe = staging_path(exe);
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "{} cannot be written by this user. Run the upgrade with sudo",
            exe.parent().unwrap_or(exe).display()
        ),
        Err(e) => Err(e).with_context(|| format!("writing next to {}", exe.display())),
    }
}

/// 写新文件、`check` 一下、盖过去。`check` 拿到的是临时文件的路径。
///
/// **任何一步失败，原来那个文件原样还在。**
pub fn replace(exe: &Path, bytes: &[u8], check: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = staging_path(exe);
    let r = (|| {
        write_executable(&tmp, bytes)?;
        check(&tmp)?;
        swap(&tmp, exe)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)?;
    // 落盘再改名：先改名后断电，路径上就是一个半截文件
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(unix)]
fn swap(new: &Path, exe: &Path) -> Result<()> {
    std::fs::rename(new, exe).with_context(|| format!("replacing {}", exe.display()))
}

/// Windows 不让覆盖一个正在跑的 exe，但让它改名。所以先把旧的挪开、再把新的
/// 挪进来；第二步失败就挪回去。挪开的那个下次升级时删掉（那时它已经不在跑了）。
#[cfg(windows)]
fn swap(new: &Path, exe: &Path) -> Result<()> {
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e).with_context(|| format!("replacing {}", exe.display()));
    }
    Ok(())
}

/// 跑一次新文件的 `--version`，报的得是要装的那一版。
///
/// 编得过但起不来的二进制（架构不对、glibc 太旧）在这里就露出来，而不是在
/// 服务重启之后。
fn runs_as(path: &Path, want: Version) -> Result<()> {
    let out = run_fresh(std::process::Command::new(path).arg("--version")).with_context(|| {
        format!(
            "the downloaded twcore does not run on this machine ({})",
            path.display()
        )
    })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let got = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<Version>().ok());
    if !out.status.success() || got != Some(want) {
        bail!(
            "the downloaded twcore reports `{}` rather than {want}; nothing was changed",
            text.trim()
        );
    }
    Ok(())
}

/// 跑一个刚写完的文件。
///
/// Linux 上，别的线程正好在 fork 的那一瞬间会连带拿着这个文件的写句柄，
/// exec 就报 `ETXTBSY`（文件正被写）—— 句柄随 exec 关掉，稍等再试就好。
fn run_fresh(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    let mut tries = 0;
    loop {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

fn client() -> Result<reqwest::Client> {
    builder().build().context("creating the HTTP client")
}

/// 走环境变量里的代理（`HTTPS_PROXY`）：服务器上出网常常只有这一条路。
fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("twcore/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        // Release 的下载地址会跳到 GitHub 的存储域名，得跟；但不跟到 http 上去
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.url().scheme() == "http"
                && a.previous().last().is_some_and(|u| u.scheme() == "https")
            {
                a.error("refused to follow a redirect from https to http")
            } else if a.previous().len() >= 10 {
                a.error("too many redirects")
            } else {
                a.follow()
            }
        }))
}

async fn get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    let r = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !r.status().is_success() {
        bail!("{url} answered {}", r.status());
    }
    Ok(r)
}

/// 最新的一版，或者指定的那一版。
pub async fn release(
    client: &reqwest::Client,
    api: &str,
    want: Option<Version>,
) -> Result<Release> {
    let url = match want {
        Some(v) => format!("{api}/repos/{REPO}/releases/tags/v{v}"),
        None => format!("{api}/repos/{REPO}/releases/latest"),
    };
    let r = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("asking GitHub for the release ({url})"))?;
    match r.status() {
        s if s.is_success() => Ok(r.json().await.context("reading the release")?),
        reqwest::StatusCode::NOT_FOUND => match want {
            Some(v) => bail!("there is no release v{v}"),
            None => bail!("there is no release yet"),
        },
        s => bail!("GitHub answered {s} for {url}"),
    }
}

/// 做什么，由版本比较决定。
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// 已经是它了
    Same,
    /// 没指定版本，而这一版比最新的还新（自己编的、或者还没发出去的）
    Ahead,
    Install,
}

pub fn plan(current: Version, target: Version, pinned: bool) -> Plan {
    match current.cmp(&target) {
        std::cmp::Ordering::Equal => Plan::Same,
        // 指定了版本就照装，哪怕是降级 —— 服务器要和桌面应用对上版本，那一版
        // 可能正好比服务器上的旧
        std::cmp::Ordering::Greater if !pinned => Plan::Ahead,
        _ => Plan::Install,
    }
}

/// 装好之后换了什么。给调用方决定要不要重启。
#[derive(Debug)]
pub struct Installed {
    pub to: Version,
}

/// 一次升级，除了「说给人听」和「重启服务」之外的全部。拆出来是为了测试能
/// 指一个假的 API 和一个假的可执行文件。
pub async fn upgrade(
    client: &reqwest::Client,
    api: &str,
    exe: &Path,
    current: Version,
    target: &str,
    opts: &Opts,
    run_check: bool,
) -> Result<Option<Installed>> {
    let pinned = opts
        .version
        .as_deref()
        .map(str::parse::<Version>)
        .transpose()?;
    let rel = release(client, api, pinned).await?;
    let to = rel.version()?;
    match plan(current, to, pinned.is_some()) {
        Plan::Same => {
            println!(
                "twcore {current} is installed, and it is {}",
                if pinned.is_some() {
                    "the version asked for"
                } else {
                    "the latest release"
                }
            );
            return Ok(None);
        }
        Plan::Ahead => {
            println!("twcore {current} is newer than the latest release, {to}; nothing to do");
            return Ok(None);
        }
        Plan::Install => {}
    }
    let asset = asset_name(target);
    let (bin_url, sha_url) = rel.pick(&asset)?;
    if opts.check {
        let how = match pinned {
            Some(v) => format!("twcore upgrade --version {v}"),
            None => "twcore upgrade".to_string(),
        };
        println!("twcore {current} is installed; {to} is available. To install it: {how}");
        return Ok(None);
    }
    check_writable(exe)?;
    println!("downloading {asset} {to}");
    let sha = get(client, sha_url).await?.text().await?;
    let bytes = get(client, bin_url).await?.bytes().await?;
    verify(&bytes, &sha)?;
    replace(
        exe,
        &bytes,
        |p| if run_check { runs_as(p, to) } else { Ok(()) },
    )?;
    println!("replaced {}: {current} → {to}", exe.display());
    Ok(Some(Installed { to }))
}

/// systemd 在不在管这个服务。
fn service_active() -> Option<bool> {
    let st = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", UNIT])
        .status()
        .ok()?;
    Some(st.success())
}

pub fn run(opts: Opts) -> Result<()> {
    let exe = std::env::current_exe().context("finding this executable")?;
    // 符号链接要落到真正的文件上：改名盖的是链接指向的那个
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    if in_app_bundle(&exe) {
        bail!(
            "{} is the copy inside the ThinkWatch Lite app, and the app updates it itself. \
             `twcore upgrade` is for a twcore installed on its own, such as on a server",
            exe.display()
        );
    }
    let target = target_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        anyhow!(
            "no twcore is published for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let current: Version = env!("CARGO_PKG_VERSION").parse()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let done = rt.block_on(async {
        let c = client()?;
        upgrade(&c, API, &exe, current, target, &opts, true).await
    })?;
    let Some(done) = done else {
        return Ok(());
    };
    // 重启是人的决定：换二进制不打断在途请求，重启会
    match (
        cfg!(target_os = "linux").then(service_active).flatten(),
        opts.restart,
    ) {
        (Some(true), true) => {
            let st = std::process::Command::new("systemctl")
                .args(["restart", UNIT])
                .status()
                .context("running systemctl restart")?;
            if !st.success() {
                bail!(
                    "systemctl restart {UNIT} failed; the new binary is in place, restart it by hand"
                );
            }
            println!("restarted {UNIT}.service; it now runs {}", done.to);
        }
        (Some(true), false) => {
            println!("{UNIT}.service is still running the previous version. To switch:");
            println!("  sudo systemctl restart {UNIT}");
        }
        (_, true) => println!(
            "no running {UNIT}.service was found to restart; restart twcore wherever it runs"
        ),
        (_, false) => {
            println!("a twcore that is running keeps the previous version until it is restarted")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn versions_compare_as_numbers_not_text() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("v0.47.0"), v("0.47.0"));
        for bad in ["0.47", "0.47.0.1", "x.1.2", "0.47.0-rc1", ""] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_pinned_version_installs_even_when_it_is_older() {
        // 服务器要和桌面应用对上版本，那一版可能比服务器上的旧
        assert_eq!(plan(v("0.48.0"), v("0.47.2"), true), Plan::Install);
        assert_eq!(plan(v("0.48.0"), v("0.47.2"), false), Plan::Ahead);
        assert_eq!(plan(v("0.47.2"), v("0.48.0"), false), Plan::Install);
        assert_eq!(plan(v("0.48.0"), v("0.48.0"), true), Plan::Same);
    }

    #[test]
    fn every_published_platform_has_an_asset_and_others_have_none() {
        assert_eq!(
            target_for("linux", "x86_64").map(asset_name).as_deref(),
            Some("twcore-x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("linux", "aarch64").map(asset_name).as_deref(),
            Some("twcore-aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("macos", "aarch64").map(asset_name).as_deref(),
            Some("twcore-aarch64-apple-darwin")
        );
        assert_eq!(
            target_for("windows", "x86_64").map(asset_name).as_deref(),
            Some("twcore-x86_64-pc-windows-msvc.exe")
        );
        // Intel Mac 没有构建：说没有，而不是装一个 arm64 的上去
        assert_eq!(target_for("macos", "x86_64"), None);
        // 这台跑测试的机器本身得在表里
        assert!(target_for(std::env::consts::OS, std::env::consts::ARCH).is_some());
    }

    /// release.yml 发的文件名和这里拼的要一致 —— 两边各写一遍，对不上时
    /// `twcore upgrade` 找不到文件
    #[test]
    fn the_release_workflow_publishes_what_upgrade_looks_for() {
        let wf = include_str!("../../../.github/workflows/release.yml");
        for (os, arch) in [
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let t = target_for(os, arch).unwrap();
            let name = asset_name(t);
            let pattern = name.replace(t, "${{ matrix.target }}");
            assert!(
                wf.contains(&format!("dist/{name}.sha256"))
                    || wf.contains(&format!("dist/{pattern}.sha256")),
                "release.yml does not publish {name}.sha256"
            );
        }
    }

    fn release_with(names: &[&str]) -> Release {
        Release {
            tag_name: "v0.48.0".into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.com/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn picking_needs_the_binary_and_its_checksum() {
        let a = "twcore-x86_64-unknown-linux-gnu";
        let r = release_with(&[
            a,
            "twcore-x86_64-unknown-linux-gnu.sha256",
            "twcore-x86_64-unknown-linux-gnu.tar.gz",
        ]);
        let (b, s) = r.pick(a).unwrap();
        assert!(b.ends_with(a));
        assert!(s.ends_with(".sha256"));
        let e = release_with(&[a]).pick(a).unwrap_err().to_string();
        assert!(e.contains("cannot be verified"), "{e}");
        let e = release_with(&[]).pick(a).unwrap_err().to_string();
        assert!(e.contains("no build"), "{e}");
    }

    #[test]
    fn the_checksum_has_to_match() {
        let body = b"twcore";
        let good: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        verify(body, &format!("{good}  twcore-x86_64-unknown-linux-gnu\n")).unwrap();
        // 大写的十六进制也是同一个值
        verify(body, &good.to_uppercase()).unwrap();
        assert!(verify(b"twcorE", &good).is_err());
        assert!(verify(body, "not a checksum").is_err());
        assert!(verify(body, "").is_err());
    }

    #[test]
    fn the_copy_inside_the_desktop_app_is_not_upgraded() {
        for p in [
            "/Applications/ThinkWatch Lite.app/Contents/Resources/twcore",
            "/tmp/.mount_ThinkWxyz/usr/lib/ThinkWatch Lite/twcore",
            "/opt/ThinkWatch Lite/usr/lib/ThinkWatch Lite/twcore",
            r"C:\Users\a\AppData\Local\ThinkWatch Lite\twcore.exe",
        ] {
            let p = PathBuf::from(p.replace('\\', std::path::MAIN_SEPARATOR_STR));
            assert!(in_app_bundle(&p), "{}", p.display());
        }
        for p in ["/usr/local/bin/twcore", "/home/a/.local/bin/twcore"] {
            assert!(!in_app_bundle(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn replacing_is_all_or_nothing() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        std::fs::write(&exe, b"old").unwrap();

        // 检查不过：旧的原样在，临时文件不留
        let e = replace(&exe, b"new", |_| bail!("does not run")).unwrap_err();
        assert!(e.to_string().contains("does not run"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);

        // 检查拿到的是写好的新文件，不是旧的
        replace(&exe, b"new", |p| {
            assert_ne!(p, exe.as_path());
            assert_eq!(std::fs::read(p).unwrap(), b"new");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            // 只剩它自己，没有临时文件
            assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
        }
    }

    /// 一个进程在跑着旧文件的时候换掉它：它照常跑完，路径上已经是新的。
    #[cfg(unix)]
    #[test]
    fn a_running_binary_can_be_replaced_under_it() {
        use std::io::BufRead;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        // 脚本由 sh 打开之后才算「在跑」：等它先说一句，再去换
        write_executable(&exe, b"#!/bin/sh\necho started\nsleep 1\necho old\n").unwrap();
        let spawn = || {
            std::process::Command::new(&exe)
                .stdout(std::process::Stdio::piped())
                .spawn()
        };
        let mut child = (0..10)
            .find_map(|_| match spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                }
                r => Some(r.unwrap()),
            })
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "started");
        replace(&exe, b"#!/bin/sh\necho new\n", |_| Ok(())).unwrap();
        assert_eq!(lines.next().unwrap().unwrap(), "old");
        child.wait().unwrap();
        let now = run_fresh(&mut std::process::Command::new(&exe)).unwrap();
        assert_eq!(String::from_utf8_lossy(&now.stdout).trim(), "new");
    }

    // ── 对着一个假的 GitHub 跑一整遍 ─────────────────────────

    struct Fake {
        base: String,
        _task: tokio::task::JoinHandle<()>,
    }

    /// 一个只认三条路径的 GitHub：最新 Release、按 tag 查的 Release、下载。
    async fn fake_github(tag: &'static str, binary: Vec<u8>, sha: String) -> Fake {
        use axum::{Router, extract::State, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asset = asset_name(target_for(std::env::consts::OS, std::env::consts::ARCH).unwrap());
        let release = serde_json::json!({
            "tag_name": tag,
            "assets": [
                { "name": asset, "browser_download_url": format!("{base}/dl/bin") },
                { "name": format!("{asset}.sha256"), "browser_download_url": format!("{base}/dl/sha") },
            ],
        });
        #[derive(Clone)]
        struct S {
            release: serde_json::Value,
            tag: &'static str,
            binary: Vec<u8>,
            sha: String,
        }
        let app = Router::new()
            .route(
                "/repos/ThinkWatchProject/ThinkWatch-Core/releases/latest",
                get(|State(s): State<S>| async move { axum::Json(s.release) }),
            )
            .route(
                "/repos/ThinkWatchProject/ThinkWatch-Core/releases/tags/{tag}",
                get(
                    |State(s): State<S>, axum::extract::Path(t): axum::extract::Path<String>| async move {
                        if t == s.tag {
                            Ok(axum::Json(s.release))
                        } else {
                            Err(axum::http::StatusCode::NOT_FOUND)
                        }
                    },
                ),
            )
            .route("/dl/bin", get(|State(s): State<S>| async move { s.binary }))
            .route("/dl/sha", get(|State(s): State<S>| async move { s.sha }))
            .with_state(S {
                release,
                tag,
                binary,
                sha,
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fake { base, _task: task }
    }

    fn sha_of(b: &[u8]) -> String {
        let h: String = Sha256::digest(b)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{h}  twcore\n")
    }

    fn opts(check: bool, version: Option<&str>) -> Opts {
        Opts {
            check,
            restart: false,
            version: version.map(str::to_string),
        }
    }

    fn target() -> &'static str {
        target_for(std::env::consts::OS, std::env::consts::ARCH).unwrap()
    }

    /// 假服务器在回环上，不能让开发机上的代理变量把请求带走
    fn client() -> reqwest::Client {
        builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn an_upgrade_downloads_verifies_and_replaces() {
        // 新的「二进制」是一段脚本，`--version` 报 0.48.0 —— 于是「跑一次看它报
        // 什么」这一步也是真跑的
        let new = b"#!/bin/sh\necho twcore 0.48.0\n".to_vec();
        let fake = fake_github("v0.48.0", new.clone(), sha_of(&new)).await;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        std::fs::write(&exe, b"old").unwrap();
        let c = client();

        // 只看不动
        let r = upgrade(
            &c,
            &fake.base,
            &exe,
            v("0.47.0"),
            target(),
            &opts(true, None),
            cfg!(unix),
        )
        .await
        .unwrap();
        assert!(r.is_none());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");

        let r = upgrade(
            &c,
            &fake.base,
            &exe,
            v("0.47.0"),
            target(),
            &opts(false, None),
            cfg!(unix),
        )
        .await
        .unwrap()
        .expect("it installs");
        assert_eq!(r.to, v("0.48.0"));
        assert_eq!(std::fs::read(&exe).unwrap(), new);

        // 已经是最新的：什么都不做
        let r = upgrade(
            &c,
            &fake.base,
            &exe,
            v("0.48.0"),
            target(),
            &opts(false, None),
            cfg!(unix),
        )
        .await
        .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn a_download_that_does_not_match_its_checksum_changes_nothing() {
        let fake = fake_github("v0.48.0", b"tampered".to_vec(), sha_of(b"original")).await;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        std::fs::write(&exe, b"old").unwrap();
        let e = upgrade(
            &client(),
            &fake.base,
            &exe,
            v("0.47.0"),
            target(),
            &opts(false, None),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    /// 下载下来的东西报的不是要装的那一版（发错了文件）：不装。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_binary_that_reports_another_version_is_not_installed() {
        let wrong = b"#!/bin/sh\necho twcore 0.46.0\n".to_vec();
        let fake = fake_github("v0.48.0", wrong.clone(), sha_of(&wrong)).await;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        std::fs::write(&exe, b"old").unwrap();
        let e = upgrade(
            &client(),
            &fake.base,
            &exe,
            v("0.47.0"),
            target(),
            &opts(false, None),
            true,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("0.46.0"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[tokio::test]
    async fn a_pinned_version_that_does_not_exist_says_so() {
        let fake = fake_github("v0.48.0", Vec::new(), String::new()).await;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("twcore");
        let e = upgrade(
            &client(),
            &fake.base,
            &exe,
            v("0.47.0"),
            target(),
            &opts(false, Some("0.9.9")),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("no release v0.9.9"), "{e}");
    }
}
