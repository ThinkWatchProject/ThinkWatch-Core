//! 接管与还原：先算出一份**可以拿给用户看的**改动，再落盘。
//!
//! 顺序是刻意的：
//!
//! ```text
//! 算改动 → 展示 diff → 用户确认 → 写回校验 → 全文备份 → 原子写 → 读回来对一遍
//! ```
//!
//! 「算改动」和「落盘」分成两个函数，是因为中间必须夹一次人的确认。
//! 一个 `adopt()` 直接把两件事做完的 API，用起来会很顺手 —— 顺手到
//! 没有人会想起要展示 diff。
//!
//! **还原（restore）和回滚（rollback）不是一回事**：还原是
//! 「把我们写的那几个字段改回去」，回滚是「拿全文备份覆盖」。卸载必须
//! 走还原 —— 拿三个月前的备份去覆盖，会把用户这期间加的 MCP server、
//! 调的权限、写的 hook 全部抹掉。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tw_types::{Msg, msg};

use crate::clients::{Client, Edit, Format, Gateway};
use crate::foreign::{self, Applied, Change, ForeignError};
use crate::json::Val;
use crate::sentinel::{self, Original, SidecarRecord, Was};

/// 接管或还原算不出来、落不了盘的原因。
///
/// **英文只写一遍**：`Display` 就是 [`PlanError::msg`] 的原句，界面拿码去翻。
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("{}", self.msg())]
    Read {
        client: String,
        source: ForeignError,
    },
    #[error("{}", self.msg())]
    Parse { client: String, msg: String },
    #[error(transparent)]
    Write(#[from] ForeignError),
    #[error("{}", self.msg())]
    ForeignSidecar {
        path: PathBuf,
        other: String,
        client: String,
    },
    #[error("{}", self.msg())]
    NoRecord { client: String, path: PathBuf },
}

impl PlanError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        match self {
            // **读不到的那个文件本身就是该客户端的配置**，所以直接说文件那一句：
            // 两句套在一起（「X 的配置读不出来：某文件读不出来：…」）只是把同一件
            // 事说了两遍
            PlanError::Read { client, source } => match source {
                ForeignError::Read { path, source } => msg!(
                    "adopt.plan.read_failed", client = client, path = path.display(), detail = source =>
                    "the configuration of {client} could not be read: {path}: {detail}"
                ),
                other => other.msg(),
            },
            PlanError::Parse { client, msg } => msg!(
                "adopt.plan.parse_failed", client = client, detail = msg =>
                "the configuration of {client} could not be parsed, so nothing was changed: {detail}"
            ),
            PlanError::Write(e) => e.msg(),
            PlanError::ForeignSidecar {
                path,
                other,
                client,
            } => msg!(
                "adopt.plan.foreign_record", path = path.display(), other = other, client = client =>
                "the record beside {path} belongs to {other}, not {client}, so nothing was changed"
            ),
            PlanError::NoRecord { client, path } => msg!(
                "adopt.plan.no_record", client = client, path = path.display() =>
                "there is no record for {client}, so there is nothing to restore from. To restore \
                 by hand, look at {path}"
            ),
        }
    }
}

/// 这次改动对某条路径做了什么。写回校验的参照物就是它们叠出来的。
#[derive(Debug, Clone)]
pub enum Target {
    Set(Vec<String>, Val),
    /// **删掉**，不是「写成空串」。还原「原本没有」的字段走这条。
    Remove(Vec<String>),
}

/// 一份算好、还没落盘的改动。**UI 拿它画 diff。**
#[derive(Debug, Clone)]
pub struct Plan {
    pub client: String,
    /// 用户看到的那个路径（可能是符号链接）
    pub path: PathBuf,
    /// `None` = 这个文件本来不存在，我们会新建
    pub before: Option<String>,
    pub after: String,
    pub originals: Vec<Original>,
    /// 这次会不会把密钥写进这个文件
    pub carries_secret: bool,
    /// 接管完成那一屏要说的话：什么时候生效、有什么代价、哪些文件会遮蔽我们
    pub notes: Vec<Msg>,
    /// 优先级比我们高、会盖住这次写入的文件（#6828）
    pub shadows: Vec<PathBuf>,
    /// 这次动了哪些路径。**只有这些路径允许变** —— 写回校验拿它当白名单
    pub targets: Vec<Target>,
    /// 还原完成后要删掉的旁文件
    pub drop_sidecar: Option<PathBuf>,
    /// 还原时要把整个配置文件删掉（当初就是我们建的，而且还原后它是空的）
    pub delete_file: bool,
    /// 之前那次接管留下的记录（备份路径、文件是不是我们建的）。
    ///
    /// **重复接管不能覆盖它。**第二次接管时文件里的值已经是我们写的了，
    /// 照着记一遍，「原值」就变成了我们自己的地址和密钥 —— 之后的还原会
    /// 把用户还原到我们这儿，而不是还原回他原来的样子。这是最隐蔽的一
    /// 种数据丢失：每一步看起来都成功了。
    pub prior: Option<(String, bool)>,
}

