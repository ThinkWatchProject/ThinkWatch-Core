//! 价目表：默认价目表的刷新、自定义价目表的增删改、查价。
//!
//! 两层（见 `tw_pricing`）：
//!
//! - **默认价目表**是公开价格数据集。随版本内置一份；联网刷新之后存在配置
//!   目录的数据文件里 —— 它是数据，不是配置：不进版本历史，也不该跟着配置
//!   回滚。`pricing.auto_update` 开着时每天刷新一次。
//! - **自定义价目表**写在 config.yaml 的 `pricing.sheets` 里，上游用
//!   `pricing:` 选一张。增删改和上游、代理走同一条路：结构进来，
//!   `tw_config::edit` 落盘，改名时引用跟着改，被引用时不能删。
//!
//! 所有金额都从网关那一份价格簿里算（`AppState::pricing`）。**这里不自己
//! 实现计价顺序** —— 界面要显示什么价，就来问 `/pricing/query`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde_yaml_ng::Value;
use tw_config::edit;
use tw_config::history::Origin;
use tw_config::refs;
use tw_yaml::Step;

use crate::contract::RouterExt;
use crate::resources::{checked_name, invalid, mapping, quoted};
use crate::{ApplyError, ControlState, Fail, apply_fail, fail};
use tw_api::ep;
use tw_types::{Msg, msg};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Pricing, status)
        .at(ep::RefreshPricing, refresh_now)
        .at(ep::SetPricingAutoUpdate, set_auto_update)
        .at(ep::QueryPrice, query)
        .at(ep::CreatePriceSheet, create_sheet)
        .at(ep::PriceSheet, sheet)
        .at(ep::UpdatePriceSheet, update_sheet)
        .at(ep::DeletePriceSheet, delete_sheet)
}

// ─────────────────────────────────────────────────────────── 默认价目表

/// 联网刷新下来的默认价目表，存在配置文件旁边。
const DATA_FILE: &str = "model_prices.json";

/// 数据集现在两 MB 多。**上限给到 32 MB**：够它再长十倍，又不至于让一个
/// 出错的响应把内存吃满。
const MAX_DATASET: usize = 32 * 1024 * 1024;

/// 整个下载的时限。网关的客户端没有整体超时（一个长任务不该被掐断），
/// 而一次后台刷新不该无限期挂着。
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

fn data_path(config: &Path) -> PathBuf {
    config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(DATA_FILE)
}

/// 启动时用哪份默认价目表：上次刷新存下的那份，或者随版本内置的 —— **哪份
/// 新用哪份**。
///
/// 存下的那份比内置的旧，说明应用升级过、之后还没刷新成功过。存下的那份
/// 读不了（被截断、被改坏）也用内置的：一个坏掉的数据文件不该让每一笔费用
/// 都变成无法计价。
pub fn load_table(config: &Path) -> tw_pricing::Table {
    let saved = read_saved(&data_path(config));
    match tw_pricing::Table::builtin() {
        Ok(builtin) => match saved {
            Some(t) if t.date >= builtin.date => t,
            _ => builtin,
        },
        Err(e) => {
            tracing::warn!("the built-in price sheet failed to load: {e}");
            saved.unwrap_or_else(tw_pricing::Table::empty)
        }
    }
}

fn read_saved(path: &Path) -> Option<tw_pricing::Table> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                "{} could not be read; pricing with the built-in sheet: {e}",
                path.display()
            );
            return None;
        }
    };
    // 数据日期是这份文件写下的那天，也就是刷新成功的那天
    let saved_at = saved_at(path)?;
    match tw_pricing::Table::fetched(&raw, local_date(saved_at)) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                "{} could not be parsed; pricing with the built-in sheet: {e}",
                path.display()
            );
            None
        }
    }
}

