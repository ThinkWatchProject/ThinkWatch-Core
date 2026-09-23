//! 控制面的凭据。
//!
//! # 为什么 socket 之外还要一道门
//!
//! unix socket 那一套（`0700` 的文件，只有属主连得上）在 macOS 上够用，但它
//! **是文件系统给的保证，不是我们给的**。Windows 上没有对等物：控制面在那里
//! 只能落在 `127.0.0.1` 的 TCP 上，而本机任意进程都连得上一个回环端口，连上
//! 之后也问不出对端是谁 —— unix socket 问得出（`SO_PEERCRED`），TCP 问不出。
//!
//! 所以那里必须有一道自己的门。**两个平台装的是同一道**，不是只给 Windows
//! 装：一道只在一个平台上生效的防线，等于一道没人日常测的防线。
//!
//! # token 从哪儿来
//!
//! 优先环境变量。桌面端自己 spawn core（它从不接管已经在跑的），所以由它生成、
//! 通过环境变量交过来。**不进 argv** —— Windows 的任务管理器和任意同用户进程
//! 都看得见命令行 —— 也不落盘。
//!
//! 没有环境变量，就是有人手工跑 `twcore serve`。那时自己生成一个，写进
//! `<配置目录>/control.token`，下次再跑读回来。**不是每次现生成**：那样拿
//! curl 调试的人每跑一次都得重抄一遍。

use std::path::{Path, PathBuf};

use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tw_types::msg;