impl Plan {
    /// 什么都不用改。**「已经是这样了」和「改完了」要能分开说** ——
    /// 前者不该产生备份，也不该在历史里留一条。
    pub fn is_noop(&self) -> bool {
        self.before.as_deref() == Some(self.after.as_str())
    }
}

fn parse_err(client: &str, e: impl std::fmt::Display) -> PlanError {
    PlanError::Parse {
        client: client.into(),
        msg: e.to_string(),
    }
}

// ---------------------------------------------------------- 三种格式的统一入口

fn semantic(fmt: Format, text: &str, client: &str) -> Result<Val, PlanError> {
    match fmt {
        Format::Json => crate::json::value(text).map_err(|e| parse_err(client, e)),
        Format::Toml => crate::toml::value(text).map_err(|e| parse_err(client, e)),
        Format::Yaml => crate::yamlval::value(text).map_err(|e| parse_err(client, e)),
    }
}

fn put(fmt: Format, text: &str, path: &[&str], v: &Val, client: &str) -> Result<String, PlanError> {
    match fmt {
        Format::Json => crate::json::set(text, path, v).map_err(|e| parse_err(client, e)),
        Format::Toml => crate::toml::set(text, path, v).map_err(|e| parse_err(client, e)),
        Format::Yaml => {
            crate::yaml::set(text, path, &v.to_line()).map_err(|e| parse_err(client, e))
        }
    }
}

fn drop_(fmt: Format, text: &str, path: &[&str], client: &str) -> Result<String, PlanError> {
    match fmt {
        Format::Json => crate::json::remove(text, path).map_err(|e| parse_err(client, e)),
        Format::Toml => crate::toml::remove(text, path).map_err(|e| parse_err(client, e)),
        Format::Yaml => crate::yaml::remove(text, path).map_err(|e| parse_err(client, e)),
    }
}

fn peek(fmt: Format, text: &str, path: &[&str], client: &str) -> Result<Option<String>, PlanError> {
    Ok(match fmt {
        Format::Json => crate::json::get(text, path)
            .map_err(|e| parse_err(client, e))?
            .map(|v| v.to_line()),
        Format::Toml => crate::toml::get(text, path)
            .map_err(|e| parse_err(client, e))?
            .map(|v| v.to_line()),
        Format::Yaml => crate::yaml::get(text, path).map_err(|e| parse_err(client, e))?,
    })
}

/// 空文件长什么样。JSON 得先有个 `{}`，不然连插字段的地方都没有。
fn empty(fmt: Format) -> &'static str {
    match fmt {
        Format::Json => "{}\n",
        Format::Toml | Format::Yaml => "",
    }
}

fn refs(path: &[String]) -> Vec<&str> {
    path.iter().map(|s| s.as_str()).collect()
}

// ---------------------------------------------------------------- 接管

/// 算一份接管改动。**不写任何东西。**
/// 「什么时候生效」那一句。
///
/// 码里带上 `takes_effect` 那个词，因为界面上的两句话措辞完全不同，
/// 不是同一句填不同的空。
fn takes_effect_note(c: &Client) -> Msg {
    msg!(
        "adopt.takes_effect",
        takes_effect = c.takes_effect.slug()
        => "{}", c.takes_effect.note()
    )
}