fn saved_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn local_date(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d").to_string()
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 刷新默认价目表的那个后台任务，以及它最近一次的结果。
pub struct Updater {
    /// 去哪儿拉。**只能在代码里给，不从配置读** —— 见 `tw_pricing::UPDATE_URL`
    url: String,
    schedule: Schedule,
    last: std::sync::Mutex<Attempt>,
    /// 同一时间只有一次刷新在跑
    running: tokio::sync::Mutex<()>,
    /// 定期刷新刚被打开：不用等到下一轮
    wake: tokio::sync::Notify,
}

/// 什么时候去刷新。
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    /// 启动之后多久第一次检查。**不在启动那一刻** —— 那时网关在起、客户端在连
    pub first_check: Duration,
    /// 多久看一次到没到时间。看一次只是比较时间，不联网。
    ///
    /// **不是睡满一整天再醒。**笔记本合盖时单调时钟不走，睡「24 小时」
    /// 可能睡上好几天；按墙上时间一轮轮比，开盖之后下一轮就会刷新。
    pub tick: Duration,
    /// 上次刷新成功之后多久再刷新
    pub every: Duration,
    /// 失败之后多久再试。离线的时候每一轮都去撞一次没有意义
    pub retry: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            first_check: Duration::from_secs(60),
            tick: Duration::from_secs(10 * 60),
            every: Duration::from_secs(24 * 3600),
            retry: Duration::from_secs(3600),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Attempt {
    at_ms: Option<u64>,
    error: Option<String>,
}

/// 刷新默认价目表没成。
///
/// **英文只写一遍**：`Display` 就是 [`RefreshError::msg`] 的原句（它也是价目
/// 页上「上次刷新失败」那一行），界面拿码去翻。`detail` 都是网络库、JSON
/// 解析器或文件系统的原话。
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("{}", self.msg())]
    Unreachable(String),
    #[error("{}", self.msg())]
    Status(u16),
    #[error("{}", self.msg())]
    BrokeOff(String),
    #[error("{}", self.msg())]
    TooLarge { mb: usize },
    #[error("{}", self.msg())]
    Dataset(String),
    #[error("{}", self.msg())]
    Save(String),
}

impl RefreshError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        match self {
            RefreshError::Unreachable(d) => msg!(
                "control.pricing.unreachable", detail = d =>
                "the price data source could not be reached: {detail}"
            ),
            RefreshError::Status(status) => msg!(
                "control.pricing.status", status = status =>
                "the price data source answered HTTP {status}"
            ),
            RefreshError::BrokeOff(d) => msg!(
                "control.pricing.broke_off", detail = d =>
                "the download broke off: {detail}"
            ),
            RefreshError::TooLarge { mb } => msg!(
                "control.pricing.too_large", mb = mb =>
                "what was downloaded is not a price data set: the file is over {mb} MB"
            ),
            RefreshError::Dataset(d) => msg!(
                "control.pricing.not_a_dataset", detail = d =>
                "what was downloaded is not a price data set: {detail}"
            ),
            RefreshError::Save(d) => msg!(
                "control.pricing.save_failed", detail = d =>
                "the price sheet could not be saved: {detail}"
            ),
        }
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new(tw_pricing::UPDATE_URL, Schedule::default())
    }
}

impl Updater {
    pub fn new(url: impl Into<String>, schedule: Schedule) -> Self {
        Self {
            url: url.into(),
            schedule,
            last: Default::default(),
            running: Default::default(),
            wake: Default::default(),
        }
    }

    fn last(&self) -> Attempt {
        self.last.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn record(&self, r: &Result<usize, RefreshError>, at: SystemTime) {
        if let Ok(mut last) = self.last.lock() {
            last.at_ms = Some(millis(at));
            last.error = r.as_ref().err().map(ToString::to_string);
        }
    }

    /// 到没到该刷新的时候。
    fn due(&self, config: &Path, now: SystemTime) -> bool {
        let fresh = saved_at(&data_path(config))
            // 文件时间比现在还晚（时钟往回调过）：算刚刷新过，而不是每一轮都去拉
            .map(|t| now.duration_since(t).unwrap_or_default())
            .is_some_and(|age| age < self.schedule.every);
        if fresh {
            return false;
        }
        match self.last() {
            Attempt {
                at_ms: Some(at),
                error: Some(_),
            } => millis(now).saturating_sub(at) >= self.schedule.retry.as_millis() as u64,
            _ => true,
        }
    }

    async fn pause(&self, d: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = self.wake.notified() => {}
        }
    }
}

