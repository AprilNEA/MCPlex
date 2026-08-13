use std::{
    collections::{BTreeMap, HashMap},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use http::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, Peer, RoleClient, RoleServer,
    model::*,
    service::{
        InboundStreamOrigin, NotificationContext, PeerRequestOptions, RequestContext, ServiceError,
    },
    transport::{
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess, auth::AuthClient,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::{Mutex, MutexGuard, Notify, RwLock},
    task::JoinHandle,
};

use crate::{
    config::{Config, ServerConfig, TransportConfig},
    oauth, secrets,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
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

pub struct Runtime {
    pub statuses: RwLock<BTreeMap<String, ServerStatus>>,
    config: RwLock<Config>,
    pub logs: RwLock<Vec<LogEntry>>,
    notify: tokio::sync::broadcast::Sender<RuntimeChange>,
    lifecycle: Mutex<()>,
    shutting_down: AtomicBool,
    log_id: AtomicU64,
}

#[derive(Clone, Debug)]
pub enum RuntimeChange {
    SyncConfig,
    Restart(String),
    Shutdown,
}

pub struct UpstreamSession {
    pub peer: Peer<RoleClient>,
    pub info: ServerInfo,
    service_cancel: Option<rmcp::service::RunningServiceCancellationToken>,
    waiter: JoinHandle<()>,
}

pub struct BridgeState {
    upstream_to_downstream_progress: RwLock<HashMap<ProgressToken, ProgressToken>>,
    downstream_to_upstream_progress: RwLock<HashMap<ProgressToken, ProgressToken>>,
    request_routes: RwLock<HashMap<RequestId, Peer<RoleServer>>>,
    request_routes_changed: Notify,
    serialize_requests: bool,
    request_gate: Mutex<()>,
}

impl BridgeState {
    pub fn new(serialize_requests: bool) -> Self {
        Self {
            upstream_to_downstream_progress: RwLock::new(HashMap::new()),
            downstream_to_upstream_progress: RwLock::new(HashMap::new()),
            request_routes: RwLock::new(HashMap::new()),
            request_routes_changed: Notify::new(),
            serialize_requests,
            request_gate: Mutex::new(()),
        }
    }

    pub async fn request_guard(&self) -> Option<MutexGuard<'_, ()>> {
        if self.serialize_requests {
            Some(self.request_gate.lock().await)
        } else {
            None
        }
    }

    pub async fn bind_request(&self, upstream: RequestId, downstream: Peer<RoleServer>) {
        self.request_routes
            .write()
            .await
            .insert(upstream, downstream);
        self.request_routes_changed.notify_waiters();
    }

    pub async fn unbind_request(&self, upstream: &RequestId) {
        self.request_routes.write().await.remove(upstream);
        self.request_routes_changed.notify_waiters();
    }

    async fn downstream_for(
        &self,
        context: &RequestContext<RoleClient>,
    ) -> Result<Peer<RoleServer>, ErrorData> {
        let origin = context.extensions.get::<InboundStreamOrigin>().cloned();
        self.downstream_for_origin(origin, context.ct.clone()).await
    }

    async fn downstream_for_notification(
        &self,
        context: &NotificationContext<RoleClient>,
    ) -> Option<Peer<RoleServer>> {
        let origin = context.extensions.get::<InboundStreamOrigin>().cloned();
        self.downstream_for_origin(origin, tokio_util::sync::CancellationToken::new())
            .await
            .ok()
    }

    async fn downstream_for_origin(
        &self,
        origin: Option<InboundStreamOrigin>,
        cancelled: tokio_util::sync::CancellationToken,
    ) -> Result<Peer<RoleServer>, ErrorData> {
        let route = async {
            loop {
                let changed = self.request_routes_changed.notified();
                let routes = self.request_routes.read().await;
                let route = match &origin {
                    Some(InboundStreamOrigin::OutboundRequest(id)) => routes.get(id).cloned(),
                    Some(InboundStreamOrigin::Unassociated) | None if routes.len() == 1 => {
                        routes.values().next().cloned()
                    }
                    Some(InboundStreamOrigin::Unassociated) | None if routes.len() > 1 => {
                        return Err(ErrorData::invalid_request(
                            "upstream reverse request is not associated with a unique downstream request",
                            None,
                        ));
                    }
                    _ => None,
                };
                drop(routes);
                if let Some(route) = route {
                    return Ok(route);
                }
                changed.await;
            }
        };
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(1), route) => result
                .map_err(|_| ErrorData::invalid_request(
                    "upstream reverse request has no active downstream request association",
                    None,
                ))?,
            _ = cancelled.cancelled() => Err(ErrorData::invalid_request(
                "upstream reverse request was cancelled before association",
                None,
            )),
        }
    }

    pub async fn bind_upstream_progress(
        &self,
        upstream: ProgressToken,
        downstream: Option<ProgressToken>,
    ) {
        if let Some(downstream) = downstream {
            self.upstream_to_downstream_progress
                .write()
                .await
                .insert(upstream, downstream);
        }
    }

    pub async fn unbind_upstream_progress(&self, upstream: &ProgressToken) {
        self.upstream_to_downstream_progress
            .write()
            .await
            .remove(upstream);
    }

    pub async fn forward_downstream_progress(
        &self,
        mut params: ProgressNotificationParam,
    ) -> Option<ProgressNotificationParam> {
        let token = self
            .upstream_to_downstream_progress
            .read()
            .await
            .get(&params.progress_token)
            .cloned()?;
        params.progress_token = token;
        Some(params)
    }

    async fn bind_downstream_progress(
        &self,
        downstream: ProgressToken,
        upstream: Option<ProgressToken>,
    ) {
        if let Some(upstream) = upstream {
            self.downstream_to_upstream_progress
                .write()
                .await
                .insert(downstream, upstream);
        }
    }

    async fn unbind_downstream_progress(&self, downstream: &ProgressToken) {
        self.downstream_to_upstream_progress
            .write()
            .await
            .remove(downstream);
    }

    pub async fn forward_upstream_progress(
        &self,
        mut params: ProgressNotificationParam,
    ) -> Option<ProgressNotificationParam> {
        let token = self
            .downstream_to_upstream_progress
            .read()
            .await
            .get(&params.progress_token)
            .cloned()?;
        params.progress_token = token;
        Some(params)
    }
}

