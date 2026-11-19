//! Live routing revisions and account provenance for upstream-bound history.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tw_config::{Config, Provider};
use tw_dialect::ir::Dialect;
use tw_dialect::sealed::Sealed;
use tw_engine::{Engine, Group, GroupType, Target};
use tw_types::msg;

use crate::error::GatewayError;
use crate::state::{AppState, Runtime};

type Fingerprint = [u8; 32];

fn hash(value: impl AsRef<[u8]>) -> Fingerprint {
    *blake3::hash(value.as_ref()).as_bytes()
}

#[derive(Clone)]
pub(crate) struct ClientRoute {
    pub key: Fingerprint,
    shape: Fingerprint,
    pub revision: u64,
}

#[derive(Clone)]
pub(crate) struct Routing {
    revision: u64,
    clients: HashMap<String, ClientRoute>,
    accounts: HashMap<String, AccountIdentity>,
}

#[derive(Clone, Copy)]
struct AccountIdentity {
    configured: Fingerprint,
    without_refresh: Option<Fingerprint>,
    verified: Fingerprint,
}

impl Routing {
    pub fn build(
        config: &Config,
        engine: &Engine,
        previous: Option<&Runtime>,
        oauth: Option<&crate::oauth::Cache>,
    ) -> Self {
        let revision = previous.map_or(0, |rt| rt.routing.revision + 1);
        let mut known = HashMap::new();
        if let Some(rt) = previous {
            for old in &rt.config.providers {
                if let Some(identity) = rt.routing.accounts.get(&old.name) {
                    known.insert(identity.configured, identity.verified);
                }
            }
            if let Some(oauth) = oauth {
                for provider in config.providers.iter().filter(|p| p.oauth.is_some()) {
                    let Some(old) = rt.config.providers.iter().find(|p| p.name == provider.name)
                    else {
                        continue;
                    };
                    if let (Some(was), Some(now)) = (&old.oauth, &provider.oauth)
                        && was.refresh != now.refresh
                        && let Some(identity) = rt.routing.accounts.get(&old.name)
                        && identity.without_refresh == Some(account(provider, false))
                        && oauth.wrote_rotation(&provider.name, was, now)
                    {
                        known.insert(account(provider, true), identity.verified);
                    }
                }
            }
        }
        let accounts: HashMap<_, _> = config
            .providers
            .iter()
            .map(|p| {
                let configured = account(p, true);
                let without_refresh = p.oauth.as_ref().map(|_| account(p, false));
                let verified = known.get(&configured).copied().unwrap_or(configured);
                (
                    p.name.clone(),
                    AccountIdentity {
                        configured,
                        without_refresh,
                        verified,
                    },
                )
            })
            .collect();
        let clients = config
            .clients
            .iter()
            .filter(|client| !client.disabled)
            .map(|client| {
                let key = hash(&client.key);
                let route = engine.route_of(&client.name);
                let rules = engine.rules_for_client(&client.name);
                let mut names = HashSet::new();
                let mut groups = Vec::new();
                for target in rules.iter().filter_map(|rule| rule.to.as_ref()) {
                    match target {
                        Target::Name(name) => {
                            if let Some(group) = engine.groups().iter().find(|g| g.name == *name) {
                                groups.push(group);
                                if group.kind == GroupType::Select {
                                    names.extend(group.selection());
                                } else {
                                    names.extend(group.providers.iter().map(|m| m.name.as_str()));
                                }
                            } else {
                                names.insert(name.as_str());
                            }
                        }
                        Target::Models(models) => {
                            names.extend(models.iter().map(|p| p.provider.as_str()));
                        }
                    }
                }
                let providers: Vec<_> = config
                    .providers
                    .iter()
                    .filter(|p| names.contains(p.name.as_str()))
                    .map(|p| {
                        (
                            &p.name,
                            accounts[&p.name].verified,
                            p.disabled,
                            &p.models,
                            &p.models_only,
                        )
                    })
                    .collect();
                let shape = hash(
                    serde_json::to_vec(&(
                        route,
                        rules,
                        groups,
                        providers,
                        &config.aliases,
                        &client.allow,
                        config.web_tools.mode,
                    ))
                    .expect("routing configuration serializes"),
                );
                let revision = previous
                    .and_then(|rt| rt.routing.client(&client.name))
                    .filter(|old| old.key == key && old.shape == shape)
                    .map_or(revision, |old| old.revision);
                (
                    client.name.clone(),
                    ClientRoute {
                        key,
                        shape,
                        revision,
                    },
                )
            })
            .collect();
        Self {
            revision,
            clients,
            accounts,
        }
    }

