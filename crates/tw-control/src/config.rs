//! 配置的生命周期：谁改的、怎么进来的、进不来时怎么办。
//!
//! 这是「两个写入方，一份文件」那道题的答案所在。四个问题里，最小替换
//! 归 `tw-yaml`，指纹和冲突归 `tw_config::store`，三道校验归
//! `tw_config::reload` —— 这里负责**把它们串成一条能跑的路**，并且保证
//! 无论从哪个方向进来，走的都是同一条。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tw_config::history::Origin;
use tw_config::store::{self, Fingerprint};
use tw_types::{Msg, msg};

/// 配置的唯一入口。**UI、CLI、文件监听都走它** —— 三条路各写一遍，
/// 就会有两条忘了存历史、一条忘了防回环。
pub struct ConfigManager {
    path: PathBuf,
    gateway: tw_gateway::AppState,
    bus: tw_observe::EventBus,
    /// 上一次**我们自己**写下去的样子。文件事件来了先和它比 ——
    /// 一样就是自己写的，直接忽略，否则会形成回环。
    seen: Mutex<Option<Fingerprint>>,
}

/// 一次配置改动没成的原因。
///
/// **每一种都带着自己那句带码的话**（[`ApplyError::msg`]），控制面原样发给
/// 界面。以前这里存的是拼好的英文，发出去时再整句塞进一个 `{detail}`
/// —— 界面拿到的码只说「改配置失败了」，真正的原因在一句它翻不了的英文里。
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Store(#[from] store::StoreError),
    /// 三道校验里的任意一道没过。**旧配置还在服务。**
    #[error("{0}")]
    Rejected(tw_config::Rejected),
    /// 校验过了但运行时对象建不起来 —— 同样保持旧的。里面是数据面说的那句
    #[error("the configuration parses but cannot be applied: {0}")]
    Build(Msg),
    /// patch 指的那个位置有问题。**和 Build 分开**：那句「配置能读但
    /// 用不起来」会让人去查配置，而该查的是这次请求写的路径。
    #[error("{0}")]
    BadPath(Msg),
    #[error("{}", self.msg())]
    Stale { base: String, current: String },
    /// 按资源改（上游、代理、价目表）时的失败：名字撞了、找不到、值写不进去。
    #[error(transparent)]
    Edit(#[from] tw_config::edit::EditError),
    /// 交过来的东西本身写得不对（名字空着、凭据写法不对、规则缺值）。
    /// **和 `Edit` 分开**：那边是「配置现在不允许」，这边是「请求写错了」
    #[error("{0}")]
    Invalid(Msg),
    /// 还有别的配置在引用它，删不掉。**消息里要说清是谁。**
    #[error("{0}")]
    InUse(Msg),
}

impl ApplyError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        match self {
            ApplyError::Store(e) => e.msg(),
            ApplyError::Rejected(r) => r.msg(),
            // 数据面那句本身就说清了哪个上游、哪个代理，前面不再垫一句
            ApplyError::Build(m)
            | ApplyError::BadPath(m)
            | ApplyError::Invalid(m)
            | ApplyError::InUse(m) => m.clone(),
            ApplyError::Stale { base, current } => msg!(
                "control.config_stale", base = base, current = current =>
                "version mismatch: this edit is based on {base}, and the current version is \
                 {current}. Refresh and edit again"
            ),
            ApplyError::Edit(e) => e.msg(),
        }
    }
}

