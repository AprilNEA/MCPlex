use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    process::Stdio,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use http::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceExt,
    model::{Prompt, Resource, Tool},
    service::NotificationContext,
    transport::{
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::{Mutex, RwLock},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, ServerConfig, TransportConfig},
    namespacing::public_name,
    secrets,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Ready,
    Degraded,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerStatus {
    pub id: String,
    pub state: State,
    pub restarts: u64,
    pub error: Option<String>,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    pub last_call_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp: u64,
    pub server: Option<String>,
    pub message: String,
}

#[derive(Default)]
pub struct Catalog {
    pub tools: Vec<Tool>,
    pub resources: Vec<Resource>,
    pub prompts: Vec<Prompt>,
    pub tool_routes: HashMap<String, (Peer<RoleClient>, String, String)>,
    pub resource_routes: HashMap<String, (Peer<RoleClient>, String, String)>,
    pub prompt_routes: HashMap<String, (Peer<RoleClient>, String, String)>,
    sources: BTreeMap<String, Source>,
}
#[derive(Clone)]
struct Source {
    prefix: String,
    generation: u64,
    peer: Peer<RoleClient>,
    tools: Vec<Tool>,
    resources: Vec<Resource>,
    prompts: Vec<Prompt>,
}

pub struct Runtime {
    pub catalog: RwLock<Catalog>,
    pub statuses: RwLock<BTreeMap<String, ServerStatus>>,
    config: RwLock<Config>,
    tasks: Mutex<HashMap<String, (CancellationToken, JoinHandle<()>)>>,
    pub logs: RwLock<Vec<LogEntry>>,
    notify: tokio::sync::broadcast::Sender<()>,
    lifecycle: Mutex<()>,
    refreshing: Mutex<HashSet<(String, u64, Category)>>,
    shutting_down: AtomicBool,
    generation: AtomicU64,
    log_id: AtomicU64,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Category {
    Tools,
    Resources,
    Prompts,
}

#[derive(Clone)]
struct UpstreamHandler {
    runtime: Weak<Runtime>,
    id: String,
    generation: u64,
}

struct WaiterGuard {
    token: Option<rmcp::service::RunningServiceCancellationToken>,
    waiter: tokio::task::AbortHandle,
}
impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            token.cancel();
        }
        self.waiter.abort();
    }
}
impl UpstreamHandler {
    fn refresh(&self, category: Category, context: NotificationContext<RoleClient>) {
        let (runtime, id, generation, peer) = (
            self.runtime.clone(),
            self.id.clone(),
            self.generation,
            context.peer,
        );
        tokio::spawn(async move {
            if let Some(runtime) = runtime.upgrade() {
                runtime.refresh(&id, generation, category, peer).await;
            }
        });
    }
}
impl ClientHandler for UpstreamHandler {
    async fn on_tool_list_changed(&self, c: NotificationContext<RoleClient>) {
        self.refresh(Category::Tools, c);
    }
    async fn on_resource_list_changed(&self, c: NotificationContext<RoleClient>) {
        self.refresh(Category::Resources, c);
    }
    async fn on_prompt_list_changed(&self, c: NotificationContext<RoleClient>) {
        self.refresh(Category::Prompts, c);
    }
}