    pub fn client(&self, name: &str) -> Option<&ClientRoute> {
        self.clients.get(name)
    }

    pub fn account(&self, provider: &str) -> Option<Fingerprint> {
        self.accounts
            .get(provider)
            .map(|identity| identity.verified)
    }
}

impl ClientRoute {
    pub fn matches(&self, rt: &Runtime, client: &str) -> bool {
        rt.routing
            .client(client)
            .is_some_and(|now| now.key == self.key && now.revision == self.revision)
    }

    pub fn current(&self, state: &AppState, client: &str) -> bool {
        self.matches(&state.runtime(), client)
    }
}

pub(crate) fn manual(engine: &Engine, group: Option<&str>) -> bool {
    group
        .and_then(|name| engine.groups().iter().find(|g| g.name == name))
        .is_some_and(|g: &Group| g.kind == GroupType::Select)
}

fn account(provider: &Provider, include_refresh: bool) -> Fingerprint {
    let token = provider
        .key
        .as_ref()
        .map(|key| key.resolve().unwrap_or_else(|_| key.raw().into()));
    let mut headers: Vec<_> = provider
        .headers
        .iter()
        .map(|header| {
            (
                header.name.trim().to_ascii_lowercase(),
                header
                    .value
                    .resolve()
                    .unwrap_or_else(|_| header.value.raw().into()),
            )
        })
        .collect();
    headers.sort();
    let oauth = provider.oauth.as_ref().map(|oauth| {
        // ChatGPT config has a stable account identifier even as its tokens rotate.
        let has_account = headers
            .iter()
            .any(|(name, value)| name == "chatgpt-account-id" && !value.is_empty());
        (
            &oauth.endpoint,
            &oauth.client_id,
            (include_refresh && !has_account).then_some(&oauth.refresh),
        )
    });
    hash(
        serde_json::to_vec(&(
            provider.base_url.trim_end_matches('/'),
            provider.effective_protocol(),
            token,
            headers,
            oauth,
            &provider.aws,
        ))
        .expect("account configuration serializes"),
    )
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Reasoning,
    Content,
    Stored,
}

impl Kind {
    pub fn conflict(self) -> GatewayError {
        GatewayError::conflict(match self {
            Self::Reasoning => msg!("gw.route.history.reasoning" =>
                "The selected upstream account cannot be verified to read this conversation's encrypted reasoning. Start a new conversation on the selected route."),
            Self::Content => msg!("gw.route.history.content" =>
                "The selected upstream account cannot be verified to read this conversation's encrypted history, such as compacted context or tool results. Start a new conversation on the selected route."),
            Self::Stored => msg!("gw.route.history.stored" =>
                "This request references history stored by an upstream account. It cannot be verified for the selected account. Start a new conversation on the selected route."),
        })
    }
}

pub(crate) struct Required {
    pub kind: Kind,
    artifacts: Vec<Fingerprint>,
    unknown: bool,
}

fn artifact(namespace: &str, text: &str) -> Fingerprint {
    let mut h = blake3::Hasher::new();
    h.update(namespace.as_bytes())
        .update(b"\0")
        .update(text.as_bytes());
    *h.finalize().as_bytes()
}

impl Required {
    pub fn read(dialect: Dialect, body: &Value) -> Option<Self> {
        if dialect == Dialect::Anthropic {
            let mut artifacts = Vec::new();
            for message in body["messages"].as_array().into_iter().flatten() {
                for block in message["content"].as_array().into_iter().flatten() {
                    anthropic_block(block, &mut artifacts);
                }
            }
            return (!artifacts.is_empty()).then_some(Self {
                kind: Kind::Reasoning,
                artifacts,
                unknown: false,
            });
        }
        if dialect != Dialect::Responses {
            return None;
        }
        let mut artifacts = Vec::new();
        let mut unknown = false;
        let mut stored = false;
        for name in ["previous_response_id", "conversation", "prompt"] {
            let v = &body[name];
            if v.is_null() || v.as_str() == Some("") {
                continue;
            }
            stored = true;
            if let Some(id) = v.as_str().or_else(|| v["id"].as_str())
                && name != "prompt"
            {
                let namespace = if name == "previous_response_id" {
                    "response"
                } else {
                    name
                };
                artifacts.push(artifact(namespace, id));
            } else {
                unknown = true;
            }
        }
        if let Some(input) = body["input"].as_array() {
            for item in input {
                if item["type"] == "item_reference" {
                    stored = true;
                    if let Some(id) = item["id"].as_str() {
                        artifacts.push(artifact("item", id));
                    } else {
                        unknown = true;
                    }
                }
                ciphertext(item, &mut artifacts);
            }
        }
        let kind = match (stored, tw_dialect::sealed::sealed(body)) {
            (true, _) => Kind::Stored,
            (false, Some(Sealed::Content)) => Kind::Content,
            (false, Some(Sealed::Reasoning)) => Kind::Reasoning,
            _ => return None,
        };
        Some(Self {
            kind,
            artifacts,
            unknown,
        })
    }
}

