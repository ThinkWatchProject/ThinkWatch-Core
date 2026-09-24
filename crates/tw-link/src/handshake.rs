//! 两条握手消息。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tw_api::CONTROL_API_VERSION;
use tw_api::control::ControlKey;

use crate::{
    ClientHello, HANDSHAKE_TIMEOUT, LinkError, MAX_HANDSHAKE_LEN, PROLOGUE, REJECT, SecureStream,
    ServerHello, invalid, params,
};

/// 以应用的身份握手。`app` 是应用自己的版本，只给 core 的日志看。
///
/// 成功时拿到加密的流和 core 的回答（它的版本号在 `ServerHello.core` 里）。
pub async fn connect<S>(
    stream: S,
    key: &ControlKey,
    app: &str,
) -> Result<(SecureStream<S>, ServerHello), LinkError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    connect_with_timeout(stream, key, app, HANDSHAKE_TIMEOUT).await
}

/// 同 [`connect`]，限时自己定。测试用得上，远程的慢链路也许用得上。
pub async fn connect_with_timeout<S>(
    stream: S,
    key: &ControlKey,
    app: &str,
    limit: Duration,
) -> Result<(SecureStream<S>, ServerHello), LinkError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(limit, client(stream, key, app, CONTROL_API_VERSION)).await {
        Ok(r) => r,
        Err(_) => Err(LinkError::Timeout),
    }
}

/// `proto` 总是 [`CONTROL_API_VERSION`]，只有测试会说别的，好看看版本不一致时
/// 两边各说什么。
pub(crate) async fn client<S>(
    mut stream: S,
    key: &ControlKey,
    app: &str,
    proto: u32,
) -> Result<(SecureStream<S>, ServerHello), LinkError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hs = snow::Builder::new(params())
        .prologue(PROLOGUE)
        .and_then(|b| b.psk(0, key.as_bytes()))
        .and_then(|b| b.build_initiator())
        .map_err(noise)?;

    let hello = serde_json::to_vec(&ClientHello {
        proto,
        app: app.to_string(),
    })
    .map_err(|e| LinkError::Io(invalid(e.to_string())))?;
    let mut buf = vec![0u8; MAX_HANDSHAKE_LEN];
    let n = hs.write_message(&hello, &mut buf).map_err(noise)?;
    write_frame(&mut stream, &buf[..n]).await?;

    // 第一个字节决定是哪一种回答：REJECT，还是一条消息的长度头
    let first = match read_byte(&mut stream).await? {
        None => return Err(LinkError::Closed),
        Some(REJECT) => return Err(LinkError::WrongKey),
        Some(b) => b,
    };
    let second = read_byte(&mut stream).await?.ok_or(LinkError::Closed)?;
    let len = u16::from_be_bytes([first, second]) as usize;
    if len > MAX_HANDSHAKE_LEN {
        return Err(LinkError::Io(invalid(format!(
            "the second handshake message says it is {len} bytes long"
        ))));
    }
    let mut msg = vec![0u8; len];
    stream
        .read_exact(&mut msg)
        .await
        .map_err(LinkError::from_io)?;
    // **这里解不开不是钥匙不对**：钥匙不对时 core 在第一条就解不开，回的是
    // REJECT。走到这一步还解不开，是数据在路上坏了，或者对面根本不是 core
    let mut payload = vec![0u8; MAX_HANDSHAKE_LEN];
    let n = hs
        .read_message(&msg, &mut payload)
        .map_err(|e| LinkError::Io(invalid(format!("the core's answer is unreadable: {e}"))))?;
    let answer: ServerHello = serde_json::from_slice(&payload[..n])
        .map_err(|e| LinkError::Io(invalid(format!("the core's answer is malformed: {e}"))))?;
    if !answer.accept || answer.proto != proto {
        return Err(LinkError::VersionMismatch {
            ours: proto,
            theirs: answer.proto,
            peer_version: answer.core,
        });
    }
    let transport = hs.into_transport_mode().map_err(noise)?;
    Ok((SecureStream::new(stream, transport), answer))
}

/// 钥匙从哪儿来。**每次握手问一次**：配置换了钥匙，下一条连接就用新的。
///
/// 返回 `None` 是「此刻没有钥匙」—— 那时谁来都一样被拒。
pub type KeySource = Arc<dyn Fn() -> Option<ControlKey> + Send + Sync>;

/// 服务端这一侧：用哪把钥匙、自己报什么版本。
///
/// 克隆很便宜，每条连接一份。
#[derive(Clone)]
pub struct Acceptor {
    key: KeySource,
    core: Arc<str>,
    limit: Duration,
}

/// 握手成功的一条连接。
pub struct Accepted<S> {
    pub stream: SecureStream<S>,
    /// 应用说了什么（版本号、应用版本）
    pub hello: ClientHello,
    /// 这条连接是用哪把钥匙握上的。钥匙换了之后，用旧钥匙进来的连接
    /// 该断开 —— 调用方拿它和当前的比
    pub key: ControlKey,
}