impl Default for BridgeState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Drop for UpstreamSession {
    fn drop(&mut self) {
        if let Some(token) = self.service_cancel.take() {
            token.cancel();
        }
        self.waiter.abort();
    }
}

#[derive(Clone)]
pub struct BridgeClientHandler {
    pub fallback_downstream: Option<Peer<RoleServer>>,
    pub client_info: ClientInfo,
    pub bridge: Arc<BridgeState>,
}

fn mcp_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn service_error(error: ServiceError) -> ErrorData {
    match error {
        ServiceError::McpError(error) => error,
        other => mcp_error(other),
    }
}

#[allow(deprecated)]
impl ClientHandler for BridgeClientHandler {
    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        let result = self
            .forward_reverse(
                ServerRequest::CreateMessageRequest(CreateMessageRequest::new(params)),
                context,
            )
            .await?;
        match result {
            ClientResult::CreateMessageResult(result) => Ok(*result),
            _ => Err(mcp_error(ServiceError::UnexpectedResponse)),
        }
    }

    async fn list_roots(
        &self,
        context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        let result = self
            .forward_reverse(
                ServerRequest::ListRootsRequest(ListRootsRequest::default()),
                context,
            )
            .await?;
        match result {
            ClientResult::ListRootsResult(result) => Ok(result),
            _ => Err(mcp_error(ServiceError::UnexpectedResponse)),
        }
    }

    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let result = self
            .forward_reverse(
                ServerRequest::ElicitRequest(ElicitRequest::new(params)),
                context,
            )
            .await?;
        match result {
            ClientResult::ElicitResult(result) => Ok(result),
            _ => Err(mcp_error(ServiceError::UnexpectedResponse)),
        }
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleClient>,
    ) -> Result<CustomResult, ErrorData> {
        match self
            .forward_reverse(ServerRequest::CustomRequest(request), context)
            .await?
        {
            ClientResult::CustomResult(result) => Ok(result),
            _ => Err(mcp_error(ServiceError::UnexpectedResponse)),
        }
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        self.send_downstream_notification(
            ServerNotification::LoggingMessageNotification(LoggingMessageNotification::new(params)),
            context,
        )
        .await;
    }
    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        self.send_downstream_notification(
            ServerNotification::ResourceUpdatedNotification(ResourceUpdatedNotification::new(
                params,
            )),
            context,
        )
        .await;
    }
    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.send_downstream_notification(
            ServerNotification::ToolListChangedNotification(ToolListChangedNotification::default()),
            context,
        )
        .await;
    }
    async fn on_resource_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.send_downstream_notification(
            ServerNotification::ResourceListChangedNotification(
                ResourceListChangedNotification::default(),
            ),
            context,
        )
        .await;
    }
    async fn on_prompt_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.send_downstream_notification(
            ServerNotification::PromptListChangedNotification(
                PromptListChangedNotification::default(),
            ),
            context,
        )
        .await;
    }
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        if let Some(params) = self.bridge.forward_downstream_progress(params).await {
            self.send_downstream_notification(
                ServerNotification::ProgressNotification(ProgressNotification::new(params)),
                context,
            )
            .await;
        }
    }
    async fn on_task_status(
        &self,
        params: TaskStatusNotificationParams,
        context: NotificationContext<RoleClient>,
    ) {
        self.send_downstream_notification(
            ServerNotification::TaskStatusNotification(TaskStatusNotification::new(params)),
            context,
        )
        .await;
    }
    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleClient>,
    ) {
        self.send_downstream_notification(
            ServerNotification::CustomNotification(notification),
            context,
        )
        .await;
    }
}