fn anthropic_block(block: &Value, out: &mut Vec<Fingerprint>) {
    let (field, namespace) = match block["type"].as_str() {
        Some("thinking") => ("signature", "anthropic/sig"),
        Some("redacted_thinking") => ("data", "anthropic_redacted"),
        _ => return,
    };
    if let Some(value) = block[field].as_str()
        && !value.is_empty()
        && !value.starts_with(tw_dialect::ir::CARRIED)
    {
        out.push(artifact(namespace, value));
    }
}

fn ciphertext(value: &Value, out: &mut Vec<Fingerprint>) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                if name == "encrypted_content"
                    && let Some(s) = value.as_str()
                    && !s.is_empty()
                    && !s.starts_with(tw_dialect::ir::CARRIED)
                {
                    out.push(artifact("ciphertext", s));
                }
                ciphertext(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                ciphertext(value, out);
            }
        }
        _ => {}
    }
}

const CAP: usize = 65_536;
const SNAPSHOT_LIMIT: usize = 4096;
const KEEP_MS: u64 = crate::affinity::SEALED_KEEP_MS;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Origin {
    account: Option<Fingerprint>,
    at: u64,
}

#[derive(Default)]
pub(crate) struct History {
    entries: Mutex<HashMap<(Fingerprint, Fingerprint), Origin>>,
    inherited_cuts: Mutex<HashSet<Fingerprint>>,
    disk: Mutex<Option<(PathBuf, RouteSnapshot)>>,
    save_queued: AtomicBool,
}

type RouteSnapshot = Vec<(Fingerprint, Fingerprint, bool)>;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    routes: RouteSnapshot,
    origins: Vec<(Fingerprint, Fingerprint, Origin)>,
}