impl std::fmt::Debug for Acceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acceptor")
            .field("core", &self.core)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl Acceptor {
    /// `core` 是 core 自己的版本，写进第二条消息给应用看。
    pub fn new(
        key: impl Fn() -> Option<ControlKey> + Send + Sync + 'static,
        core: impl Into<String>,
    ) -> Self {
        Self {
            key: Arc::new(key),
            core: Arc::from(core.into()),
            limit: HANDSHAKE_TIMEOUT,
        }
    }

    /// 换一个限时。默认 [`HANDSHAKE_TIMEOUT`]。
    pub fn with_timeout(mut self, limit: Duration) -> Self {
        self.limit = limit;
        self
    }

    /// 在一条刚接进来的连接上握手。
    ///
    /// 失败时该回的都已经回过了（REJECT 或者 `accept: false`），调用方只管
    /// 记一行日志、丢掉这条连接。
    pub async fn accept<S>(&self, stream: S) -> Result<Accepted<S>, LinkError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match tokio::time::timeout(self.limit, self.server(stream)).await {
            Ok(r) => r,
            Err(_) => Err(LinkError::Timeout),
        }
    }

    async fn server<S>(&self, mut stream: S) -> Result<Accepted<S>, LinkError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let first = match read_byte(&mut stream).await? {
            None => return Err(LinkError::Closed),
            Some(b) => b,
        };
        let second = read_byte(&mut stream).await?.ok_or(LinkError::Closed)?;
        let len = u16::from_be_bytes([first, second]) as usize;
        if len > MAX_HANDSHAKE_LEN {
            // 不是我们的客户端（比如有人拿 curl 敲过来，`GE` 两个字节读成了
            // 一个很大的长度）。**照钥匙不对回**：对面分不清这两种，也不该分清
            reject(&mut stream).await;
            return Err(LinkError::WrongKey);
        }
        let mut msg = vec![0u8; len];
        stream
            .read_exact(&mut msg)
            .await
            .map_err(LinkError::from_io)?;

        // 钥匙在读完第一条之后才取：这条连接用的是此刻生效的那一把
        let Some(key) = (self.key)() else {
            reject(&mut stream).await;
            return Err(LinkError::WrongKey);
        };
        let mut hs = snow::Builder::new(params())
            .prologue(crate::PROLOGUE)
            .and_then(|b| b.psk(0, key.as_bytes()))
            .and_then(|b| b.build_responder())
            .map_err(noise)?;
        let mut payload = vec![0u8; MAX_HANDSHAKE_LEN];
        let n = match hs.read_message(&msg, &mut payload) {
            Ok(n) => n,
            Err(_) => {
                reject(&mut stream).await;
                return Err(LinkError::WrongKey);
            }
        };
        // 解得开却读不成：钥匙对，说的却不是这个协议。回一个 `accept: false`，
        // 它至少能看到我们的版本号
        let hello: Option<ClientHello> = serde_json::from_slice(&payload[..n]).ok();
        let accept = hello
            .as_ref()
            .is_some_and(|h| h.proto == CONTROL_API_VERSION);
        let answer = serde_json::to_vec(&crate::ServerHello {
            proto: CONTROL_API_VERSION,
            core: self.core.to_string(),
            accept,
        })
        .map_err(|e| LinkError::Io(invalid(e.to_string())))?;
        let mut buf = vec![0u8; MAX_HANDSHAKE_LEN];
        let n = hs.write_message(&answer, &mut buf).map_err(noise)?;
        write_frame(&mut stream, &buf[..n]).await?;
        let Some(hello) = hello else {
            let _ = stream.shutdown().await;
            return Err(LinkError::Io(invalid(
                "the first handshake message decrypted but is not a hello",
            )));
        };
        if !accept {
            let _ = stream.shutdown().await;
            return Err(LinkError::VersionMismatch {
                ours: CONTROL_API_VERSION,
                theirs: hello.proto,
                peer_version: hello.app,
            });
        }
        let transport = hs.into_transport_mode().map_err(noise)?;
        Ok(Accepted {
            stream: SecureStream::new(stream, transport),
            hello,
            key,
        })
    }
}

/// 回那个明文字节，然后关掉写的一侧。**失败不管**：对面可能早就走了。
async fn reject<S: AsyncWrite + Unpin>(stream: &mut S) {
    let _ = stream.write_all(&[REJECT]).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, msg: &[u8]) -> Result<(), LinkError> {
    let len = u16::try_from(msg.len()).expect("a handshake message is at most 1024 bytes");
    let mut out = Vec::with_capacity(2 + msg.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(msg);
    stream.write_all(&out).await.map_err(LinkError::from_io)?;
    stream.flush().await.map_err(LinkError::from_io)
}

/// 读一个字节；对面在这之前就关了是 `None`。
async fn read_byte<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<u8>, LinkError> {
    let mut b = [0u8; 1];
    match stream.read(&mut b).await {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(b[0])),
        Err(e) => match LinkError::from_io(e) {
            LinkError::Closed => Ok(None),
            other => Err(other),
        },
    }
}

/// snow 自己的错误。**正常使用碰不到**：参数都是常量，缓冲区都够大。
fn noise(e: snow::Error) -> LinkError {
    LinkError::Io(std::io::Error::other(format!("noise: {e}")))
}