pub fn plan_adopt(c: &Client, home: &Path, gw: &Gateway) -> Result<Plan, PlanError> {
    let path = c.config_path(home);
    let before = foreign::read(&path).map_err(|source| PlanError::Read {
        client: c.id.into(),
        source,
    })?;
    let base = before
        .clone()
        .unwrap_or_else(|| empty(c.format).to_string());

    // 之前接管过的话，「原值」以那一次的记录为准，不看现在文件里是什么
    // —— 现在文件里的正是我们上次写进去的。
    let real = foreign::resolve(&path)?;
    let side = sentinel::sidecar_path(&real);
    let prior_rec: Option<SidecarRecord> = std::fs::read_to_string(&side)
        .ok()
        .and_then(|t| serde_json::from_str::<SidecarRecord>(&t).ok())
        .filter(|r| r.client == c.id);
    let prior_originals = match &prior_rec {
        Some(r) => Some(originals_from(r, c.format, c.id)?),
        None => None,
    };

    let edits = crate::clients::edits(c, gw);
    let mut text = base.clone();
    let mut originals = Vec::new();
    let mut carries_secret = false;

    let mut targets = Vec::new();
    for Edit {
        path: p,
        value,
        secret,
    } in &edits
    {
        let r = refs(p);
        let was = match prior_originals
            .as_ref()
            .and_then(|os| os.iter().find(|o| o.path == *p))
        {
            Some(o) => o.was.clone(),
            None => match peek(c.format, &text, &r, c.id)? {
                None => Was::Missing,
                // 原值是密钥的话，它只进全文备份，不进旁文件
                Some(v) if *secret => Was::Secret(v),
                Some(v) => Was::Value(v),
            },
        };
        originals.push(Original::new(p, was));
        carries_secret |= secret;
        text = put(c.format, &text, &r, value, c.id)?;
        targets.push(Target::Set(p.clone(), value.clone()));
    }

    // 哨兵注释放在最前面 —— 要的是**用户打开文件就看见**。
    // 严格 JSON 装不下注释，那时只有旁文件。
    if let Some(prefix) = c.comment_prefix() {
        let block = sentinel::comment_block(prefix, &originals);
        // 重复接管不该叠一堆哨兵
        text = format!("{block}{}", sentinel::strip(&text, prefix));
    }

    let mut notes = vec![takes_effect_note(c)];
    notes.extend(c.costs.iter().map(|(code, text)| Msg {
        code: (*code).into(),
        args: BTreeMap::new(),
        text: (*text).into(),
    }));
    if c.verified == crate::clients::Verified::FieldsOnly {
        notes.push(msg!(
            "adopt.plan.fields_only"
            => "{} Do not take it as working until the first request arrives.",
            c.verified.note()
        ));
    }

    let shadows: Vec<_> = c
        .shadow_paths(home)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if !shadows.is_empty() {
        notes.push(msg!(
            "adopt.plan.shadowed",
            paths = shadows
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
            => "{paths} was found, and it takes precedence over what was written here, so a setting of the same name there wins."
        ));
    }

    Ok(Plan {
        client: c.id.into(),
        path,
        before,
        after: text,
        originals,
        carries_secret,
        notes,
        shadows,
        targets,
        drop_sidecar: None,
        delete_file: false,
        prior: prior_rec.map(|r| (r.backup, r.created_file)),
    })
}

/// 从一份接管记录里还原出「原值」。密钥类的去全文备份里取。
fn originals_from(
    rec: &SidecarRecord,
    fmt: Format,
    client: &str,
) -> Result<Vec<Original>, PlanError> {
    let backed = std::fs::read_to_string(&rec.backup).ok();
    let backed_val = match &backed {
        Some(t) if t.trim().is_empty() => Some(Val::Obj(Vec::new())),
        Some(t) => Some(semantic(fmt, t, client)?),
        None => None,
    };
    Ok(rec
        .originals
        .iter()
        .map(|f| {
            let path = f.path.clone();
            let was = match f.was.as_str() {
                "missing" => Was::Missing,
                "secret" => match backed_val.as_ref().and_then(|v| lookup(v, &refs(&path))) {
                    Some(v) => Was::Secret(v.to_line()),
                    // 备份没了就拿不回来。**记成「原本没有」是错的** ——
                    // 那会让还原把一个本来有值的字段删掉，还一声不吭。
                    None => Was::Secret(String::new()),
                },
                _ => match &f.value {
                    Some(v) => Was::Value(v.clone()),
                    None => Was::Missing,
                },
            };
            Original {
                field: f.field.clone(),
                path,
                was,
            }
        })
        .collect())
}