/// 起那个后台任务。
pub fn spawn(s: ControlState) {
    tokio::spawn(async move {
        let u = s.price_updater.clone();
        u.pause(u.schedule.first_check).await;
        loop {
            if s.config().pricing.auto_update
                && u.due(s.config_path(), SystemTime::now())
                && let Err(e) = refresh(&s).await
            {
                tracing::info!("the default price sheet could not be updated; retrying later: {e}");
            }
            u.pause(u.schedule.tick).await;
        }
    });
}

/// 刷新一次，返回有几个模型的价格变了。
///
/// **先落盘再换表** —— 换了但没存下来的话，重启之后又回到旧表，而界面
/// 刚说过已经更新。
pub async fn refresh(s: &ControlState) -> Result<usize, RefreshError> {
    let u = s.price_updater.clone();
    let _one = u.running.lock().await;
    let result = match fetch(&u.url).await {
        Ok(raw) => {
            let s = s.clone();
            // 解析两 MB 的 JSON、写文件：都不该占着异步线程
            tokio::task::spawn_blocking(move || apply(&s, &raw))
                .await
                .unwrap_or_else(|e| Err(RefreshError::Save(e.to_string())))
        }
        Err(e) => Err(e),
    };
    u.record(&result, SystemTime::now());
    result
}

/// **不用数据面那个客户端**：那个不跟重定向（它发的请求带凭据）。这里什么
/// 凭据都不带，托管地址搬了家 GitHub 回的是 301，跟过去才拉得到。
async fn fetch(url: &str) -> Result<Vec<u8>, RefreshError> {
    let http = tw_gateway::public_client().map_err(|e| RefreshError::Unreachable(e.detail.text))?;
    let mut resp = http
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| RefreshError::Unreachable(chain(&e)))?;
    if !resp.status().is_success() {
        return Err(RefreshError::Status(resp.status().as_u16()));
    }
    let mut raw = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| RefreshError::BrokeOff(chain(&e)))?
    {
        if raw.len() + chunk.len() > MAX_DATASET {
            return Err(RefreshError::TooLarge {
                mb: MAX_DATASET / 1024 / 1024,
            });
        }
        raw.extend_from_slice(&chunk);
    }
    Ok(raw)
}

fn apply(s: &ControlState, raw: &[u8]) -> Result<usize, RefreshError> {
    // 解析器那句外面还套着一层「价格数据解析不了」，和这里要说的是同一件事，
    // 只取里面那句原话
    let table = tw_pricing::Table::fetched(raw, local_date(SystemTime::now())).map_err(|e| {
        RefreshError::Dataset(match e {
            tw_pricing::PricingError::Dataset(d) | tw_pricing::PricingError::Snapshot(d) => d,
        })
    })?;
    // 解析成功就一定是 UTF-8（JSON 只能是）
    let text = std::str::from_utf8(raw).map_err(|e| RefreshError::Dataset(e.to_string()))?;
    tw_config::store::write_atomic(&data_path(s.config_path()), text)
        .map_err(|e| RefreshError::Save(e.to_string()))?;
    let changed = table.changed_from(s.gateway.pricing.load().table());
    s.gateway.set_price_table(table);
    Ok(changed)
}

/// reqwest 的错误只说最外面一层（「error sending request」），原因在里面。
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push('：');
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

async fn status(State(s): State<ControlState>) -> Json<tw_api::PricingStatus> {
    Json(pricing_status(&s).await)
}

async fn pricing_status(s: &ControlState) -> tw_api::PricingStatus {
    let (date, source, models) = {
        let book = s.gateway.pricing.load();
        let t = book.table();
        (t.date.clone(), t.source.slug().to_string(), t.len())
    };
    let last = s.price_updater.last();
    // **拿不到存储就是 0** —— 观测层起不来时网关照常转发，这一页也该照常打开
    let (unpriced_recent, unpriced_models) = match &s.store {
        Some(st) => st.lock().await.db().unpriced_recent(7).unwrap_or_else(|e| {
            tracing::debug!("the count of unpriced requests could not be read: {e}");
            (0, Vec::new())
        }),
        None => (0, Vec::new()),
    };
    tw_api::PricingStatus {
        date,
        source,
        models,
        auto_update: s.config().pricing.auto_update,
        checked_at_ms: last.at_ms,
        error: last.error,
        unpriced_recent,
        unpriced_models,
    }
}

