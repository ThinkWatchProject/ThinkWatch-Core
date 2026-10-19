//! 多设备同步那一半的契约：同步位置、状态、开启与加入、冲突、加密、停止。
//!
//! 同步怎么合并、同步位置上放什么在 core 的 `tw-sync` 里；这里只有线上的样子。冲突、随之
//! 不同步的项目按项目（[`ItemRef`]）说，和导入、恢复是同一套。

use serde::{Deserialize, Serialize};

use crate::{ImportAction, ImportDecision, ImportRow, ItemRef, Msg};

fn direct() -> String {
    "direct".to_string()
}

/// 同步位置。`proxy` 是 `direct`、`system`（跟随系统）或者出站代理的名字：WebDAV、S3 的请求经
/// 它发出，文件夹用不到。
///
/// **口令、密码、密钥原样来回**（界面不打码配置值）：设置表单回填的就是它们。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SyncTarget {
    /// 本机（远程 core 时是服务器上）的一个文件夹：iCloud Drive、Dropbox、OneDrive、Syncthing、
    /// NAS 共享目录
    Folder {
        path: String,
        #[serde(default = "direct")]
        proxy: String,
    },
    /// 坚果云、Nextcloud、群晖 NAS……。`http://` 只给本机和内网
    Webdav {
        url: String,
        #[serde(default)]
        user: String,
        #[serde(default)]
        pass: String,
        #[serde(default = "direct")]
        proxy: String,
    },
    /// Amazon S3、Cloudflare R2、阿里云 OSS、腾讯云 COS、MinIO。寻址方式（桶名进域名还是进
    /// 路径）由端点决定，不用选
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        #[serde(default)]
        prefix: String,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default = "direct")]
        proxy: String,
    },
}

slug_enum! {
    /// 同步此刻怎么样。
    pub enum SyncState {
        /// 没开
        Off = "off",
        /// 已同步
        Idle = "idle",
        /// 正在同步
        Syncing = "syncing",
        /// 有冲突等着处理（其余照常同步）
        Conflict = "conflict",
        /// 连续 2 小时连不上同步位置。恢复后自动继续，本机的修改那时上传
        Unreachable = "unreachable",
        /// 同步位置拒绝访问：改连接参数
        AuthFailed = "auth_failed",
        /// 同步口令已在其他设备上更改（或者同步位置开了加密）：输入口令
        Locked = "locked",
        /// 其他设备用的是较新的版本，本机读不了：暂停，更新之后继续
        TooNew = "too_new",
        /// 安全模式：不自动同步
        Paused = "paused",
        /// 同步位置上的 keyring 不见了、被换成了更早的、一直读不出来。**不会自己关掉加密**：
        /// 由人选「重新上传本机配置」（`POST /sync/republish`：放回这台设备记着的 keyring）或者
        /// 停止同步。原因在 `error`
        KeyringMissing = "keyring_missing",
        /// 同步位置空了（连着几轮、过了一段时间都是空的）：其他设备删除了同步数据，或者它已经
        /// 不是原来那个位置。同步设置不动，由人选「重新上传本机配置」（`POST /sync/republish`）
        /// 或者停止同步
        Emptied = "emptied",
        /// 同步设置用不了（比如出站代理被删了、不加密的 `http://` 地址设了代理）：改设置。
        /// 原因在 `error`
        Misconfigured = "misconfigured",
    }
}

/// 这台设备
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncDeviceRef {
    pub id: String,
    pub name: String,
}

/// 参与同步的一台设备
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncDevice {
    pub id: String,
    pub name: String,
    pub core_version: String,
    /// 最后一次同步成功的时间
    pub last_seen_ms: u64,
    /// 是这台设备
    pub this: bool,
    /// 「已暂停：需更新」：同步位置上的最新版本它读不了
    pub paused_update: bool,
    /// 它的 core 开着远程控制端口（界面标「远程 core」）
    pub remote_core: bool,
}