/// 落盘。**用户确认之后才该调到这里。**
pub fn apply(c: &Client, plan: &Plan, backup_root: &Path) -> Result<Applied, PlanError> {
    let fmt = c.format;
    let client = plan.client.clone();
    let expect = {
        let base = plan
            .before
            .clone()
            .unwrap_or_else(|| empty(fmt).to_string());
        let mut v = semantic(fmt, &base, &client)?;
        for t in &plan.targets {
            v = match t {
                Target::Set(p, val) => v.with(&refs(p), val),
                Target::Remove(p) => v.without(&refs(p)),
            };
        }
        v
    };

    let applied = foreign::apply(
        &Change {
            path: &plan.path,
            before: plan.before.as_deref(),
            after: &plan.after,
            carries_secret: plan.carries_secret,
        },
        backup_root,
        |text| {
            // **写回校验**：重新解析，和「原文件 + 预期的那几处改动」比。
            // 对不上就拒绝落盘、原文件一个字节不动。
            let got = semantic(fmt, text, &client).map_err(|e| e.to_string())?;
            if got.normalized() == expect.normalized() {
                Ok(())
            } else {
                Err("the edited content is not the original plus the intended change".into())
            }
        },
    )?;

    // 旁文件在最后写，因为它要记下备份路径。**写不成就把配置也退回去**
    // —— 一次接管要么完整，要么等于没发生；只改了配置却没留下还原记录，
    // 是这里面最坏的一种半成品。
    let side = sentinel::sidecar_path(&applied.real);
    // 重复接管时，**指向第一次那份备份**。这一次的备份里装的是已经被我们
    // 改过的文件，拿它去还原等于还原到我们自己身上。
    let (backup, created) = plan
        .prior
        .clone()
        .unwrap_or_else(|| (applied.backup.display().to_string(), applied.created));
    let rec = SidecarRecord::new(&plan.client, now_ms(), &backup, created, &plan.originals);
    if let Err(e) = write_sidecar(&side, &rec) {
        let _ = rollback(&applied);
        return Err(PlanError::Write(e));
    }
    Ok(applied)
}

/// 落盘一次还原。和 [`apply`] 走同一套护栏，只是最后**删掉**旁文件而不是
/// 写它 —— 还原之后不该再留下「这个文件被接管着」的痕迹。
pub fn apply_restore(c: &Client, plan: &Plan, backup_root: &Path) -> Result<Applied, PlanError> {
    let fmt = c.format;
    if plan.delete_file {
        // 删之前照样先备份。**「删掉一个文件」是这里面最不可逆的动作**，
        // 它更需要那份备份，不是更不需要。
        let applied = foreign::apply(
            &Change {
                path: &plan.path,
                before: plan.before.as_deref(),
                after: plan.before.as_deref().unwrap_or(""),
                carries_secret: false,
            },
            backup_root,
            |_| Ok(()),
        )?;
        std::fs::remove_file(&applied.real).map_err(|source| ForeignError::Write {
            path: applied.real.clone(),
            source,
        })?;
        if let Some(side) = &plan.drop_sidecar {
            let _ = std::fs::remove_file(side);
        }
        return Ok(applied);
    }
    let client = plan.client.clone();
    let expect = {
        let base = plan
            .before
            .clone()
            .unwrap_or_else(|| empty(fmt).to_string());
        let mut v = semantic(fmt, &base, &client)?;
        for t in &plan.targets {
            v = match t {
                Target::Set(p, val) => v.with(&refs(p), val),
                Target::Remove(p) => v.without(&refs(p)),
            };
        }
        v
    };
    let applied = foreign::apply(
        &Change {
            path: &plan.path,
            before: plan.before.as_deref(),
            after: &plan.after,
            carries_secret: false,
        },
        backup_root,
        |text| {
            let got = semantic(fmt, text, &client).map_err(|e| e.to_string())?;
            if got.normalized() == expect.normalized() {
                Ok(())
            } else {
                Err(
                    "the restored content is not the current file minus the fields written here"
                        .into(),
                )
            }
        },
    )?;
    if let Some(side) = &plan.drop_sidecar {
        let _ = std::fs::remove_file(side);
    }
    Ok(applied)
}