// 名字住在契约层：桌面端要知道环境变量叫什么才能把 token 交过来，而它
// 依赖的是 tw-api，够不着这个 crate。两边各写一遍字符串就是两边会漂。
use tw_api::control::{TOKEN_ENV as ENV, token_file};

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("the control-plane token file {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the control-plane token file {path} could not be written: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone)]
pub struct Token(String);

/// **不打印出来。**`Token` 会跟着别的结构体一起落进 `tracing` 的 Debug 输出，
/// 而日志是会被整段贴进 issue 的。
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// 环境变量优先，没有就读或建那个文件。
    pub fn resolve(dir: &Path) -> Result<Self, TokenError> {
        match Self::from_env() {
            Some(t) => Ok(t),
            None => Self::load_or_create(dir),
        }
    }

    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(ENV).ok()?;
        let v = raw.trim();
        // 空字符串当作没给。**一个空 token 不是一道松的门，是一道开着的门** ——
        // 下面的比较会对每一个同样没带凭据的请求成立。
        if v.is_empty() {
            None
        } else {
            Some(Self(v.to_string()))
        }
    }

    fn load_or_create(dir: &Path) -> Result<Self, TokenError> {
        let path = token_file(dir);
        match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => return Ok(Self(s.trim().to_string())),
            // 空文件：当没有过，重新生成盖掉。理由同 `from_env` 里那条。
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(TokenError::Read { path, source }),
        }
        let token = Self::generate();
        write_private(&path, token.as_str())?;
        Ok(token)
    }

    /// 32 字节的随机数，写成十六进制。
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
        Self(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 对方给的那串对不对。
    ///
    /// **恒定时间**：不在第一个不同的字节上提前返回。本机上的时序攻击不好做，
    /// 但这里的差别只是一个 `|=` 和一个 `break`。
    pub fn matches(&self, presented: &str) -> bool {
        let (want, got) = (self.0.as_bytes(), presented.as_bytes());
        // 长度不等直接否。**长度不是秘密** —— 自己生成的永远是 64 个十六进制
        // 字符 —— 所以为了不泄漏它去把比较写复杂没有意义。
        if want.len() != got.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in want.iter().zip(got) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// 建的时候就是 `0600`。
///
/// **不是建完再 `chmod`** —— 那中间有一个全世界可读的窗口，而窗口里躺着的
/// 正是这道门的钥匙。已经存在的那个文件先删掉再建，否则 `mode` 对它不生效。
fn write_private(path: &Path, contents: &str) -> Result<(), TokenError> {
    use std::io::Write;
    let err = |source| TokenError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(err)?
        .write_all(contents.as_bytes())
        .map_err(err)
}

/// 把鉴权装到一份路由表上。
///
/// **装在 serve 那一层，不装进 `router()`。**`router()` 是路由表本身，十几个
/// 集成测试直接拿它跑处理函数；把门焊进去，等于让每个测试都先学会开门 ——
/// 而它们测的不是门。门装在真正对外的那一步上，门自己在下面单独测。
pub fn guard(app: Router, token: Token) -> Router {
    app.layer(axum::middleware::from_fn_with_state(Arc::new(token), check))
}

async fn check(State(token): State<Arc<Token>>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(got) if token.matches(got) => next.run(req).await,
        _ => unauthorized(),
    }
}

/// `Authorization: Bearer <token>`。
///
/// 方案名按 RFC 7235 大小写不敏感 —— 我们自己的客户端永远写 `Bearer`，但拿
/// curl 手敲的人写 `bearer` 不该被当成凭据不对。
fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(msg!(
            "control.unauthorized" =>
            "The control plane needs the token it was started with. The desktop app passes it automatically; \
             a hand-written client reads it from control.token in the configuration directory."
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(token: Token) -> Router {
        guard(Router::new().route("/x", get(|| async { "ok" })), token)
    }

    async fn status_with(auth: Option<&str>) -> StatusCode {
        let token = Token("sesame".to_string());
        let mut req = Request::builder().uri("/x");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        app(token)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_right_token_gets_in_and_nothing_else_does() {
        assert_eq!(status_with(Some("Bearer sesame")).await, StatusCode::OK);
        // 方案名大小写不敏感
        assert_eq!(status_with(Some("bearer sesame")).await, StatusCode::OK);

        assert_eq!(
            status_with(None).await,
            StatusCode::UNAUTHORIZED,
            "没带凭据"
        );
        assert_eq!(
            status_with(Some("Bearer open")).await,
            StatusCode::UNAUTHORIZED,
            "凭据不对"
        );
        assert_eq!(
            status_with(Some("sesame")).await,
            StatusCode::UNAUTHORIZED,
            "少了方案名"
        );
        assert_eq!(
            status_with(Some("Basic sesame")).await,
            StatusCode::UNAUTHORIZED,
            "换了方案名"
        );
        assert_eq!(
            status_with(Some("Bearer ")).await,
            StatusCode::UNAUTHORIZED,
            "空凭据"
        );
    }

    /// 前缀对上不算对上 —— 恒定时间那段比较里长度是先判的，别把它判反了。
    #[test]
    fn a_prefix_is_not_a_match() {
        let t = Token("sesame".to_string());
        assert!(t.matches("sesame"));
        assert!(!t.matches("sesam"));
        assert!(!t.matches("sesamee"));
        assert!(!t.matches(""));
    }

    #[test]
    fn a_generated_token_is_64_hex_characters_and_never_the_same_twice() {
        let a = Token::generate();
        let b = Token::generate();
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
    }

    /// 第二次启动要拿到同一个 —— 否则每跑一次，手工客户端都得重抄一遍。
    #[test]
    fn the_file_is_written_once_and_read_back_after_that() {
        let d = tempfile::tempdir().unwrap();
        let first = Token::load_or_create(d.path()).unwrap();
        let again = Token::load_or_create(d.path()).unwrap();
        assert_eq!(first.as_str(), again.as_str());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_file(d.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "token 文件不该有别人的份");
        }
    }

    /// 空文件当作没有。**留着它等于门开着**
    #[test]
    fn an_empty_file_is_replaced_rather_than_trusted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(token_file(d.path()), "   \n").unwrap();
        let t = Token::load_or_create(d.path()).unwrap();
        assert_eq!(t.as_str().len(), 64);
    }

    /// 写进文件时带了换行，读回来不该把换行也当成凭据的一部分。
    #[test]
    fn surrounding_whitespace_in_the_file_is_not_part_of_the_token() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(token_file(d.path()), "  abc123\n").unwrap();
        assert!(Token::load_or_create(d.path()).unwrap().matches("abc123"));
    }

    /// 日志里不能出现它。
    #[test]
    fn it_does_not_print_itself() {
        let t = Token("sesame".to_string());
        assert!(!format!("{t:?}").contains("sesame"));
    }
}