impl History {
    pub fn strict(&self, route: &ClientRoute) -> bool {
        route.revision != 0
            || self
                .inherited_cuts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&route.key)
    }

    fn routes(&self, routing: &Routing) -> RouteSnapshot {
        let mut routes: RouteSnapshot = routing
            .clients
            .values()
            .map(|route| (route.key, route.shape, self.strict(route)))
            .collect();
        routes.sort_unstable_by_key(|(key, _, _)| *key);
        routes
    }

    pub fn export(&self, routing: &Routing) -> Snapshot {
        self.snapshot(self.routes(routing))
    }

    fn snapshot(&self, routes: RouteSnapshot) -> Snapshot {
        let now = crate::server::now_ms();
        let mut origins: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, origin)| now.saturating_sub(origin.at) < KEEP_MS)
            .map(|((key, id), origin)| (*key, *id, *origin))
            .collect();
        origins.sort_unstable_by_key(|(_, _, origin)| std::cmp::Reverse(origin.at));
        origins.truncate(SNAPSHOT_LIMIT);
        Snapshot { routes, origins }
    }

    /// Keep enough proof to enforce a cut across a restart. The file contains
    /// fingerprints only and is written atomically when routes change.
    pub fn set_dir(&self, dir: &Path, routing: &Routing) {
        let path = dir.join("route-history.json");
        let saved: Option<Snapshot> = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= 4 * 1024 * 1024)
            .and_then(|_| std::fs::read(&path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if let Some(snapshot) = saved {
            self.import(routing, snapshot);
        }
        *self.disk.lock().unwrap_or_else(|p| p.into_inner()) = Some((path, Vec::new()));
        self.sync_routing(routing);
    }

    pub fn sync_routing(&self, routing: &Routing) {
        let routes = self.routes(routing);
        // The safe-mode fallback has no authenticated clients.
        if routes.is_empty() {
            return;
        }
        let mut disk = self.disk.lock().unwrap_or_else(|p| p.into_inner());
        let Some((path, saved)) = disk.as_mut() else {
            return;
        };
        if *saved == routes {
            return;
        }
        if self.write_snapshot(path, &routes) {
            *saved = routes;
        }
    }

    fn write_snapshot(&self, path: &Path, routes: &RouteSnapshot) -> bool {
        let Ok(json) = serde_json::to_string(&self.snapshot(routes.clone())) else {
            return false;
        };
        match tw_config::store::write_atomic(path, &json) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!("could not save routing history fingerprints: {error}");
                false
            }
        }
    }

    // Output can arrive every few milliseconds. Group the writes and keep all
    // file I/O off the response path.
    fn queue_save(self: &Arc<Self>) {
        if self.save_queued.load(Ordering::Acquire)
            || self
                .disk
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none()
            || self.save_queued.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let history = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let _ = tokio::task::spawn_blocking(move || {
                history.save_queued.store(false, Ordering::Release);
                let disk = history.disk.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((path, routes)) = disk.as_ref().filter(|(_, routes)| !routes.is_empty())
                {
                    history.write_snapshot(path, routes);
                }
            })
            .await;
        });
    }

    pub fn import(&self, routing: &Routing, snapshot: Snapshot) {
        let shapes: HashMap<_, _> = routing
            .clients
            .values()
            .map(|route| (route.key, route.shape))
            .collect();
        self.inherited_cuts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(
                snapshot
                    .routes
                    .into_iter()
                    .filter_map(|(key, shape, strict)| {
                        shapes
                            .get(&key)
                            .is_some_and(|current| strict || *current != shape)
                            .then_some(key)
                    }),
            );
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        for (key, id, imported) in snapshot.origins.into_iter().take(SNAPSHOT_LIMIT) {
            if entries.len() >= CAP {
                break;
            }
            entries
                .entry((key, id))
                .and_modify(|origin| {
                    if origin.account != imported.account {
                        origin.account = None;
                    }
                    origin.at = origin.at.max(imported.at);
                })
                .or_insert(imported);
        }
    }

    pub fn check(
        &self,
        required: &Required,
        key: Fingerprint,
        account: Fingerprint,
        strict: bool,
    ) -> Result<(), GatewayError> {
        let now = crate::server::now_ms();
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let readable = (!strict || !required.unknown)
            && required.artifacts.iter().all(|id| {
                entries
                    .get(&(key, *id))
                    .filter(|origin| now.saturating_sub(origin.at) < KEEP_MS)
                    .map_or(!strict, |origin| origin.account == Some(account))
            });
        if readable {
            Ok(())
        } else {
            Err(required.kind.conflict())
        }
    }

    pub fn record(self: &Arc<Self>, key: Fingerprint, account: Fingerprint, value: &Value) {
        let mut artifacts = Vec::new();
        match value["type"].as_str() {
            Some("response.output_item.done") => output_item(&value["item"], &mut artifacts),
            Some("response.created" | "response.completed" | "response.incomplete") => {
                output_response(&value["response"], &mut artifacts);
            }
            _ if value["object"] == "response" || value.get("output").is_some() => {
                output_response(value, &mut artifacts);
            }
            _ => {}
        }
        self.record_artifacts(key, account, artifacts);
    }

    fn record_artifacts(
        self: &Arc<Self>,
        key: Fingerprint,
        account: Fingerprint,
        artifacts: Vec<Fingerprint>,
    ) {
        if artifacts.is_empty() {
            return;
        }
        let now = crate::server::now_ms();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() + artifacts.len() > CAP {
            entries.retain(|_, origin| now.saturating_sub(origin.at) < KEEP_MS);
            if entries.len() + artifacts.len() > CAP {
                let mut newest: Vec<_> = entries.drain().collect();
                newest.sort_unstable_by_key(|(_, origin)| std::cmp::Reverse(origin.at));
                entries.extend(newest.into_iter().take(CAP / 2));
            }
        }
        use std::collections::hash_map::Entry;
        let mut changed = false;
        for id in artifacts {
            if entries.len() >= CAP && !entries.contains_key(&(key, id)) {
                break;
            }
            match entries.entry((key, id)) {
                Entry::Occupied(mut entry) => {
                    let origin = entry.get_mut();
                    if origin.account.is_some_and(|old| old != account) {
                        origin.account = None;
                        changed = true;
                    }
                    origin.at = now;
                }
                Entry::Vacant(entry) => {
                    entry.insert(Origin {
                        account: Some(account),
                        at: now,
                    });
                    changed = true;
                }
            }
        }
        drop(entries);
        if changed {
            self.queue_save();
        }
    }
}

fn output_response(response: &Value, out: &mut Vec<Fingerprint>) {
    if let Some(id) = response["id"].as_str() {
        out.push(artifact("response", id));
    }
    // A response's own ID and output were issued here. Any echoed conversation
    // reference is already an input dependency and is left out of this record.
    if let Some(items) = response["output"].as_array() {
        for item in items {
            output_item(item, out);
        }
    }
}