/// 立即刷新。**不看定期刷新开没开** —— 这是用户自己点的。
async fn refresh_now(
    State(s): State<ControlState>,
) -> Result<Json<tw_api::PricingRefreshed>, Fail> {
    let changed = refresh(&s).await.map_err(|e| {
        let code = match e {
            RefreshError::Save(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_GATEWAY,
        };
        fail(code, e.msg())
    })?;
    Ok(Json(tw_api::PricingRefreshed {
        status: pricing_status(&s).await,
        changed,
    }))
}

async fn set_auto_update(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AutoUpdateSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            // 开着是默认值，**默认值不写进文件**
            let value = (!req.on).then_some(Value::Bool(false));
            Ok(edit::set(
                text,
                &[Step::key("pricing"), Step::key("auto_update")],
                value.as_ref(),
            )?)
        })
        .await
        .map_err(apply_fail)?;
    if req.on {
        s.price_updater.wake.notify_one();
    }
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ─────────────────────────────────────────────────────────── 查价

/// 搜索默认最多返回几个。
const SEARCH_LIMIT: usize = 50;
const SEARCH_LIMIT_MAX: usize = 500;

async fn query(
    State(s): State<ControlState>,
    Json(q): Json<tw_api::PriceQuery>,
) -> Result<Json<tw_api::PriceQueryResult>, Fail> {
    let loaded = s.gateway.pricing.load_full();
    let (book, sheet) = match q.sheet {
        tw_api::SheetRef::Default => (loaded, None),
        tw_api::SheetRef::Named { name } => {
            if loaded.config().sheet(&name).is_none() {
                return Err(fail(
                    StatusCode::NOT_FOUND,
                    msg!(
                        "control.sheet_not_found", sheet = name =>
                        "There is no price sheet named `{sheet}`."
                    ),
                ));
            }
            (loaded, Some(name))
        }
        tw_api::SheetRef::Draft { sheet } => {
            let def = sheet_def(&sheet).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let name = def.name.clone();
            (Arc::new(loaded.with_draft(def)), Some(name))
        }
    };
    let (models, matched) = if q.models.is_empty() {
        search(
            &book,
            sheet.as_deref(),
            q.search.as_deref().unwrap_or_default(),
            q.limit.unwrap_or(SEARCH_LIMIT).min(SEARCH_LIMIT_MAX),
        )
    } else {
        let n = q.models.len();
        (q.models, n)
    };
    let date = &book.table().date;
    let items = models
        .into_iter()
        .map(|model| {
            let r = book.resolve(sheet.as_deref(), &model);
            tw_api::ResolvedPrice {
                price: r
                    .as_ref()
                    .map(|r| price_fields(&tw_pricing::PerMillion::of(&r.price))),
                source: r.as_ref().map(|r| tw_store::price_source(&r.source, date)),
                estimated: r.as_ref().is_some_and(|r| r.cross_platform),
                max_input_tokens: r.as_ref().and_then(|r| r.price.max_input_tokens),
                model,
            }
        })
        .collect();
    Ok(Json(tw_api::PriceQueryResult { items, matched }))
}

/// 按名字搜模型：默认价目表里的，加上这张价目表单独覆盖的。
///
/// **不带平台前缀的名字排在前面。**数据集里同一个模型常有七八个写法
/// （`bedrock/…`、`vertex_ai/…`、`anthropic.…-v1:0`），客户端发来的是不带
/// 前缀的那个。
fn search(
    book: &tw_pricing::PriceBook,
    sheet: Option<&str>,
    needle: &str,
    limit: usize,
) -> (Vec<String>, usize) {
    let needle = needle.trim().to_lowercase();
    let mut names: std::collections::BTreeSet<&str> = book.table().models().collect();
    if let Some(def) = sheet.and_then(|n| book.config().sheet(n)) {
        names.extend(def.models.keys().map(String::as_str));
    }
    let mut hits: Vec<&str> = names
        .into_iter()
        .filter(|m| needle.is_empty() || m.to_lowercase().contains(&needle))
        .collect();
    let prefixed = |m: &str| m.contains('/') || m.contains(':');
    hits.sort_by_key(|m| prefixed(m));
    let matched = hits.len();
    (
        hits.into_iter().take(limit).map(str::to_string).collect(),
        matched,
    )
}