impl ConfigManager {
    pub fn new(path: PathBuf, gateway: tw_gateway::AppState, bus: tw_observe::EventBus) -> Self {
        let seen = store::read(&path).ok().map(|l| l.fingerprint);
        Self {
            path,
            gateway,
            bus,
            seen: Mutex::new(seen),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 磁盘上现在这一份，连带它的版本号。
    pub fn current(&self) -> Result<tw_config::Loaded, store::StoreError> {
        store::read(&self.path)
    }

    /// 从文件重新读一遍并试着换入。文件监听走这条。
    ///
    /// 返回 `Ok(None)` 表示「这就是我们自己刚写的，什么都没做」。
    pub async fn reload_from_disk(&self) -> Result<Option<String>, ApplyError> {
        let loaded = self.current()?;
        {
            let seen = self.seen.lock().await;
            if let Some(prev) = seen.as_ref()
                && prev.same_content(&loaded.fingerprint)
            {
                // **这是回环的唯一出口。**没有它，我们写一次文件，监听
                // 响一次，我们再重载一次，严重时是个循环。
                return Ok(None);
            }
        }
        let version = self.apply_text(&loaded.text, Origin::External).await?;
        *self.seen.lock().await = Some(loaded.fingerprint);
        Ok(Some(version))
    }

    /// 校验 + 换入 + 记历史，**但不写盘**。
    ///
    /// 文件已经是那个样子了（外部改动），或者调用方接下来自己写。
    async fn apply_text(&self, text: &str, origin: Origin) -> Result<String, ApplyError> {
        let cfg = tw_config::try_parse(text).map_err(|r| {
            // **先告诉界面，再返回错误。**这条路径最常见的调用方是文件
            // 监听，而它的错误没有人接 —— 不发事件的话，用户在编辑器里
            // 写错一个字，界面上什么都不会发生。
            self.bus.emit(tw_api::Event::ConfigRejected {
                id: self.bus.next_id(),
                stage: r.stage.into(),
                message: (*r.message).clone(),
                line: r.line,
                excerpt: r.excerpt.clone(),
                origin: origin.into(),
                at_ms: now_ms(),
            });
            tracing::warn!(
                "the configuration did not validate; staying on the previous version: {r}"
            );
            ApplyError::Rejected(r)
        })?;
        self.gateway
            .reload(cfg)
            .map_err(|e| ApplyError::Build(e.detail))?;
        // 存刚刚生效的这一版。加上写入路径在写之前存的那一次，去重
        // 之后的效果是「每个存在过的版本各一条」，最新那条就是现在跑
        // 着的 —— 于是「回到上一版」在列表上就是第二条，不用数。
        let _ = tw_config::history::snapshot(&self.path, text, origin);
        let version = store::version_of(text);
        tracing::info!(%version, origin = origin.slug(), "the configuration is in effect");
        self.bus.emit(tw_api::Event::ConfigReloaded {
            id: self.bus.next_id(),
            version: version.clone(),
            origin: origin.into(),
            at_ms: now_ms(),
        });
        Ok(version)
    }

    /// 写一份新配置进去。UI 和 CLI 都走这条。
    ///
    /// `base_version` 是乐观并发的凭据：**对不上就是 409**，
    /// 而不是覆盖。`None` 表示调用方明确要覆盖（比如首次生成）。
    pub async fn write(
        &self,
        new_text: &str,
        base_version: Option<&str>,
        origin: Origin,
    ) -> Result<String, ApplyError> {
        let cur = self.current()?;
        if let Some(base) = base_version
            && cur.version() != base
        {
            return Err(ApplyError::Stale {
                base: base.to_string(),
                current: cur.version(),
            });
        }
        // **先校验再写。**写完才发现读不回来，那份坏配置已经在盘上了 ——
        // 而用户下一次启动会撞上它。
        tw_config::try_parse(new_text).map_err(ApplyError::Rejected)?;
        // 改之前那一版进历史。**这一步在写盘之前** —— 写完再存的话，
        // 中间崩一次就永远丢了那一版，而那恰恰是最需要它的时刻。
        let _ = tw_config::history::snapshot(&self.path, &cur.text, origin);
        // 写盘之前再确认一次磁盘还是我们读到的那份 —— 绝不静默覆盖手改
        let fp = store::write_if_unchanged(&self.path, &cur.fingerprint, new_text)?;
        // **先记指纹再换入。**顺序反了的话，文件事件可能在记指纹之前就
        // 到了，于是我们把自己刚写的当成外部改动又重载一遍。
        *self.seen.lock().await = Some(fp);
        self.apply_text(new_text, origin).await
    }

    /// 在当前这一版上做一次改动：`f` 拿到磁盘上的原文和它解析出来的配置，
    /// 返回改完的原文。
    ///
    /// **所有按资源的写入都走这一条**：版本核对、先校验再写、写之前存历史、
    /// 写完换入 —— 和 `patch` 同一条路，只是「怎么改」交给调用方。
    pub async fn transform<F>(
        &self,
        base_version: Option<&str>,
        origin: Origin,
        f: F,
    ) -> Result<String, ApplyError>
    where
        F: FnOnce(&str, &tw_config::Config) -> Result<String, ApplyError>,
    {
        let cur = self.current()?;
        if let Some(base) = base_version
            && cur.version() != base
        {
            return Err(ApplyError::Stale {
                base: base.to_string(),
                current: cur.version(),
            });
        }
        let cfg = tw_config::try_parse(&cur.text).map_err(ApplyError::Rejected)?;
        let text = f(&cur.text, &cfg)?;
        self.write(&text, Some(&cur.version()), origin).await
    }

    /// 回到某一版。
    pub async fn rollback(&self, version: &str) -> Result<String, ApplyError> {
        let all = tw_config::history::list(&self.path)?;
        let target = all
            .iter()
            .rev()
            .find(|v| v.version == version || v.version.ends_with(version))
            .ok_or_else(|| {
                ApplyError::BadPath(msg!(
                    "control.no_such_version", version = version =>
                    "the version history has no {version}"
                ))
            })?;
        let text = tw_config::history::read(target)?;
        let cur = self.current().ok();
        // 回滚也走同一条写入路径，所以它同样会：校验、存历史、防回环。
        self.write(
            &text,
            cur.as_ref().map(|c| c.version()).as_deref(),
            Origin::Rollback,
        )
        .await
    }

    /// 记下一个不是我们写的指纹。**首次运行生成配置之后要调它** ——
    /// 不然那次写会被自己的监听当成外部改动。
    pub async fn mark_written(&self, fp: Fingerprint) {
        *self.seen.lock().await = Some(fp);
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 起一个盯着配置文件的后台任务。
///
/// 返回的 `Watch` **必须留着** —— 扔掉它就停止监听，而那个失效是静默的。
pub fn spawn_watcher(
    mgr: Arc<ConfigManager>,
) -> Result<tw_config::watch::Watch, tw_config::watch::WatchError> {
    let (w, mut rx) = tw_config::watch::watch(mgr.path())?;
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            match mgr.reload_from_disk().await {
                Ok(Some(v)) => {
                    tracing::debug!(version = %v, "the configuration file changed and was reloaded")
                }
                Ok(None) => tracing::trace!("this process wrote that change; ignoring it"),
                // 错误已经在 apply_text 里发过事件了，这里只记一行
                Err(e) => tracing::debug!("the reload failed: {e}"),
            }
        }
    });
    Ok(w)
}

/// 光标落在哪个东西上（反向联动）。
///
/// 返回它所属的**顶层段落和名字**（`providers` / `官方`），因为界面要的
/// 是「显示哪个表单」，而不是精确到字段。
pub async fn path_at(
    axum::extract::State(s): axum::extract::State<crate::ControlState>,
    axum::extract::Query(q): axum::extract::Query<tw_api::ConfigAtQuery>,
) -> Result<axum::Json<tw_api::ConfigAt>, crate::Fail> {
    let cur = s.cfg.current().map_err(crate::unreadable_config)?;
    let path = tw_yaml::path_at(&cur.text, q.offset.min(cur.text.len()));
    let mut out = tw_api::ConfigAt {
        section: None,
        name: None,
    };
    if let Some(p) = path {
        if let Some(tw_yaml::Step::Key(k)) = p.first() {
            out.section = Some(k.clone());
        }
        // 名字从那一项自己的 `name:` 上取 —— 下标对界面没有意义
        if p.len() >= 2
            && let Some(tw_yaml::Step::Index(_)) = p.get(1)
        {
            let mut np = p[..2].to_vec();
            np.push(tw_yaml::Step::Key("name".into()));
            out.name = tw_yaml::find(&cur.text, &np).ok().map(|f| f.value);
        }
    }
    Ok(axum::Json(out))
}

/// 把 `/providers/relay-cn/base_url` 这样的路径解析成 `tw-yaml` 的步骤。
///
/// **用名字而不是下标。**下标会在用户重排上游之后指向另一个东西，而那
/// 种错误完全静默 —— 你以为改的是官方，实际改的是中转。
///
/// 段落是数字时仍然当下标用：`/routes/0/to` 是合理的写法，因为规则的
/// 顺序本身就是它的语义（自上而下首个命中）。
pub fn resolve_path(text: &str, pointer: &str) -> Result<Vec<tw_yaml::Step>, Msg> {
    let nodes = tw_yaml::nodes(text).map_err(|e| e.msg())?;
    let mut out: Vec<tw_yaml::Step> = Vec::new();
    for seg in pointer.trim_matches('/').split('/') {
        if seg.is_empty() {
            continue;
        }
        // 当前位置是个序列吗
        let here = nodes.iter().find(|n| n.path == out);
        let is_seq = matches!(here.map(|n| &n.kind), Some(tw_yaml::NodeKind::Seq));
        if is_seq {
            if let Ok(i) = seg.parse::<usize>() {
                out.push(tw_yaml::Step::Index(i));
                continue;
            }
            // 按 name 找
            let idx = nodes.iter().find_map(|n| {
                let tw_yaml::NodeKind::Scalar { value, .. } = &n.kind else {
                    return None;
                };
                if value != seg || n.path.len() != out.len() + 2 {
                    return None;
                }
                if n.path.last() != Some(&tw_yaml::Step::Key("name".into())) {
                    return None;
                }
                if !n.path.starts_with(&out) {
                    return None;
                }
                match n.path[out.len()] {
                    tw_yaml::Step::Index(i) => Some(i),
                    _ => None,
                }
            });
            match idx {
                Some(i) => out.push(tw_yaml::Step::Index(i)),
                None => {
                    return Err(msg!(
                        "control.patch.no_entry", path = pointer, name = seg =>
                        "{path} has no entry named `{name}`. List entries are found by name, or by index."
                    ));
                }
            }
        } else {
            out.push(tw_yaml::Step::Key(seg.to_string()));
        }
    }
    Ok(out)
}

impl ConfigManager {
    /// 按字段改配置（`PATCH /config`）。
    ///
    /// **所有改动一起算，一起写。**一次 patch 里改三个字段却分三次写盘，
    /// 中间任何一次失败都会留下一份半改的配置 —— 而那份配置是合法的，
    /// 所以没有任何人会发现。
    pub async fn patch(
        &self,
        ops: &[tw_api::PatchOp],
        base_version: Option<&str>,
        origin: Origin,
    ) -> Result<String, ApplyError> {
        let cur = self.current()?;
        if let Some(base) = base_version
            && cur.version() != base
        {
            return Err(ApplyError::Stale {
                base: base.to_string(),
                current: cur.version(),
            });
        }
        let mut text = cur.text.clone();
        for op in ops {
            text = match op {
                tw_api::PatchOp::Replace { path, value } => {
                    let steps = resolve_path(&text, path).map_err(ApplyError::BadPath)?;
                    let scalar = match value {
                        tw_api::PatchValue::Str(v) => tw_yaml::Scalar::Str(v.clone()),
                        tw_api::PatchValue::Int(v) => tw_yaml::Scalar::Int(*v),
                        tw_api::PatchValue::Bool(v) => tw_yaml::Scalar::Bool(*v),
                        tw_api::PatchValue::Null => tw_yaml::Scalar::Null,
                    };
                    // **`insert` 而不是 `set`。**配置里绝大多数字段是可选的、
                    // 默认不写的，只能改「用户碰巧写过」的字段，等于表单模式在
                    // 他最需要的时候是死的。已经写过的走 `set`，那是它的第一步。
                    // 改不动的原因（找不到、不是标量、在锚点里）那一句本身就
                    // 带着路径，前面不再垫一句「改不了」
                    tw_yaml::insert(&text, &steps, &scalar).map_err(bad_path)?
                }
                tw_api::PatchOp::Append { path, item } => {
                    let steps = resolve_path(&text, path).map_err(ApplyError::BadPath)?;
                    tw_yaml::append(&text, &steps, item).map_err(bad_path)?
                }
                tw_api::PatchOp::Remove { path } => {
                    let steps = resolve_path(&text, path).map_err(ApplyError::BadPath)?;
                    // 路径指向那一项，最后一步就是它在列表里的位置。
                    // **按名字解析、按下标删** —— 名字是用户写的，下标是
                    // 我们刚刚算出来的，中间没有任何一次用户可见的重排。
                    let not_an_entry = || {
                        ApplyError::BadPath(msg!(
                            "control.patch.not_an_entry", path = path =>
                            "{path} does not point at an entry of a list. A delete names the \
                             entry, as in /clients/codex."
                        ))
                    };
                    let (last, parent) = steps.split_last().ok_or_else(not_an_entry)?;
                    let tw_yaml::Step::Index(i) = last else {
                        return Err(not_an_entry());
                    };
                    tw_yaml::remove(&text, parent, *i).map_err(bad_path)?
                }
                tw_api::PatchOp::Clear { path } => {
                    // 要清的列表可能根本还没写进文件 —— 「一个都不给」
                    // 正是用户第一次碰 `allow` 的那一下。`resolve_path`
                    // 对没写过的键会原样留成 Key，走得通。
                    let steps = resolve_path(&text, path).map_err(ApplyError::BadPath)?;
                    tw_yaml::clear_seq(&text, &steps).map_err(bad_path)?
                }
            };
        }
        self.write(&text, Some(&cur.version()), origin).await
    }
}

fn bad_path(e: tw_yaml::PatchError) -> ApplyError {
    ApplyError::BadPath(e.msg())
}

#[cfg(test)]
mod patch_seq_tests {
    use super::*;