/// `GET /sync`，也是 [`crate::Event::SyncStatus`] 带的那一份。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncView {
    pub state: SyncState,
    /// 没开是空的
    pub target: Option<SyncTarget>,
    /// 同步位置上的内容加没加密
    pub encrypted: bool,
    pub device: Option<SyncDeviceRef>,
    pub last_synced_ms: Option<u64>,
    /// 从这一刻起一直没同步成。成功一次就清掉
    pub failing_since_ms: Option<u64>,
    /// 要人处理的状态（拒绝访问、口令、版本过新、连不上）的原因
    pub error: Option<Msg>,
    pub devices: Vec<SyncDevice>,
    /// 还没处理的冲突有几处
    pub conflicts: u32,
    /// 不同步的上游（名字）
    pub excluded: Vec<String>,
    /// 随之不同步的项目：引用了不同步的上游的路由、别名、分组
    pub excluded_cascade: Vec<ItemRef>,
    /// 同步位置上跳过了的文件：坏了的、不是这把密钥加密的、加密的位置上没加密的、等了
    /// 30 分钟还没传完的。不是 0 时界面提一句
    pub skipped: u32,
    /// 同步成了，但有没照办的地方（上一轮的）：界面提一句，**不是失败**，不计入连不上。现在
    /// 只有 `sync.prefs_too_large`：同步数据里的通用偏好超出上限，这台留着自己的，其余照常
    pub notices: Vec<Msg>,
}

/// `POST /sync/test`、`POST /sync/enable/preview`：连上去看看。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncTestRequest {
    pub target: SyncTarget,
    /// 已经有加密的同步数据时，给口令才看得到设备列表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

/// 同步位置上已经有的同步数据
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncExisting {
    pub encrypted: bool,
    /// 加密的：口令对了。不加密的总是 `true`
    pub unlocked: bool,
    /// 已经在同步的设备。加密而口令不对时是空的
    pub devices: Vec<SyncDevice>,
}

/// `POST /sync/test` 的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncProbe {
    /// 读写都行
    pub writable: bool,
    /// 不行的原因
    pub error: Option<Msg>,
    /// 空的同步位置是 `null`（开启同步走「新建」）；已经有同步数据走「加入」
    pub existing: Option<SyncExisting>,
}

/// `POST /sync/enable/preview` 的结果：加入时本机配置和同步数据怎么合，规则和导入一样。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncJoinPreview {
    /// 本机配置还是初始配置（新安装后没改过）：不出现合并这一步，直接用同步数据
    pub initial: bool,
    /// 同步数据里的每个项目和本机比（[`ImportRow`]）。`initial` 时是空的
    pub rows: Vec<ImportRow>,
    /// core 替人定了、不用选的那几项（两种加入方式都照它办，`decisions` 里给了也不看）。它们
    /// 在 `rows` 里照常列出，界面那一行显示成定下的处理和原因，不当作冲突
    pub auto: Vec<SyncJoinAuto>,
}

/// 加入时不用选的一项（[`SyncJoinPreview::auto`]）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncJoinAuto {
    pub item: ItemRef,
    /// 怎么处理。现在只有 `overwrite`：采用同步数据
    pub action: ImportAction,
    pub reason: SyncJoinAutoReason,
}

slug_enum! {
    /// 为什么不用选。
    pub enum SyncJoinAutoReason {
        /// 两边的默认网关密钥（`default_key` 指着的那一把）同名、值不同：每个安装都会自动生成
        /// 一把，加入的设备采用同步数据的值。本机已接管的客户端随之自动更新
        /// （`GatewayKeysChanged`）
        DefaultKey = "default_key",
    }
}

slug_enum! {
    /// 加入时怎么处理本机已有的配置。
    pub enum SyncJoin {
        /// 按导入的规则合并，冲突项逐项选择（`decisions`）
        Merge = "merge",
        /// 用同步数据替换本机配置。本机已接管的客户端正在用的网关密钥并入同步数据
        UseRemote = "use_remote",
    }
}

/// `POST /sync/enable`：开启同步。同步位置空着就新建，已经有同步数据就加入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncEnable {
    pub target: SyncTarget,
    /// 新建时加密（缺省）还是不加密。加入时按同步位置上的，这一项不看
    pub encrypt: bool,
    /// 新建加密的同步时至少 12 个字符；加入加密的同步时是那个口令
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    pub device_name: String,
    pub join: SyncJoin,
    /// `merge` 时逐项的处理（[`ImportDecision`]），没列的新项目照常导入
    #[serde(default)]
    pub decisions: Vec<ImportDecision>,
}