impl BridgeClientHandler {
    async fn send_downstream_notification(
        &self,
        mut notification: ServerNotification,
        context: NotificationContext<RoleClient>,
    ) {
        let downstream = if let Some(downstream) = &self.fallback_downstream {
            Some(downstream.clone())
        } else {
            self.bridge.downstream_for_notification(&context).await
        };
        notification.extensions_mut().insert(context.meta);
        if let Some(downstream) = downstream {
            let _ = downstream.send_notification(notification).await;
        }
    }

    async fn forward_reverse(
        &self,
        request: ServerRequest,
        context: RequestContext<RoleClient>,
    ) -> Result<ClientResult, ErrorData> {
        if self.client_info.protocol_version >= ProtocolVersion::V_2026_07_28 {
            return Err(ErrorData::invalid_request(
                "MCP 2026-07-28 server-to-client requests must use an InputRequiredResult (MRTR)",
                None,
            ));
        }
        let downstream = if let Some(downstream) = &self.fallback_downstream {
            downstream.clone()
        } else {
            self.bridge.downstream_for(&context).await?
        };
        let handle = downstream
            .send_cancellable_request(
                request,
                PeerRequestOptions::no_options().with_meta(context.meta.clone()),
            )
            .await
            .map_err(service_error)?;
        let id = handle.id.clone();
        let progress_token = handle.progress_token.clone();
        self.bridge
            .bind_downstream_progress(progress_token.clone(), context.meta.get_progress_token())
            .await;
        let result = tokio::select! {
            result = handle.await_response() => result.map_err(service_error),
            _ = context.ct.cancelled() => {
                let _ = downstream.notify_cancelled(CancelledNotificationParam::new(
                    Some(id), Some("originating upstream request cancelled".to_owned())
                )).await;
                Err(ErrorData::internal_error("request cancelled", None))
            }
        };
        self.bridge
            .unbind_downstream_progress(&progress_token)
            .await;
        result
    }
}