    const CFG: &str = "version: 1\nclients:\n  # 首次运行生成的\n  - name: default\n    key: tw-aaa\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-a\n";

    /// 这三条是「界面能不能建东西」的全部依据。
    #[test]
    fn append_then_remove_by_name_round_trips_and_keeps_comments() {
        let steps = resolve_path(CFG, "/clients").unwrap();
        let two = tw_yaml::append(CFG, &steps, "name: codex\nkey: tw-bbb").unwrap();
        assert!(two.contains("# 首次运行生成的"), "{two}");
        let cfg: tw_config::Config = serde_yaml_ng::from_str(&two).unwrap();
        assert_eq!(cfg.clients.len(), 2);
        assert_eq!(cfg.clients[1].name, "codex");

        // 按名字定位那一项，再删
        let item = resolve_path(&two, "/clients/codex").unwrap();
        let (last, parent) = item.split_last().unwrap();
        let tw_yaml::Step::Index(i) = last else {
            panic!("按名字解析出来的最后一步应该是下标：{item:?}");
        };
        let back = tw_yaml::remove(&two, parent, *i).unwrap();
        let cfg: tw_config::Config = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.clients[0].name, "default");
        assert!(back.contains("# 首次运行生成的"), "{back}");
    }

    #[test]
    fn a_new_key_can_carry_its_route_binding_in_one_write() {
        // 建密钥和绑路由是**一次写入**，不是两次 —— 中间那一刻
        // 「有一把还没绑路由的密钥」是个用户能看见的中间状态。
        let steps = resolve_path(CFG, "/clients").unwrap();
        let out =
            tw_yaml::append(CFG, &steps, "name: codex\nkey: tw-bbb\nroute: 长上下文").unwrap();
        let cfg: tw_config::Config = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(cfg.clients[1].route.as_deref(), Some("长上下文"));
    }

