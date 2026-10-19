//! 配置包那一半的契约：导出、导入、本机备份与恢复、Lite 的通用偏好、固定密钥。
//!
//! 配置包（`.twconfig`）的格式和「项目」的划分在 core 的 `tw-pack` 里；这里只有线上的样子。
//! 一个项目是配置里有名字的一样东西（[`ItemRef`]），导入和恢复都按项目列、按项目选。

use serde::{Deserialize, Serialize};

use crate::{ConfigOrigin, Msg};

slug_enum! {
    /// 配置里的一样东西是哪一种。
    pub enum ItemKind {
        /// 上游
        Provider = "provider",
        /// 出站代理
        Proxy = "proxy",
        /// 网关密钥
        Key = "key",
        /// 模型别名
        Alias = "alias",
        /// 策略组
        Group = "group",
        Route = "route",
        /// 价目表
        PriceSheet = "price_sheet",
        /// 自定义安全规则。名字是 `<分项>/<规则名>`（`content/项目代号`）
        SecurityRule = "security_rule",
        Plugin = "plugin",
        /// 上游的先后（没写路由时就是故障转移的顺序）。名字是空的
        ProviderOrder = "provider_order",
        /// 路由的先后。名字是空的
        RouteOrder = "route_order",
        /// 插件的先后（运行的顺序）。名字是空的
        PluginOrder = "plugin_order",
        /// 全局设置的一节：`failover` / `security` / `default_route` / `default_key` /
        /// `client_probes` / `pricing`
        Global = "global",
        /// Lite 的通用偏好。名字是空的
        ClientPrefs = "client_prefs",
    }
}

/// 一个项目：种类加名字。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ItemRef {
    pub kind: ItemKind,
    pub name: String,
}

/// `GET /config/items`：配置里的每个项目，和它引用的项目（导出所选项目时随附的那些）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ItemNode {
    pub item: ItemRef,
    /// 直接引用的项目：路由 → 上游、策略组；上游 → 出站代理、价目表……
    pub refs: Vec<ItemRef>,
}

/// 导出哪些。线上是 `"all"`，或者 `{ "items": [ItemRef, …] }`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ExportScope {
    /// 全部配置
    All,
    /// 这几项，**连同它们引用的项目**（core 补齐）
    Items(Vec<ItemRef>),
}

/// `POST /config/export`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExportRequest {
    pub scope: ExportScope,
    /// 带上密钥：上游密钥、请求头的值、AWS 凭据、代理密码、网关密钥。不带时这些值是空的，
    /// 写成 `${变量名}` 的照样带着（那不是密钥）
    pub secrets: bool,
    /// 用口令加密。至少 8 个字符；不给是不加密
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Lite 的通用偏好，**经远程连接导出时由 Lite 附上**（那时偏好在 Lite 那边）。经本机连接
    /// 导出时不看它：用 core 存着的那一份
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub client_prefs: Option<serde_json::Value>,
}

/// 导出（或另存）的文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExportedFile {
    /// 建议的文件名，`ThinkWatch-2026-10-18.twconfig`
    pub file_name: String,
    /// 文件内容，base64
    pub data_b64: String,
    /// 包里有哪些项目（随附的也在里面）
    pub included: Vec<ItemRef>,
    /// 登录没有随文件导出的上游（ChatGPT 账号）：导入之后要重新登录
    pub omitted_logins: Vec<String>,
}

/// 一个配置包的内容，导入之前看。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportSource {
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

slug_enum! {
    /// 完整的配置，还是选出来的几项。
    pub enum PackKind {
        Full = "full",
        /// 选择项目导出、分享
        Partial = "partial",
    }
}

slug_enum! {
    /// 配置包是怎么来的。
    pub enum PackOrigin {
        Export = "export",
        Share = "share",
        Ui = "ui",
        Cli = "cli",
        External = "external",
        Defaults = "defaults",
        Sync = "sync",
        Import = "import",
        Restore = "restore",
        Repair = "repair",
        /// 更新的版本才有的来源
        Other = "other",
    }
}

/// 配置包里有多少东西。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PackCounts {
    pub providers: u32,
    pub proxies: u32,
    pub clients: u32,
    pub aliases: u32,
    pub groups: u32,
    pub routes: u32,
    pub price_sheets: u32,
    pub security_rules: u32,
    pub plugins: u32,
}

/// 配置包的说明（`manifest.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PackInfo {
    pub kind: PackKind,
    pub created_at_ms: u64,
    /// 哪个版本的 Core 写的
    pub core_version: String,
    /// 带没带密钥
    pub secrets: bool,
    pub encrypted: bool,
    pub origin: PackOrigin,
    pub counts: PackCounts,
}

/// `POST /config/import/preview`：导入会怎么做。**什么都不写**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportPreview {
    pub manifest: PackInfo,
    /// 按磁盘上哪一版算的。导入时带回来：配置在这中间变了就是 409，重新预览
    pub base_version: String,
    /// 包里的每个项目，按种类、名字排。顺序项目不在里面：新加的接在后面
    pub rows: Vec<ImportRow>,
    /// 包里的 Lite 通用偏好。**经远程连接导入时由 Lite 照它在本机应用**，core 不写；经本机
    /// 连接导入时它是 `client_prefs` 那一行，core 写下并发 [`crate::Event::ClientPrefsChanged`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub client_prefs: Option<serde_json::Value>,
    /// 较旧版本导出的包里读不进来的字段，按一键修复的规矩（不认识的字段删掉、取值不合法的
    /// 回到默认值）**在内存里修过了**：预览和导入都按修好的算，包本身不变。没修过是空的
    pub repairs: Vec<crate::ConfigFix>,
}