impl Runtime {
    pub async fn new(config: Config) -> Arc<Self> {
        let (notify, _) = tokio::sync::broadcast::channel(16);
        let this = Arc::new(Self {
            statuses: RwLock::new(BTreeMap::new()),
            config: RwLock::new(config.clone()),
            logs: RwLock::new(Vec::new()),
            notify,
            lifecycle: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            log_id: AtomicU64::new(1),
        });
        this.reset_statuses(&config).await;
        this
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeChange> {
        self.notify.subscribe()
    }
    pub async fn config(&self) -> Config {
        self.config.read().await.clone()
    }

    async fn reset_statuses(&self, config: &Config) {
        let old = self.statuses.read().await.clone();
        let mut statuses = BTreeMap::new();
        for (id, server) in &config.servers {
            statuses.insert(
                id.clone(),
                ServerStatus {
                    id: id.clone(),
                    state: if server.enabled {
                        State::Idle
                    } else {
                        State::Disabled
                    },
                    restarts: old.get(id).map_or(0, |s| s.restarts),
                    error: None,
                    tools: old.get(id).map_or(0, |s| s.tools),
                    resources: old.get(id).map_or(0, |s| s.resources),
                    prompts: old.get(id).map_or(0, |s| s.prompts),
                    last_call_ms: old.get(id).and_then(|s| s.last_call_ms),
                },
            );
        }
        *self.statuses.write().await = statuses;
    }

    pub async fn reload(self: &Arc<Self>, config: Config) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("runtime is shutting down");
        }
        if self.config.read().await.daemon != config.daemon {
            anyhow::bail!("daemon endpoint changed; process restart required");
        }
        *self.config.write().await = config.clone();
        self.reset_statuses(&config).await;
        let _ = self.notify.send(RuntimeChange::SyncConfig);
        self.log("configuration reloaded").await;
        Ok(())
    }

    pub async fn restart(self: &Arc<Self>, id: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let config = self.config.read().await.clone();
        let server = config.servers.get(id).context("unknown server")?;
        if !server.enabled {
            anyhow::bail!("server is disabled");
        }
        if let Some(status) = self.statuses.write().await.get_mut(id) {
            status.state = State::Idle;
            status.restarts += 1;
            status.error = None;
            status.tools = 0;
            status.resources = 0;
            status.prompts = 0;
        }
        let _ = self.notify.send(RuntimeChange::Restart(id.to_owned()));
        self.log(format!("{id}: sessions restart requested")).await;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let _guard = self.lifecycle.lock().await;
        self.shutting_down.store(true, Ordering::Release);
        let _ = self.notify.send(RuntimeChange::Shutdown);
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

    pub async fn record_inventory(
        &self,
        id: &str,
        tools: Option<usize>,
        resources: Option<usize>,
        prompts: Option<usize>,
    ) {
        if let Some(status) = self.statuses.write().await.get_mut(id) {
            if let Some(tools) = tools {
                status.tools = tools;
            }
            if let Some(resources) = resources {
                status.resources = resources;
            }
            if let Some(prompts) = prompts {
                status.prompts = prompts;
            }
        }
    }

    pub async fn connect_session(
        self: &Arc<Self>,
        id: &str,
        server: &ServerConfig,
        fallback_downstream: Option<Peer<RoleServer>>,
        client_info: ClientInfo,
        bridge: Arc<BridgeState>,
    ) -> Result<UpstreamSession> {
        let result = self
            .connect_session_inner(id, server, fallback_downstream, client_info, bridge)
            .await;
        if let Err(error) = &result {
            let error = redact(&format!("{error:#}"));
            self.set_connection_state(id, State::Degraded, Some(error.clone()))
                .await;
            self.log(format!("{id}: connection failed: {error}")).await;
        }
        result
    }

    async fn connect_session_inner(
        self: &Arc<Self>,
        id: &str,
        server: &ServerConfig,
        fallback_downstream: Option<Peer<RoleServer>>,
        client_info: ClientInfo,
        bridge: Arc<BridgeState>,
    ) -> Result<UpstreamSession> {
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("runtime is shutting down");
        }
        self.set_connection_state(id, State::Starting, None).await;
        self.log(format!("{id}: connecting session")).await;
        let lifecycle = if fallback_downstream.is_some() {
            ClientLifecycleMode::Initialize
        } else {
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            }
        };
        let handler = BridgeClientHandler {
            fallback_downstream,
            client_info,
            bridge,
        };
        let service = match &server.transport {
            TransportConfig::Stdio { command, args, env } => {
                let mut resolved = Vec::new();
                for (key, value) in env {
                    if !value.starts_with("env:") && !value.starts_with("keychain:") {
                        tracing::warn!(server = id, key, "plaintext secret in config");
                    }
                    resolved.push((key.clone(), secrets::resolve(value)?));
                }
                let transport = TokioChildProcess::new(Command::new(command).configure(|c| {
                    c.args(args).envs(resolved).stderr(Stdio::inherit());
                }))?;
                tokio::time::timeout(
                    Duration::from_secs(10),
                    handler.serve_with_lifecycle(transport, lifecycle),
                )
                .await
                .context("upstream connection timed out")??
            }
            TransportConfig::Http {
                url,
                headers,
                oauth: oauth_config,
            } => {
                let mut map = HashMap::new();
                for (key, value) in headers {
                    if !value.starts_with("env:") && !value.starts_with("keychain:") {
                        tracing::warn!(server = id, key, "plaintext secret in config");
                    }
                    map.insert(
                        HeaderName::try_from(key)?,
                        HeaderValue::try_from(secrets::resolve(value)?)?,
                    );
                }
                let cfg =
                    StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(map);
                if oauth_config.is_some() {
                    let manager = oauth::authorization_manager(id, url).await?;
                    let client = AuthClient::new(reqwest::Client::new(), manager);
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        handler.serve_with_lifecycle(
                            StreamableHttpClientTransport::with_client(client, cfg),
                            lifecycle,
                        ),
                    )
                    .await
                    .context("upstream connection timed out")??
                } else {
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        handler.serve_with_lifecycle(
                            StreamableHttpClientTransport::from_config(cfg),
                            lifecycle,
                        ),
                    )
                    .await
                    .context("upstream connection timed out")??
                }
            }
        };
        let peer = service.peer().clone();
        let negotiated = service
            .peer_info()
            .context("upstream supplied no server info")?;
        let mut info = ServerInfo::new(negotiated.capabilities.clone())
            .with_protocol_version(negotiated.protocol_version.clone())
            .with_server_info(
                negotiated
                    .server_info
                    .clone()
                    .unwrap_or_else(|| Implementation::new("upstream", "unknown")),
            );
        info.instructions = negotiated.instructions.clone();
        info.meta = negotiated.meta.clone();
        let runtime = Arc::downgrade(self);
        let session_id = id.to_owned();
        let service_cancel = service.cancellation_token();
        let waiter = tokio::spawn(async move {
            if let Err(error) = service.waiting().await
                && let Some(runtime) = runtime.upgrade()
            {
                runtime
                    .log(format!("{session_id}: disconnected: {error}"))
                    .await;
                runtime
                    .set_connection_state(
                        &session_id,
                        State::Degraded,
                        Some(redact(&error.to_string())),
                    )
                    .await;
            }
        });
        self.set_connection_state(id, State::Ready, None).await;
        self.log(format!("{id}: session connected")).await;
        Ok(UpstreamSession {
            peer,
            info,
            service_cancel: Some(service_cancel),
            waiter,
        })
    }

    async fn set_connection_state(&self, id: &str, state: State, error: Option<String>) {
        if let Some(status) = self.statuses.write().await.get_mut(id) {
            status.state = state;
            status.error = error;
        }
    }
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
                enabled: false,
                tags: vec![],
            },
        );
        let runtime = Runtime::new(config).await;
        runtime.record_call("test", 42).await;
        assert_eq!(runtime.statuses.read().await["test"].last_call_ms, Some(42));
    }
    #[test]
    fn redaction_hides_credentials_but_keeps_diagnostics() {
        assert_eq!(redact("connection refused"), "connection refused");
        assert!(!redact("Bearer abc token=xyz password:bad").contains("abc"));
    }

    #[tokio::test]
    async fn progress_tokens_are_translated_in_both_directions() {
        let bridge = BridgeState::default();
        let downstream = ProgressToken(NumberOrString::String("downstream".into()));
        let upstream = ProgressToken(NumberOrString::String("upstream".into()));

        bridge
            .bind_upstream_progress(upstream.clone(), Some(downstream.clone()))
            .await;
        let forwarded = bridge
            .forward_downstream_progress(ProgressNotificationParam::new(upstream.clone(), 1.0))
            .await
            .expect("mapped upstream token");
        assert_eq!(forwarded.progress_token, downstream);
        bridge.unbind_upstream_progress(&upstream).await;
        assert!(
            bridge
                .forward_downstream_progress(ProgressNotificationParam::new(upstream, 2.0))
                .await
                .is_none()
        );

        let upstream = ProgressToken(NumberOrString::String("upstream-reverse".into()));
        let downstream = ProgressToken(NumberOrString::String("downstream-reverse".into()));
        bridge
            .bind_downstream_progress(downstream.clone(), Some(upstream.clone()))
            .await;
        let forwarded = bridge
            .forward_upstream_progress(ProgressNotificationParam::new(downstream.clone(), 3.0))
            .await
            .expect("mapped downstream token");
        assert_eq!(forwarded.progress_token, upstream);
        bridge.unbind_downstream_progress(&downstream).await;
    }
}