    #[test]
    fn removing_a_name_that_is_not_there_says_so_instead_of_deleting_something_else() {
        // **最该防的一条**：解析不到就报错，不要退化成「删第 0 项」。
        assert!(resolve_path(CFG, "/clients/不存在").is_err());
    }
}

#[cfg(test)]
mod msg_codes {
    use super::*;

    /// **改配置失败时发出去的是原因自己的码**，不是一个装着英文的套话。
    #[test]
    fn an_apply_error_speaks_with_the_code_of_its_cause() {
        let stale = ApplyError::Stale {
            base: "a".into(),
            current: "b".into(),
        };
        assert_eq!(stale.msg().code, "control.config_stale");
        assert_eq!(stale.msg().text, stale.to_string());
        let inner = msg!("t.x" => "x");
        for e in [
            ApplyError::Invalid(inner.clone()),
            ApplyError::InUse(inner.clone()),
            ApplyError::BadPath(inner.clone()),
            ApplyError::Build(inner.clone()),
        ] {
            assert_eq!(e.msg(), inner);
        }
        let e = ApplyError::Edit(tw_config::edit::EditError::Multiline);
        assert_eq!(e.msg().code, "config.edit.multiline");
        let e = ApplyError::Store(tw_config::StoreError::Missing { path: "/x".into() });
        assert_eq!(e.msg().code, "config.store.missing");
        let e = ApplyError::Rejected(tw_config::try_parse("version: 1\n").unwrap_err());
        assert_eq!(e.msg().code, "config.no_clients");
    }

    #[test]
    fn a_patch_path_that_names_no_entry_has_its_own_code() {
        let cfg = "version: 1\nclients:\n  - name: c\n    key: tw-k\n";
        let m = resolve_path(cfg, "/clients/不存在").unwrap_err();
        assert_eq!(m.code, "control.patch.no_entry");
        assert_eq!(m.arg("name"), "不存在");
    }
}