slug_enum! {
    /// 包里的一项和本机比。
    pub enum ImportStatus {
        /// 本机没有：直接导入，可以不选
        New = "new",
        /// 本机有，内容一样：不处理
        Same = "same",
        /// 本机有，内容不同：必须选覆盖、改名导入或跳过（全局设置只有覆盖、跳过）
        Conflict = "conflict",
        /// 导入不了，原因在 `error`
        Error = "error",
    }
}

slug_enum! {
    /// 导入时要知道的一件事。
    pub enum ImportFlag {
        /// 包里不带密钥：上游导入为停用，填上密钥之后启用；网关密钥重新生成
        MissingKey = "missing_key",
        /// ChatGPT 账号上游：登录不随文件带，导入为停用，登录之后启用
        NeedsLogin = "needs_login",
        /// 插件：导入为停用。改得了工具调用的插件启用时要批准
        PluginDisabled = "plugin_disabled",
    }
}

/// 包里的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportRow {
    pub item: ItemRef,
    pub status: ImportStatus,
    /// 全局设置、通用偏好：冲突时只能覆盖或跳过
    pub global: bool,
    pub flags: Vec<ImportFlag>,
    /// 冲突时哪几个合并单位不同：上游是 `conn`（地址、协议、密钥、请求头、代理）、`models`、
    /// `billing`、`other`（并发、身份、余额、停用）；插件是 `script`、`settings`、`enabled`；
    /// 全局设置和通用偏好是字段的路径（`failover.pause_secs`）；别的是 `all`
    pub differs: Vec<String>,
    /// 改名导入时的缺省名字（`公司中转 (2)`）。冲突的、有名字的项目才有
    pub suggested_name: Option<String>,
    pub error: Option<Msg>,
    /// 它引用的、包里也有的项目。**被跳过的项目被谁引用着，谁就导入不了**（除非引用的那一项
    /// 是改名导入）：界面按它把那几行标成错误
    pub refs: Vec<ItemRef>,
}

slug_enum! {
    /// 对一项怎么处理。
    pub enum ImportAction {
        /// 本机没有的：导入
        Import = "import",
        /// 用包里的替换本机同名的那一项。本机引用它的地方不变
        Overwrite = "overwrite",
        /// 用新名字导入，包里引用它的项目跟着改用新名字
        Rename = "rename",
        Skip = "skip",
    }
}

/// 对一项的处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportDecision {
    pub item: ItemRef,
    pub action: ImportAction,
    /// 改名导入的新名字。自定义安全规则只写规则名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

/// `POST /config/import`。没有写进 `decisions` 的项目不导入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportRequest {
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// 预览时的那一版
    pub base_version: String,
    pub decisions: Vec<ImportDecision>,
}

/// 一个版本里有多少东西。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BackupCounts {
    pub providers: u32,
    pub plugins: u32,
}

/// 一个备份版本（`GET /backups`），新的在前。本机的和同步位置上的同一个版本是一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BackupEntry {
    /// 恢复、另存时带上
    pub id: String,
    pub at_ms: u64,
    pub origin: ConfigOrigin,
    /// 同步来的版本：哪台设备上传的
    pub device: Option<String>,
    /// 存在本机
    pub local: bool,
    /// 存在同步位置
    pub sync: bool,
    pub counts: BackupCounts,
    pub core_version: String,
    /// 和现在的配置是同一份内容
    pub current: bool,
}

/// `POST /backups/file`：一个版本另存为配置包。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BackupFileRequest {
    pub id: String,
    /// 用口令加密。至少 8 个字符；不给是不加密
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

/// `POST /backups/preview`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RestorePreviewRequest {
    pub id: String,
}

/// 恢复会改什么。**什么都不写**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RestorePreview {
    /// 恢复时带回来
    pub base_version: String,
    /// 和现在不同的项目。缺省全选；取消的保持现在的样子
    pub rows: Vec<RestoreRow>,
    /// 开着同步时，恢复的结果会同步到几台别的设备。没开同步是 0
    pub sync_devices: u32,
    /// 那一版里这一版 Core 读不进来的字段（升级之前留的版本），按一键修复的规矩在内存里修过了。
    /// 没修过是空的
    pub repairs: Vec<crate::ConfigFix>,
}

slug_enum! {
    /// 恢复对一项做什么。
    pub enum RestoreChange {
        /// 那一版有、现在没有
        Add = "add",
        /// 现在有、那一版没有
        Remove = "remove",
        Modify = "modify",
    }
}

slug_enum! {
    /// 恢复时为什么留着它。
    pub enum KeptReason {
        /// 本机接管的客户端正在用这把网关密钥：不删
        KeyInUse = "key_in_use",
    }
}

/// 恢复里的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RestoreRow {
    pub item: ItemRef,
    pub change: RestoreChange,
    /// 不照那一版改的原因。有的话这一项保持现在的样子
    pub kept: Option<KeptReason>,
}

/// `POST /backups/restore`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RestoreRequest {
    pub id: String,
    pub base_version: String,
    /// 只恢复这几项。不给是全部
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ItemRef>>,
}

/// Lite 的通用偏好（语言、外观、菜单栏、通知、检查更新），**core 不解析**，最大 64 KiB。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ClientPrefs {
    /// 还没存过是空
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub prefs: Option<serde_json::Value>,
}

/// Lite 的通用偏好最大多少字节（写成 JSON 之后）
pub const CLIENT_PREFS_MAX: usize = 64 * 1024;

/// `PUT /pinned-keys`：本机接管的客户端正在用的网关密钥。恢复、导入、同步不删它们。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PinnedKeys {
    pub names: Vec<String>,
}