impl Runtime {
    pub async fn new(config: Config) -> Arc<Self> {
        let (notify, _) = tokio::sync::broadcast::channel(16);
        let this = Arc::new(Self {
            catalog: RwLock::new(Catalog::default()),
            statuses: RwLock::new(BTreeMap::new()),
            config: RwLock::new(Config {
                daemon: config.daemon.clone(),
                servers: BTreeMap::new(),
            }),
            tasks: Mutex::new(HashMap::new()),
            logs: RwLock::new(Vec::new()),
            notify,
            lifecycle: Mutex::new(()),
            refreshing: Mutex::new(HashSet::new()),
            shutting_down: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            log_id: AtomicU64::new(1),
        });
        {
            let _guard = this.lifecycle.lock().await;
            this.apply_locked(config)
                .await
                .expect("initial config is valid");
        }
        this
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.notify.subscribe()
    }
    pub async fn config(&self) -> Config {
        self.config.read().await.clone()
    }
    pub async fn log(&self, message: impl Into<String>) {
        let message = message.into();
        let server = message.split_once(':').map(|(id, _)| id.to_owned());
        let mut logs = self.logs.write().await;
        logs.push(LogEntry {
            id: self.log_id.fetch_add(1, Ordering::Relaxed),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            server,
            message: redact(&message),
        });
        if logs.len() > 500 {
            logs.remove(0);
        }
    }
    pub async fn record_call(&self, id: &str, elapsed_ms: u64) {
        if let Some(status) = self.statuses.write().await.get_mut(id) {
            status.last_call_ms = Some(elapsed_ms);
        }
    }
    pub async fn reload(self: &Arc<Self>, config: Config) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("runtime is shutting down");
        }
        self.apply_locked(config).await
    }
    async fn apply_locked(self: &Arc<Self>, config: Config) -> Result<()> {
        let old = self.config.read().await.clone();
        if old == config {
            return Ok(());
        }
        if old.daemon != config.daemon {
            anyhow::bail!("daemon endpoint changed; process restart required");
        }
        *self.config.write().await = config.clone();
        let mut changed = Vec::new();
        for id in old.servers.keys().chain(config.servers.keys()) {
            if old.servers.get(id) != config.servers.get(id) && !changed.contains(id) {
                changed.push(id.clone());
            }
        }
        for id in changed {
            self.stop_locked(&id).await;
            if let Some(server) = config.servers.get(&id) {
                if server.enabled {
                    self.start(id, server.clone()).await
                } else {
                    self.set_disabled(&id).await
                }
            }
        }
        for (id, s) in &config.servers {
            if !self.statuses.read().await.contains_key(id) {
                if s.enabled {
                    self.start(id.clone(), s.clone()).await
                } else {
                    self.set_disabled(id).await
                }
            }
        }
        let _ = self.notify.send(());
        self.log("configuration reloaded").await;
        Ok(())
    }
    async fn set_disabled(&self, id: &str) {
        self.statuses.write().await.insert(
            id.into(),
            ServerStatus {
                id: id.into(),
                state: State::Disabled,
                restarts: 0,
                error: None,
                tools: 0,
                resources: 0,
                prompts: 0,
                last_call_ms: None,
            },
        );
    }
    async fn start(self: &Arc<Self>, id: String, server: ServerConfig) {
        let cancel = CancellationToken::new();
        self.statuses.write().await.insert(
            id.clone(),
            ServerStatus {
                id: id.clone(),
                state: State::Starting,
                restarts: 0,
                error: None,
                tools: 0,
                resources: 0,
                prompts: 0,
                last_call_ms: None,
            },
        );
        let me = self.clone();
        let child = cancel.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move { me.supervise(id, server, child).await });
        let replaced = self.tasks.lock().await.insert(task_id, (cancel, handle));
        assert!(replaced.is_none(), "duplicate supervisor task insertion");
    }
    async fn stop_locked(&self, id: &str) {
        if let Some((cancel, mut handle)) = self.tasks.lock().await.remove(id) {
            cancel.cancel();
            // rmcp's own cancellation cleanup has a five-second budget. Keep the
            // supervisor owned long enough for it to cancel and join its waiter.
            if tokio::time::timeout(Duration::from_secs(10), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        self.remove_source(id).await;
        self.statuses.write().await.remove(id);
    }
    pub async fn restart(self: &Arc<Self>, id: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let cfg = self
            .config
            .read()
            .await
            .servers
            .get(id)
            .cloned()
            .context("unknown server")?;
        if !cfg.enabled {
            anyhow::bail!("server is disabled");
        }
        self.stop_locked(id).await;
        self.start(id.into(), cfg).await;
        Ok(())
    }
    async fn supervise(
        self: Arc<Self>,
        id: String,
        server: ServerConfig,
        cancel: CancellationToken,
    ) {
        let mut delay = Duration::from_secs(1);
        let mut restarts = 0;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            self.set_state(&id, State::Starting, restarts, None).await;
            let result = self.connect_once(&id, &server, restarts, &cancel).await;
            if cancel.is_cancelled() {
                break;
            }
            restarts += 1;
            let error = result.err().map(|e| redact(&e.to_string()));
            self.remove_source(&id).await;
            self.set_state(&id, State::Degraded, restarts, error.clone())
                .await;
            self.log(format!("{id}: disconnected: {}", error.unwrap_or_default()))
                .await;
            tokio::select! { _=cancel.cancelled()=>break, _=tokio::time::sleep(delay)=>{} }
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        self.remove_source(&id).await;
    }
    async fn connect_once(
        self: &Arc<Self>,
        id: &str,
        server: &ServerConfig,
        restarts: u64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        match &server.transport {
            TransportConfig::Stdio { command, args, env } => {
                let mut resolved = Vec::new();
                for (k, v) in env {
                    if !v.starts_with("env:") && !v.starts_with("keychain:") {
                        tracing::warn!(server = id, key = k, "plaintext secret in config")
                    }
                    resolved.push((k.clone(), secrets::resolve(v)?));
                }
                let t = TokioChildProcess::new(Command::new(command).configure(|c| {
                    c.args(args).envs(resolved).stderr(Stdio::inherit());
                }))?;
                let generation = self.generation.fetch_add(1, Ordering::Relaxed);
                let handler = UpstreamHandler {
                    runtime: Arc::downgrade(self),
                    id: id.into(),
                    generation,
                };
                let service = tokio::time::timeout(Duration::from_secs(10), handler.serve(t))
                    .await
                    .context("upstream connection timed out")??;
                self.run_service(id, server, generation, restarts, service, cancel)
                    .await
            }
            TransportConfig::Http { url, headers } => {
                let mut map = HashMap::new();
                for (k, v) in headers {
                    if !v.starts_with("env:") && !v.starts_with("keychain:") {
                        tracing::warn!(server = id, key = k, "plaintext secret in config")
                    }
                    map.insert(
                        HeaderName::try_from(k)?,
                        HeaderValue::try_from(secrets::resolve(v)?)?,
                    );
                }
                let cfg =
                    StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(map);
                let generation = self.generation.fetch_add(1, Ordering::Relaxed);
                let handler = UpstreamHandler {
                    runtime: Arc::downgrade(self),
                    id: id.into(),
                    generation,
                };
                let service = tokio::time::timeout(
                    Duration::from_secs(10),
                    handler.serve(StreamableHttpClientTransport::from_config(cfg)),
                )
                .await
                .context("upstream connection timed out")??;
                self.run_service(id, server, generation, restarts, service, cancel)
                    .await
            }
        }
    }
    async fn run_service(
        &self,
        id: &str,
        server: &ServerConfig,
        generation: u64,
        restarts: u64,
        service: rmcp::service::RunningService<RoleClient, UpstreamHandler>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let peer = service.peer().clone();
        let capabilities = &service
            .peer_info()
            .context("upstream supplied no server info")?
            .capabilities;
        let hydrate = async {
            let tools = if capabilities.tools.is_some() {
                peer.list_all_tools().await?
            } else {
                Vec::new()
            };
            let resources = if capabilities.resources.is_some() {
                peer.list_all_resources().await?
            } else {
                Vec::new()
            };
            let prompts = if capabilities.prompts.is_some() {
                peer.list_all_prompts().await?
            } else {
                Vec::new()
            };
            Ok::<_, rmcp::service::ServiceError>((tools, resources, prompts))
        };
        let (tools, resources, prompts) = tokio::time::timeout(Duration::from_secs(10), hydrate)
            .await
            .context("initial catalog hydration timed out")??;
        if cancel.is_cancelled() {
            service.cancellation_token().cancel();
            service.waiting().await?;
            return Ok(());
        }
        self.install(
            id,
            Source {
                prefix: server.alias.clone().unwrap_or_else(|| id.into()),
                generation,
                peer,
                tools,
                resources,
                prompts,
            },
            cancel,
        )
        .await?;
        self.set_state(id, State::Ready, restarts, None).await;
        let token = service.cancellation_token();
        let mut waiter = tokio::spawn(async move { service.waiting().await });
        // If the supervisor itself is force-aborted, cancellation still reaches
        // rmcp and the consuming waiter cannot become detached.
        let mut waiter_guard = WaiterGuard {
            token: Some(token),
            waiter: waiter.abort_handle(),
        };
        tokio::select! {
            result = &mut waiter => result.context("upstream waiter failed")?.map(|_| ()).map_err(Into::into),
            _ = cancel.cancelled() => {
                if let Some(token) = waiter_guard.token.take() { token.cancel(); }
                waiter.await.context("upstream waiter failed")?.map(|_| ()).map_err(Into::into)
            }
        }
    }
    async fn install(&self, id: &str, source: Source, cancel: &CancellationToken) -> Result<()> {
        let mut catalog = self.catalog.write().await;
        if cancel.is_cancelled() {
            anyhow::bail!("source stopped during catalog hydration")
        }
        let mut sources = catalog.sources.clone();
        if sources
            .get(id)
            .is_some_and(|old| old.generation >= source.generation)
        {
            anyhow::bail!("stale source generation")
        }
        sources.insert(id.into(), source);
        let candidate = build_catalog(sources)?;
        if cancel.is_cancelled() {
            anyhow::bail!("source stopped during catalog commit")
        }
        *catalog = candidate;
        drop(catalog);
        let _ = self.notify.send(());
        Ok(())
    }
    async fn remove_source(&self, id: &str) {
        let mut catalog = self.catalog.write().await;
        let mut sources = catalog.sources.clone();
        if sources.remove(id).is_some() {
            *catalog = build_catalog(sources).expect("source removal must be valid");
            drop(catalog);
            let _ = self.notify.send(());
        }
    }
    async fn set_state(&self, id: &str, state: State, restarts: u64, error: Option<String>) {
        let c = self.catalog.read().await;
        let tools = c
            .tools
            .iter()
            .filter(|t| {
                c.tool_routes
                    .get(t.name.as_ref())
                    .is_some_and(|r| r.2 == id)
            })
            .count();
        let resources = c.resource_routes.values().filter(|r| r.2 == id).count();
        let prompts = c.prompt_routes.values().filter(|r| r.2 == id).count();
        drop(c);
        if let Some(s) = self.statuses.write().await.get_mut(id) {
            s.state = state;
            s.restarts = restarts;
            s.error = error;
            s.tools = tools;
            s.resources = resources;
            s.prompts = prompts;
        }
    }
    pub async fn shutdown(&self) {
        let _guard = self.lifecycle.lock().await;
        self.shutting_down.store(true, Ordering::Release);
        let ids: Vec<_> = self.tasks.lock().await.keys().cloned().collect();
        for id in ids {
            self.stop_locked(&id).await;
        }
    }

    async fn refresh(&self, id: &str, generation: u64, category: Category, peer: Peer<RoleClient>) {
        let key = (id.to_owned(), generation, category);
        if !self.refreshing.lock().await.insert(key.clone()) {
            return;
        }
        if self
            .catalog
            .read()
            .await
            .sources
            .get(id)
            .is_none_or(|s| s.generation != generation)
        {
            self.refreshing.lock().await.remove(&key);
            return;
        }
        let request = async {
            match category {
                Category::Tools => peer.list_all_tools().await.map(|v| (Some(v), None, None)),
                Category::Resources => peer
                    .list_all_resources()
                    .await
                    .map(|v| (None, Some(v), None)),
                Category::Prompts => peer.list_all_prompts().await.map(|v| (None, None, Some(v))),
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(10), request).await;
        self.refreshing.lock().await.remove(&key);
        let Ok(Ok((tools, resources, prompts))) = result else {
            return;
        };
        let mut c = self.catalog.write().await;
        let mut sources = c.sources.clone();
        let Some(source) = sources.get_mut(id).filter(|s| s.generation == generation) else {
            return;
        };
        if let Some(v) = tools {
            source.tools = v;
        }
        if let Some(v) = resources {
            source.resources = v;
        }
        if let Some(v) = prompts {
            source.prompts = v;
        }
        let Ok(candidate) = build_catalog(sources) else {
            return;
        };
        if c.sources.get(id).is_none_or(|s| s.generation != generation) {
            return;
        }
        *c = candidate;
        drop(c);
        let _ = self.notify.send(());
    }
}

/// Build and validate every derived route off-lock; callers swap the result atomically.
fn build_catalog(sources: BTreeMap<String, Source>) -> Result<Catalog> {
    let mut catalog = Catalog {
        sources: sources.clone(),
        ..Catalog::default()
    };
    let mut uri_counts = HashMap::new();
    for source in sources.values() {
        for resource in &source.resources {
            *uri_counts.entry(resource.uri.clone()).or_insert(0usize) += 1;
        }
    }
    for (id, source) in sources {
        for mut tool in source.tools {
            let upstream = tool.name.to_string();
            let public = public_name(&source.prefix, &upstream);
            tool.name = Cow::Owned(public.clone());
            if catalog
                .tool_routes
                .insert(public.clone(), (source.peer.clone(), upstream, id.clone()))
                .is_some()
            {
                anyhow::bail!("duplicate public tool route '{public}'");
            }
            catalog.tools.push(tool);
        }
        for mut prompt in source.prompts {
            let upstream = prompt.name.clone();
            let public = public_name(&source.prefix, &upstream);
            prompt.name = public.clone();
            if catalog
                .prompt_routes
                .insert(public.clone(), (source.peer.clone(), upstream, id.clone()))
                .is_some()
            {
                anyhow::bail!("duplicate public prompt route '{public}'");
            }
            catalog.prompts.push(prompt);
        }
        for mut resource in source.resources {
            let original = resource.uri.clone();
            let public = if uri_counts[&original] > 1 {
                format!("mcplex+{}:{}", source.prefix, percent(&original))
            } else {
                original.clone()
            };
            resource.uri = public.clone();
            if catalog
                .resource_routes
                .insert(public.clone(), (source.peer.clone(), original, id.clone()))
                .is_some()
            {
                anyhow::bail!("duplicate public resource route '{public}'");
            }
            catalog.resources.push(resource);
        }
    }
    validate_route_keys("tool", catalog.tool_routes.keys().map(String::as_str))?;
    validate_route_keys("prompt", catalog.prompt_routes.keys().map(String::as_str))?;
    validate_route_keys(
        "resource",
        catalog.resource_routes.keys().map(String::as_str),
    )?;
    Ok(catalog)
}

fn validate_route_keys<'a>(kind: &str, keys: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for key in keys {
        if !seen.insert(key) {
            anyhow::bail!("duplicate public {kind} route '{key}'");
        }
    }
    Ok(())
}