/// 算一份还原改动。**不写任何东西。**
///
/// 分工是刻意的：**旁文件说「我们动过哪几个字段」，全文备份说「它们原来
/// 是什么」**。所以密钥类的原值一份都不用抄进旁文件，也不会因此丢失。
pub fn plan_restore(c: &Client, home: &Path) -> Result<Plan, PlanError> {
    let path = c.config_path(home);
    let real = foreign::resolve(&path)?;
    let before = foreign::read(&path).map_err(|source| PlanError::Read {
        client: c.id.into(),
        source,
    })?;
    let side = sentinel::sidecar_path(&real);
    let rec: SidecarRecord = match std::fs::read_to_string(&side) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| parse_err(c.id, e))?,
        Err(_) => {
            return Err(PlanError::NoRecord {
                client: c.id.into(),
                path: side,
            });
        }
    };
    if rec.client != c.id {
        return Err(PlanError::ForeignSidecar {
            path: side,
            other: rec.client,
            client: c.id.into(),
        });
    }

    let Some(mut text) = before.clone() else {
        // 文件都没了，没什么可还原的 —— 把记录删掉就行
        return Ok(Plan {
            client: c.id.into(),
            path,
            before,
            after: String::new(),
            originals: Vec::new(),
            carries_secret: false,
            notes: vec![
                msg!("adopt.restore.config_gone" => "The configuration file is gone; only the record was removed."),
            ],
            shadows: Vec::new(),
            targets: Vec::new(),
            drop_sidecar: Some(side),
            delete_file: false,
            prior: None,
        });
    };

    // 全文备份是原值的来源。它还在的话，连密钥都能原样放回去。
    let backup = PathBuf::from(&rec.backup);
    let backed = std::fs::read_to_string(&backup).ok();
    let backed_val = match &backed {
        // 空备份 = 接管前那个文件根本不存在。**这和「备份丢了」不是一
        // 回事**：前者知道原来什么都没有，后者是不知道原来是什么。
        Some(t) if t.trim().is_empty() => Some(Val::Obj(Vec::new())),
        Some(t) => Some(semantic(c.format, t, c.id)?),
        None => None,
    };

    let mut notes = Vec::new();
    let mut targets = Vec::new();
    for f in &rec.originals {
        let p = f.path.clone();
        let r = refs(&p);
        let from_backup = backed_val.as_ref().and_then(|v| lookup(v, &r));
        match (f.was.as_str(), from_backup, &f.value) {
            ("missing", _, _) => targets.push(Target::Remove(p.clone())),
            (_, Some(v), _) => targets.push(Target::Set(p.clone(), v)),
            (_, None, Some(v)) => targets.push(Target::Set(p.clone(), Val::s(v))),
            ("secret", None, None) => {
                // **说出来，别假装还原成功了。**原来那儿是用户自己的
                // 密钥，备份没了我们就是拿不回来；留着我们的密钥比删掉
                // 更糟 —— 那等于卸载之后还在替他发着请求。
                targets.push(Target::Remove(p.clone()));
                notes.push(msg!(
                    "adopt.restore.secret_lost",
                    field = f.field.clone(),
                    backup = backup.display()
                    => "The original value of {field} is a secret kept only in the full backup, and {backup} is gone. The field was removed and has to be filled in again by hand."
                ));
            }
            (_, None, None) => targets.push(Target::Remove(p.clone())),
        }
    }

    // 我们凭空造出来的容器（比如原本没有的 `env`）要跟着收走，
    // 否则「还原」之后会留下一个用户从来没有过的空段落。
    if let Some(bv) = &backed_val {
        for f in &rec.originals {
            let p = f.path.clone();
            for cut in (1..p.len()).rev() {
                let anc = &p[..cut];
                if lookup(bv, &refs(anc)).is_none()
                    && !targets
                        .iter()
                        .any(|t| matches!(t, Target::Remove(x) if x == anc))
                {
                    targets.push(Target::Remove(anc.to_vec()));
                }
            }
        }
    }

    for t in &targets {
        text = match t {
            Target::Set(p, v) => put(c.format, &text, &refs(p), v, c.id)?,
            Target::Remove(p) => drop_(c.format, &text, &refs(p), c.id)?,
        };
    }
    if let Some(prefix) = c.comment_prefix() {
        text = sentinel::strip(&text, prefix);
    }

    // 当初这个文件就是我们建的，还原之后又空了 —— 那就整个删掉。
    // **只在空的时候删**：用户可能在这三个月里往里加了自己的东西，
    // 那些必须留下（卸载走还原，不是拿备份覆盖）。
    let delete_file = rec.created_file
        && matches!(semantic(c.format, &text, c.id)?, Val::Obj(ms) if ms.is_empty());
    if delete_file {
        notes.push(msg!("adopt.restore.file_removed" => "This file was created here, restoring leaves it empty, and it was removed with the rest."));
    }

    notes.insert(0, takes_effect_note(c));
    Ok(Plan {
        client: c.id.into(),
        path,
        before,
        after: text,
        originals: Vec::new(),
        carries_secret: false,
        notes,
        shadows: Vec::new(),
        targets,
        drop_sidecar: Some(side),
        delete_file,
        prior: None,
    })
}