// ─────────────────────────────────────────────────────────── 自定义价目表

/// 一张价目表的完整定义。编辑对话框从这里取。
async fn sheet(
    State(s): State<ControlState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<tw_api::PriceSheetInput>, Fail> {
    let cfg = s.config();
    let def = cfg.pricing.sheet(&name).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            msg!(
                "control.sheet_not_found", sheet = name =>
                "There is no price sheet named `{sheet}`."
            ),
        )
    })?;
    Ok(Json(tw_api::PriceSheetInput {
        name: def.name.clone(),
        multiplier: def.multiplier,
        models: def
            .models
            .iter()
            .map(|(m, p)| (m.clone(), price_fields(p)))
            .collect(),
    }))
}

async fn create_sheet(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PriceSheetSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let def = sheet_def(&req.sheet).map_err(invalid)?;
            let mut out = edit::upsert(text, edit::PRICE_SHEETS, None, &mapping(&def)?)?;
            if let Some(used_by) = &req.used_by {
                out = assign(&out, cfg, None, &def.name, used_by)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn update_sheet(
    State(s): State<ControlState>,
    UrlPath(name): UrlPath<String>,
    Json(req): Json<tw_api::PriceSheetSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let def = sheet_def(&req.sheet).map_err(invalid)?;
            let mut out = edit::upsert(text, edit::PRICE_SHEETS, Some(&name), &mapping(&def)?)?;
            if def.name != name {
                // **和那一张在同一个版本里改** —— 分两次写的话，中间那一版的
                // 上游选着一张不存在的价目表，会被校验拒掉
                out = refs::rename_sheet(&out, cfg, &name, &def.name)?;
            }
            if let Some(used_by) = &req.used_by {
                out = assign(&out, cfg, Some(&name), &def.name, used_by)?;
            }
            Ok(out)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn delete_sheet(
    State(s): State<ControlState>,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let users = refs::sheet_users(cfg, &name);
            if !users.is_empty() {
                return Err(ApplyError::InUse(msg!(
                    "control.sheet_in_use", sheet = name, upstreams = quoted(&users) =>
                    "Price sheet `{sheet}` is still used by upstream {upstreams}; unlink those \
                     before deleting it."
                )));
            }
            Ok(edit::remove(text, edit::PRICE_SHEETS, &name)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 让恰好 `used_by` 这几家上游使用价目表 `name`。
///
/// `cfg` 是写之前的那一份（下标对得上）；`old` 是改名前的名字 —— 改名的
/// 引用已经跟着改过，所以原来用 `old` 的上游这时候用的是 `name`。
fn assign(
    text: &str,
    cfg: &tw_config::Config,
    old: Option<&str>,
    name: &str,
    used_by: &[String],
) -> Result<String, ApplyError> {
    if let Some(missing) = used_by
        .iter()
        .find(|u| !cfg.providers.iter().any(|p| &p.name == *u))
    {
        return Err(ApplyError::Edit(edit::EditError::NotFound {
            what: "upstream",
            name: missing.clone(),
        }));
    }
    let mut out = text.to_string();
    for (i, p) in cfg.providers.iter().enumerate() {
        let current = match p.pricing.as_deref() {
            Some(s) if Some(s) == old => Some(name),
            other => other,
        };
        let path = [Step::key("providers"), Step::Index(i), Step::key("pricing")];
        match (used_by.contains(&p.name), current == Some(name)) {
            (true, false) => {
                out = edit::set(&out, &path, Some(&Value::String(name.to_string())))?;
            }
            // 原来用它、不在列表里的：改回默认价目表
            (false, true) => out = edit::set(&out, &path, None)?,
            _ => {}
        }
    }
    Ok(out)
}

/// 概览里的自定义价目表。
pub(crate) fn sheet_views(cfg: &tw_config::Config) -> Vec<tw_api::PriceSheetView> {
    cfg.pricing
        .sheets
        .iter()
        .map(|sh| tw_api::PriceSheetView {
            name: sh.name.clone(),
            multiplier: sh.multiplier,
            overrides: sh.models.len(),
            used_by: refs::sheet_users(cfg, &sh.name),
        })
        .collect()
}

fn sheet_def(input: &tw_api::PriceSheetInput) -> Result<tw_pricing::SheetDef, Msg> {
    let def = tw_pricing::SheetDef {
        name: checked_name(&input.name, "price sheet")?,
        multiplier: input.multiplier,
        models: input
            .models
            .iter()
            .map(|(m, p)| (m.clone(), per_million(p)))
            .collect(),
    };
    def.validate().map_err(|e| e.msg())?;
    Ok(def)
}

fn per_million(p: &tw_api::PriceFields) -> tw_pricing::PerMillion {
    tw_pricing::PerMillion {
        input: p.input,
        output: p.output,
        cache_read: p.cache_read,
        cache_write_5m: p.cache_write_5m,
        cache_write_1h: p.cache_write_1h,
        input_above_200k: p.input_above_200k,
        output_above_200k: p.output_above_200k,
    }
}

pub(crate) fn price_fields(p: &tw_pricing::PerMillion) -> tw_api::PriceFields {
    tw_api::PriceFields {
        input: p.input,
        output: p.output,
        cache_read: p.cache_read,
        cache_write_5m: p.cache_write_5m,
        cache_write_1h: p.cache_write_1h,
        input_above_200k: p.input_above_200k,
        output_above_200k: p.output_above_200k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_table_older_than_the_builtin_one_is_not_used() {
        let d = tempfile::tempdir().unwrap();
        let config = d.path().join("config.yaml");
        std::fs::write(
            data_path(&config),
            r#"{"m":{"input_cost_per_token":1e-6,"output_cost_per_token":2e-6}}"#,
        )
        .unwrap();
        // 刚写的：比内置快照新
        let t = load_table(&config);
        assert_eq!(t.source, tw_pricing::TableSource::Fetched);
        assert_eq!(t.len(), 1);

        // 改成内置快照之前的日期：应用升级过，那份过时了
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(data_path(&config))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(load_table(&config).source, tw_pricing::TableSource::Builtin);
    }

    #[test]
    fn a_broken_saved_table_falls_back_to_the_builtin_one() {
        let d = tempfile::tempdir().unwrap();
        let config = d.path().join("config.yaml");
        std::fs::write(data_path(&config), "{\"truncated").unwrap();
        assert_eq!(load_table(&config).source, tw_pricing::TableSource::Builtin);
    }

    #[test]
    fn a_refresh_is_due_once_the_saved_table_is_a_day_old_and_a_failure_waits_an_hour() {
        let d = tempfile::tempdir().unwrap();
        let config = d.path().join("config.yaml");
        let u = Updater::default();
        let now = SystemTime::now();
        // 从没刷新过
        assert!(u.due(&config, now));
        // 刚刷新过
        std::fs::write(data_path(&config), "{}").unwrap();
        assert!(!u.due(&config, now));
        // 一天之后
        let tomorrow = now + Duration::from_secs(24 * 3600 + 1);
        assert!(u.due(&config, tomorrow));
        // 那时刷新失败了：一小时之内不再试
        u.record(&Err(RefreshError::Unreachable("离线".into())), tomorrow);
        assert!(!u.due(&config, tomorrow + Duration::from_secs(60)));
        assert!(u.due(&config, tomorrow + Duration::from_secs(3601)));
    }
}

#[cfg(test)]
mod msg_codes {
    use super::*;

    #[test]
    fn every_refresh_error_has_its_own_code() {
        let all = [
            RefreshError::Unreachable("x".into()),
            RefreshError::Status(503),
            RefreshError::BrokeOff("x".into()),
            RefreshError::TooLarge { mb: 16 },
            RefreshError::Dataset("x".into()),
            RefreshError::Save("x".into()),
        ];
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            let m = e.msg();
            assert!(m.code.starts_with("control.pricing."), "{m:?}");
            assert!(!m.text.is_empty() && m.text == e.to_string(), "{m:?}");
            assert!(seen.insert(m.code.clone()), "码重复了：{}", m.code);
        }
    }
}