/// `PUT /sync/settings`：改连接参数（密码、密钥、代理）、设备名称、不同步的上游。只改给了的。
///
/// **换成另一个同步位置（另一个文件夹、地址、存储桶、前缀）不在这里**：那是先停止同步、再
/// 开启同步（`sync.place_changed`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncSettingsSave {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SyncTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded: Option<Vec<String>>,
}

/// 冲突的一边
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncConflictSide {
    /// 设备名称
    pub device: String,
    pub at_ms: u64,
    /// 这一边删了这个对象
    pub deleted: bool,
}

slug_enum! {
    /// 冲突不是因为两边改得不一样，而是：
    pub enum SyncConflictReason {
        /// 另一边删了一把网关密钥，而本机已接管的客户端正在用它：没删
        KeyInUse = "key_in_use",
        /// 合并的结果过不了配置校验（比如路由引用的上游被另一边删了）
        Invalid = "invalid",
    }
}

/// 冲突里的一个合并单位，两边的值（规范化之后的 JSON）。一边删了对象时那一边是 `null`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncConflictDiff {
    pub unit: String,
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub local: Option<serde_json::Value>,
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub remote: Option<serde_json::Value>,
}

/// `GET /sync/conflicts` 的一项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncConflict {
    pub id: String,
    pub item: ItemRef,
    /// 不同的合并单位：上游是 `conn`、`models`、`billing`、`other`，插件是 `script`、`settings`、
    /// `enabled`，全局设置是字段的路径，别的是 `all`
    pub units: Vec<String>,
    /// 本机这一边（冲突处理之前本机一直用它）
    pub local: SyncConflictSide,
    /// 另一边
    pub remote: SyncConflictSide,
    /// 两边不同的地方，逐个单位：点对象名时显示的差异
    pub diff: Vec<SyncConflictDiff>,
    /// 能不能「两者都保留」：两边都还有这个对象，而且它有名字（和 `options` 里有没有 `both`
    /// 是一回事）
    pub both_allowed: bool,
    pub reason: Option<SyncConflictReason>,
    /// 这处冲突能选哪几种处理，界面只给这几个按钮：`local` 总在；`remote` 在合并结果过不了
    /// 校验（`invalid`）时没有；`both` 同 `both_allowed`。网关密钥的「两者都保留」新增的那一把
    /// 是一个新的值
    pub options: Vec<SyncTake>,
}

slug_enum! {
    /// 处理一处冲突：采用哪一边。
    pub enum SyncTake {
        Local = "local",
        Remote = "remote",
        /// 两者都保留：本机的留原名称，另一边的以「名称 (设备名)」新增
        Both = "both",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncConflictDecision {
    pub id: String,
    pub take: SyncTake,
    /// 两者都保留时另一份的名字。不给就是「名称 (设备名)」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

/// `POST /sync/conflicts`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncResolve {
    pub decisions: Vec<SyncConflictDecision>,
}

slug_enum! {
    pub enum SyncEncryptionAction {
        /// 设口令、开启加密：重新上传最新版本，删掉未加密的版本
        Enable = "enable",
        /// 关闭加密：要当前的口令
        Disable = "disable",
        /// 更改口令：只重新加密 keyring
        Change = "change",
    }
}

/// `POST /sync/encryption`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncEncryption {
    pub action: SyncEncryptionAction,
    /// 开启时是新口令（至少 12 个字符），关闭、更改时是当前的口令
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// 更改时的新口令（至少 12 个字符）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_passphrase: Option<String>,
}

/// `POST /sync/unlock`：其他设备改了口令（或者开了加密）之后，在这台设备上输入
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncUnlock {
    pub passphrase: String,
}

/// `POST /sync/disable`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncDisable {
    /// 连同步位置上的全部版本、设备记录和 keyring 一起删掉。其他设备随后提示同步位置为空
    #[serde(default)]
    pub purge: bool,
}

/// 不加密的同步改了一个上游的地址（[`crate::ProviderView::sync_url_changed`]）：最近一次的
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncUrlChange {
    /// 改之前的地址
    pub from: String,
    /// 改的那台设备的名称
    pub device: Option<String>,
    pub at_ms: u64,
}