fn lookup(v: &Val, path: &[&str]) -> Option<Val> {
    let mut cur = v;
    for k in path {
        let Val::Obj(ms) = cur else { return None };
        cur = &ms.iter().find(|(mk, _)| mk == k)?.1;
    }
    Some(cur.clone())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn write_sidecar(path: &Path, rec: &SidecarRecord) -> Result<(), ForeignError> {
    let text = serde_json::to_string_pretty(rec).unwrap_or_default();
    crate::foreign::write_private(path, text.as_bytes()).map_err(|source| ForeignError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn rollback(a: &Applied) -> std::io::Result<()> {
    if a.created {
        std::fs::remove_file(&a.real)
    } else {
        let text = std::fs::read_to_string(&a.backup)?;
        std::fs::write(&a.real, text)
    }
}

#[cfg(test)]
mod msg_codes {
    use super::*;
    use crate::mcp::McpError;

    #[test]
    fn every_adopt_error_has_its_own_code() {
        let p = || PathBuf::from("/h/.claude/settings.json");
        let io = || std::io::Error::other("denied");
        let foreign = || {
            vec![
                ForeignError::Read {
                    path: p(),
                    source: io(),
                },
                ForeignError::Write {
                    path: p(),
                    source: io(),
                },
                ForeignError::ChangedUnderUs { path: p() },
                ForeignError::VerifyFailed("x".into()),
                ForeignError::Readback { path: p() },
                ForeignError::LinkLoop { path: p() },
            ]
        };
        let mut all: Vec<(Msg, String)> = foreign()
            .into_iter()
            .map(|e| (e.msg(), e.to_string()))
            .collect();
        for e in [
            PlanError::Read {
                client: "claude-code".into(),
                source: ForeignError::Read {
                    path: p(),
                    source: io(),
                },
            },
            PlanError::Parse {
                client: "claude-code".into(),
                msg: "x".into(),
            },
            PlanError::ForeignSidecar {
                path: p(),
                other: "codex".into(),
                client: "claude-code".into(),
            },
            PlanError::NoRecord {
                client: "claude-code".into(),
                path: p(),
            },
        ] {
            all.push((e.msg(), e.to_string()));
        }
        for e in [
            McpError::UnknownClient("x".into()),
            McpError::Parse {
                client: "zed".into(),
                msg: "x".into(),
            },
            McpError::NotThere {
                client: "zed".into(),
                name: "fs".into(),
            },
        ] {
            all.push((e.msg(), e.to_string()));
        }
        let mut seen = std::collections::HashSet::new();
        for (m, display) in &all {
            assert!(m.code.starts_with("adopt."), "{m:?}");
            assert!(!m.text.is_empty() && &m.text == display, "{m:?}");
            assert!(seen.insert(m.code.clone()), "码重复了：{}", m.code);
        }
        // 包着的那几层用里面那一句的码
        let w = PlanError::Write(ForeignError::LinkLoop { path: p() });
        assert_eq!(w.msg().code, "adopt.file.link_loop");
        let r = PlanError::Read {
            client: "x".into(),
            source: ForeignError::LinkLoop { path: p() },
        };
        assert_eq!(r.msg().code, "adopt.file.link_loop");
        // 不能写的理由本来就有码，直接说它
        let e = crate::mcp::target("zed").unwrap().why_not().unwrap();
        let m = McpError::NotCopyable {
            client: "zed".into(),
            why: e.clone(),
        }
        .msg();
        assert_eq!(m, e);
    }
}