fn output_item(item: &Value, out: &mut Vec<Fingerprint>) {
    if let Some(id) = item["id"].as_str() {
        out.push(artifact("item", id));
    }
    ciphertext(item, out);
}

/// Observe Responses output without changing what is sent to the client.
pub(crate) struct Capture {
    history: std::sync::Arc<History>,
    key: Fingerprint,
    account: Fingerprint,
    dialect: Dialect,
    sse: bool,
    signatures: HashMap<u64, String>,
    pending: Vec<u8>,
    overflow: bool,
}

impl Capture {
    pub fn new(
        state: &AppState,
        rt: &Runtime,
        key: &str,
        provider: &str,
        dialect: Dialect,
        sse: bool,
    ) -> Option<Self> {
        Some(Self {
            history: state.route_history.clone(),
            key: rt.routing.client(key)?.key,
            account: rt.routing.account(provider)?,
            dialect,
            sse,
            signatures: HashMap::new(),
            pending: Vec::new(),
            overflow: false,
        })
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        const MAX_CAPTURE: usize = 4 * 1024 * 1024;
        if self.overflow {
            return;
        }
        if self.pending.len().saturating_add(chunk.len()) > MAX_CAPTURE {
            self.pending.clear();
            self.overflow = true;
            return;
        }
        self.pending.extend_from_slice(chunk);
        if self.sse {
            let mut read = 0;
            while let Some((end, sep)) = tw_dialect::frame::frame_end(&self.pending[read..]) {
                if let Some(frame) = tw_dialect::frame::parse(&self.pending[read..read + end])
                    && let Ok(value) = serde_json::from_str::<Value>(&frame.data)
                {
                    self.record(value);
                }
                read += end + sep;
            }
            self.pending.drain(..read);
        } else if let Ok(value) = serde_json::from_slice::<Value>(&self.pending) {
            self.record(value);
            self.pending.clear();
        }
    }

    pub fn finish(&mut self) {
        if self.sse
            && let Some(frame) = tw_dialect::frame::parse(&self.pending)
            && let Ok(value) = serde_json::from_str::<Value>(&frame.data)
        {
            self.record(value);
        }
    }

    fn record(&mut self, value: Value) {
        if self.dialect == Dialect::Responses {
            self.history.record(self.key, self.account, &value);
            return;
        }
        let mut artifacts = Vec::new();
        match value["type"].as_str() {
            Some("content_block_start") => anthropic_block(&value["content_block"], &mut artifacts),
            Some("content_block_delta") if value["delta"]["type"] == "signature_delta" => {
                if let (Some(index), Some(delta)) = (
                    value["index"].as_u64(),
                    value["delta"]["signature"].as_str(),
                ) && (self.signatures.contains_key(&index) || self.signatures.len() < 32)
                {
                    let signature = self.signatures.entry(index).or_default();
                    if signature.len() + delta.len() <= 64 * 1024 {
                        signature.push_str(delta);
                    }
                }
            }
            Some("content_block_stop") => {
                if let Some(signature) = value["index"]
                    .as_u64()
                    .and_then(|i| self.signatures.remove(&i))
                {
                    anthropic_block(
                        &serde_json::json!({"type":"thinking","signature":signature}),
                        &mut artifacts,
                    );
                }
            }
            Some("message") => {
                for block in value["content"].as_array().into_iter().flatten() {
                    anthropic_block(block, &mut artifacts);
                }
            }
            _ => {}
        }
        self.history
            .record_artifacts(self.key, self.account, artifacts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signed_anthropic_history_matches_only_the_issuing_account() {
        let key = hash("key");
        let owner = hash("account-a");
        let history = Arc::new(History::default());
        let mut capture = Capture {
            history: history.clone(),
            key,
            account: owner,
            dialect: Dialect::Anthropic,
            sse: true,
            signatures: HashMap::new(),
            pending: Vec::new(),
            overflow: false,
        };
        for value in [
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"part1"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"part2"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"sealed"}}),
        ] {
            let frame = format!("data: {value}\n\n");
            for chunk in frame.as_bytes().chunks(7) {
                capture.feed(chunk);
            }
        }
        let body = json!({"messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"...","signature":"part1part2"},
            {"type":"redacted_thinking","data":"sealed"}
        ]}]});
        let required = Required::read(Dialect::Anthropic, &body).unwrap();
        assert!(history.check(&required, key, owner, true).is_ok());
        assert_eq!(
            history
                .check(&required, key, hash("account-b"), true)
                .unwrap_err()
                .detail
                .code,
            "gw.route.history.reasoning"
        );
    }
}