fn percent(value: &str) -> String {
    const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, URI_COMPONENT).to_string()
}
fn redact(s: &str) -> String {
    let mut output = Vec::new();
    let mut redact_next = false;
    for word in s.split_whitespace().take(80) {
        let lower = word.to_ascii_lowercase();
        let sensitive = [
            "authorization",
            "bearer",
            "password",
            "passwd",
            "secret",
            "token",
            "api_key",
            "apikey",
        ]
        .iter()
        .any(|needle| {
            lower.starts_with(needle)
                || lower.contains(&format!("{needle}="))
                || lower.contains(&format!("{needle}:"))
        });
        output.push(if sensitive || redact_next {
            "[REDACTED]"
        } else {
            word
        });
        redact_next = matches!(lower.trim_end_matches(':'), "bearer" | "authorization");
    }
    output.join(" ").chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn call_latency_updates_status() {
        let mut config = Config::default();
        config.servers.insert(
            "test".into(),
            ServerConfig {
                transport: TransportConfig::Stdio {
                    command: "unused".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                },
                alias: None,
                enabled: false,
                tags: vec![],
            },
        );
        let runtime = Runtime::new(config).await;
        runtime.record_call("test", 42).await;
        assert_eq!(runtime.statuses.read().await["test"].last_call_ms, Some(42));
        runtime.shutdown().await;
    }
    #[test]
    fn redaction_hides_credentials_but_keeps_diagnostics() {
        assert_eq!(redact("connection refused"), "connection refused");
        assert!(!redact("Bearer abc token=xyz password:bad").contains("abc"));
    }
    #[test]
    fn route_key_validation_rejects_candidate_collisions() {
        assert!(validate_route_keys("tool", ["a__one", "b__one"]).is_ok());
        let error = validate_route_keys("tool", ["same", "same"]).unwrap_err();
        assert_eq!(error.to_string(), "duplicate public tool route 'same'");
    }
    #[test]
    fn resource_uri_encoding_uses_rfc3986_unreserved_characters() {
        assert_eq!(
            percent("https://例.test/a b+%~-._"),
            "https%3A%2F%2F%E4%BE%8B.test%2Fa%20b%2B%25~-._"
        );
    }
}
