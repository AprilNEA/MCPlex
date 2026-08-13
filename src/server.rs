use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use notify::{RecursiveMode, Watcher};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::{NotificationContext, PeerRequestOptions, RequestContext, SubscriptionContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt as _;

use crate::{
    config::{Config, ServerConfig, default_path},
    secrets,
    upstream::{BridgeState, Runtime, RuntimeChange, UpstreamSession},
};

#[derive(Clone)]
struct SessionProxy {
    runtime: Arc<Runtime>,
    id: Arc<str>,
    server: ServerConfig,
    upstream: Arc<Mutex<Option<UpstreamSession>>>,
    info: Arc<StdRwLock<Option<ServerInfo>>>,
    bridge: Arc<BridgeState>,
    shared: Arc<SharedUpstream>,
}

struct SharedUpstream {
    upstream: Mutex<Option<UpstreamSession>>,
    info: StdRwLock<Option<ServerInfo>>,
    bridge: Arc<BridgeState>,
}

impl SharedUpstream {
    async fn connect(
        &self,
        runtime: &Arc<Runtime>,
        id: &str,
        server: &ServerConfig,
        client_info: ClientInfo,
    ) -> Result<rmcp::Peer<rmcp::RoleClient>> {
        let mut upstream = self.upstream.lock().await;
        if upstream
            .as_ref()
            .is_some_and(|session| session.peer.is_transport_closed())
        {
            *upstream = None;
            *self.info.write().expect("server info lock poisoned") = None;
        }
        if upstream.is_none() {
            let session = runtime
                .connect_session(id, server, None, client_info, self.bridge.clone())
                .await?;
            *self.info.write().expect("server info lock poisoned") = Some(session.info.clone());
            *upstream = Some(session);
        }
        Ok(upstream
            .as_ref()
            .expect("shared upstream initialized")
            .peer
            .clone())
    }
}

impl SessionProxy {
    fn new(
        runtime: Arc<Runtime>,
        id: Arc<str>,
        server: ServerConfig,
        shared: Arc<SharedUpstream>,
    ) -> Self {
        Self {
            runtime,
            id,
            server,
            upstream: Arc::new(Mutex::new(None)),
            info: Arc::new(StdRwLock::new(None)),
            bridge: Arc::new(BridgeState::default()),
            shared,
        }
    }

    async fn legacy_peer(&self) -> Result<rmcp::Peer<rmcp::RoleClient>, ErrorData> {
        self.upstream
            .lock()
            .await
            .as_ref()
            .map(|session| session.peer.clone())
            .ok_or_else(|| ErrorData::invalid_request("MCP session is not initialized", None))
    }

    fn client_info(context: &RequestContext<RoleServer>) -> Result<ClientInfo, ErrorData> {
        let capabilities = context.client_capabilities().ok_or_else(|| {
            ErrorData::invalid_request("request omitted client capabilities", None)
        })?;
        let implementation = context
            .client_info()
            .unwrap_or_else(|| Implementation::new("mcplex", env!("CARGO_PKG_VERSION")));
        Ok(
            ClientInfo::new(capabilities, implementation).with_protocol_version(
                context
                    .protocol_version()
                    .unwrap_or(ProtocolVersion::V_2026_07_28),
            ),
        )
    }

    async fn ensure_shared(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(rmcp::Peer<rmcp::RoleClient>, Arc<BridgeState>), ErrorData> {
        let peer = self
            .shared
            .connect(
                &self.runtime,
                &self.id,
                &self.server,
                Self::client_info(context)?,
            )
            .await
            .map_err(internal)?;
        Ok((peer, self.shared.bridge.clone()))
    }

    async fn upstream_for(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(rmcp::Peer<rmcp::RoleClient>, Arc<BridgeState>), ErrorData> {
        if context
            .meta
            .missing_required_keys(&ProtocolVersion::V_2026_07_28)
            .is_empty()
        {
            self.ensure_shared(context).await
        } else {
            Ok((self.legacy_peer().await?, self.bridge.clone()))
        }
    }

    async fn forward(
        &self,
        mut request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerResult, ErrorData> {
        if let Some(parts) = context.extensions.get::<http::request::Parts>() {
            let mut forwarded = HeaderMap::new();
            for (name, value) in &parts.headers {
                if name.as_str().starts_with("mcp-param-") {
                    forwarded.insert(name.clone(), value.clone());
                }
            }
            request.extensions_mut().insert(forwarded);
        }
        let (peer, bridge) = self.upstream_for(&context).await?;
        let _request_guard = bridge.request_guard().await;
        let started = Instant::now();
        let handle = peer
            .send_cancellable_request(
                request,
                PeerRequestOptions::no_options().with_meta(context.meta.clone()),
            )
            .await
            .map_err(service_error)?;
        let id = handle.id.clone();
        let progress_token = handle.progress_token.clone();
        bridge.bind_request(id.clone(), context.peer.clone()).await;
        bridge
            .bind_upstream_progress(progress_token.clone(), context.meta.get_progress_token())
            .await;
        let result = tokio::select! {
            result = handle.await_response() => result.map_err(service_error),
            _ = context.ct.cancelled() => {
                let _ = peer.notify_cancelled(CancelledNotificationParam::new(
                    Some(id.clone()), Some("originating downstream request cancelled".to_owned())
                )).await;
                Err(ErrorData::internal_error("request cancelled", None))
            }
        };
        bridge.unbind_request(&id).await;
        bridge.unbind_upstream_progress(&progress_token).await;
        self.runtime
            .record_call(&self.id, started.elapsed().as_millis() as u64)
            .await;
        result
    }
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}
fn service_error(e: rmcp::service::ServiceError) -> ErrorData {
    match e {
        rmcp::service::ServiceError::McpError(error) => error,
        other => internal(other),
    }
}
fn unexpected() -> ErrorData {
    internal(rmcp::service::ServiceError::UnexpectedResponse)
}
impl ServerHandler for SessionProxy {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(
            ProtocolVersion::KNOWN_VERSIONS
                .iter()
                .filter(|version| **version <= ProtocolVersion::V_2026_07_28)
                .cloned()
                .collect(),
        )
    }

    fn get_info(&self) -> ServerInfo {
        self.info
            .read()
            .expect("server info lock poisoned")
            .clone()
            .or_else(|| {
                self.shared
                    .info
                    .read()
                    .expect("server info lock poisoned")
                    .clone()
            })
            .unwrap_or_else(|| {
                ServerInfo::new(ServerCapabilities::default())
                    .with_server_info(Implementation::new("mcplex", env!("CARGO_PKG_VERSION")))
            })
    }

    async fn initialize(
        &self,
        mut request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let downstream_version = if request.protocol_version < ProtocolVersion::V_2026_07_28
            && ProtocolVersion::KNOWN_VERSIONS.contains(&request.protocol_version)
        {
            request.protocol_version.clone()
        } else {
            ProtocolVersion::LATEST
        };
        request.protocol_version = downstream_version.clone();
        context.peer.set_peer_info(request.clone());
        if !self.server.enabled {
            return Err(ErrorData::invalid_request(
                "configured MCP server is disabled",
                None,
            ));
        }
        let session = self
            .runtime
            .connect_session(
                &self.id,
                &self.server,
                Some(context.peer.clone()),
                request,
                self.bridge.clone(),
            )
            .await
            .map_err(internal)?;
        let mut info = session.info.clone();
        info.protocol_version = downstream_version;
        *self.upstream.lock().await = Some(session);
        *self.info.write().expect("server info lock poisoned") = Some(info.clone());
        Ok(info)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        self.ensure_shared(&context).await?;
        let info = self
            .shared
            .info
            .read()
            .expect("server info lock poisoned")
            .clone()
            .expect("shared upstream info initialized");
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            info,
        ))
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let serialize_requests = matches!(
            &self.server.transport,
            crate::config::TransportConfig::Stdio { .. }
        );
        let session = self
            .runtime
            .connect_session(
                &self.id,
                &self.server,
                None,
                Self::client_info(context.request_context())?,
                Arc::new(BridgeState::new(serialize_requests)),
            )
            .await
            .map_err(internal)?;
        let peer = session.peer.clone();
        let mut subscription = peer
            .listen(context.accepted().clone())
            .await
            .map_err(service_error)?;
        loop {
            tokio::select! {
                _ = context.cancelled() => {
                    subscription
                        .cancel_with_reason(Some("downstream subscription cancelled".to_owned()))
                        .await
                        .map_err(service_error)?;
                    return Ok(());
                }
                notification = subscription.next() => {
                    let Some(notification) = notification.map_err(service_error)? else {
                        return Ok(());
                    };
                    context.sink().send(notification).await.map_err(internal)?;
                }
            }
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        match self
            .forward(
                ClientRequest::ListToolsRequest(ListToolsRequest {
                    method: Default::default(),
                    params: request,
                    extensions: Default::default(),
                }),
                context,
            )
            .await?
        {
            ServerResult::ListToolsResult(result) => {
                self.runtime
                    .record_inventory(&self.id, Some(result.tools.len()), None, None)
                    .await;
                Ok(result)
            }
            _ => Err(unexpected()),
        }
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        match self
            .forward(
                ClientRequest::ListResourcesRequest(ListResourcesRequest {
                    method: Default::default(),
                    params: request,
                    extensions: Default::default(),
                }),
                context,
            )
            .await?
        {
            ServerResult::ListResourcesResult(result) => {
                self.runtime
                    .record_inventory(&self.id, None, Some(result.resources.len()), None)
                    .await;
                Ok(result)
            }
            _ => Err(unexpected()),
        }
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        match self
            .forward(
                ClientRequest::ListResourceTemplatesRequest(ListResourceTemplatesRequest {
                    method: Default::default(),
                    params: request,
                    extensions: Default::default(),
                }),
                context,
            )
            .await?
        {
            ServerResult::ListResourceTemplatesResult(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        match self
            .forward(
                ClientRequest::ListPromptsRequest(ListPromptsRequest {
                    method: Default::default(),
                    params: request,
                    extensions: Default::default(),
                }),
                context,
            )
            .await?
        {
            ServerResult::ListPromptsResult(result) => {
                self.runtime
                    .record_inventory(&self.id, None, None, Some(result.prompts.len()))
                    .await;
                Ok(result)
            }
            _ => Err(unexpected()),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match self
            .forward(
                ClientRequest::CallToolRequest(CallToolRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::CallToolResult(result) => Ok(CallToolResponse::Complete(result)),
            ServerResult::InputRequiredResult(result) => {
                Ok(CallToolResponse::InputRequired(result))
            }
            ServerResult::CreateTaskResult(result) => Ok(CallToolResponse::Task(result)),
            _ => Err(unexpected()),
        }
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        match self
            .forward(
                ClientRequest::GetPromptRequest(GetPromptRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::GetPromptResult(result) => Ok(GetPromptResponse::Complete(result)),
            ServerResult::InputRequiredResult(result) => {
                Ok(GetPromptResponse::InputRequired(result))
            }
            _ => Err(unexpected()),
        }
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        match self
            .forward(
                ClientRequest::ReadResourceRequest(ReadResourceRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::ReadResourceResult(result) => Ok(ReadResourceResponse::Complete(result)),
            ServerResult::InputRequiredResult(result) => {
                Ok(ReadResourceResponse::InputRequired(result))
            }
            _ => Err(unexpected()),
        }
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        match self
            .forward(
                ClientRequest::CompleteRequest(CompleteRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::CompleteResult(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }

    #[allow(deprecated)]
    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        match self
            .forward(
                ClientRequest::SetLevelRequest(SetLevelRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        match self
            .forward(
                ClientRequest::SubscribeRequest(SubscribeRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        match self
            .forward(
                ClientRequest::UnsubscribeRequest(UnsubscribeRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        match self
            .forward(
                ClientRequest::GetTaskRequest(GetTaskRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::GetTaskResult(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        match self
            .forward(
                ClientRequest::UpdateTaskRequest(UpdateTaskRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::TaskAckResult(_) | ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        match self
            .forward(
                ClientRequest::CancelTaskRequest(CancelTaskRequest::new(request)),
                context,
            )
            .await?
        {
            ServerResult::TaskAckResult(_) | ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        if let Ok(peer) = self.legacy_peer().await {
            let mut notification = ClientNotification::RootsListChangedNotification(
                RootsListChangedNotification::default(),
            );
            notification.extensions_mut().insert(context.meta);
            let _ = peer.send_notification(notification).await;
        }
    }

    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        if let Some(notification) = self.bridge.forward_upstream_progress(notification).await
            && let Ok(peer) = self.legacy_peer().await
        {
            let mut notification =
                ClientNotification::ProgressNotification(ProgressNotification::new(notification));
            notification.extensions_mut().insert(context.meta);
            let _ = peer.send_notification(notification).await;
        }
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        match self
            .forward(ClientRequest::CustomRequest(request), context)
            .await?
        {
            ServerResult::CustomResult(result) => Ok(result),
            _ => Err(internal(rmcp::service::ServiceError::UnexpectedResponse)),
        }
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleServer>,
    ) {
        if let Ok(peer) = self.legacy_peer().await {
            let mut notification = ClientNotification::CustomNotification(notification);
            notification.extensions_mut().insert(context.meta);
            let _ = peer.send_notification(notification).await;
        }
    }
}

type McpRouters = Arc<RwLock<HashMap<String, (ServerConfig, Router)>>>;

fn needs_modern_capability_prewarm(headers: &HeaderMap) -> bool {
    let modern = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|version| version == ProtocolVersion::V_2026_07_28.as_str());
    let capability_dependent = headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|method| {
            matches!(
                method,
                "subscriptions/listen" | "tasks/get" | "tasks/update" | "tasks/cancel"
            )
        });
    modern && capability_dependent
}

fn gateway_client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mcplex", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2026_07_28)
}

fn mcp_router(runtime: Arc<Runtime>, id: String, server: ServerConfig) -> Router {
    let id: Arc<str> = id.into();
    let serialize_requests = matches!(
        server.transport,
        crate::config::TransportConfig::Stdio { .. }
    );
    let shared = Arc::new(SharedUpstream {
        upstream: Mutex::new(None),
        info: StdRwLock::new(None),
        bridge: Arc::new(BridgeState::new(serialize_requests)),
    });
    let service_runtime = runtime.clone();
    let service_id = id.clone();
    let service_server = server.clone();
    let service_shared = shared.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(SessionProxy::new(
                service_runtime.clone(),
                service_id.clone(),
                service_server.clone(),
                service_shared.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route_service("/", service)
        .layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let runtime = runtime.clone();
                let id = id.clone();
                let server = server.clone();
                let shared = shared.clone();
                async move {
                    if needs_modern_capability_prewarm(request.headers())
                        && shared
                            .connect(&runtime, &id, &server, gateway_client_info())
                            .await
                            .is_err()
                    {
                        return StatusCode::BAD_GATEWAY.into_response();
                    }
                    next.run(request).await
                }
            },
        ))
}

async fn sync_mcp_routers(registry: &McpRouters, runtime: &Arc<Runtime>, restart: Option<&str>) {
    let config = runtime.config().await;
    let mut routers = registry.write().await;
    routers.retain(|id, _| config.servers.get(id).is_some_and(|server| server.enabled));
    for (id, server) in config.servers {
        if !server.enabled {
            continue;
        }
        let unchanged = routers
            .get(&id)
            .is_some_and(|(current, _)| current == &server);
        if !unchanged || restart == Some(id.as_str()) {
            routers.insert(
                id.clone(),
                (server.clone(), mcp_router(runtime.clone(), id, server)),
            );
        }
    }
}

async fn aggregate_removed() -> impl IntoResponse {
    (
        StatusCode::GONE,
        Json(json!({
            "error": "aggregate MCP endpoint removed",
            "use": "/mcp/{server-id}"
        })),
    )
}

async fn dedicated_mcp(
    State(api): State<Api>,
    AxumPath(server): AxumPath<String>,
    mut request: Request<Body>,
) -> impl IntoResponse {
    if request.headers().contains_key(http::header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let router = api
        .mcp_routers
        .read()
        .await
        .get(&server)
        .map(|(_, router)| router.clone());
    let Some(router) = router else {
        return StatusCode::NOT_FOUND.into_response();
    };
    *request.uri_mut() = Uri::from_static("/");
    router
        .oneshot(request)
        .await
        .expect("infallible router")
        .into_response()
}

#[derive(Clone)]
struct Api {
    runtime: Arc<Runtime>,
    mcp_routers: McpRouters,
    token: Arc<str>,
    path: PathBuf,
    mutation: Arc<Mutex<()>>,
}
fn auth(headers: &HeaderMap, state: &Api) -> Result<(), (StatusCode, Json<Value>)> {
    let expected = format!("Bearer {}", state.token);
    if headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&expected) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        ))
    }
}
async fn status(State(s): State<Api>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth(&h, &s) {
        return e.into_response();
    }
    Json(json!({"servers":*s.runtime.statuses.read().await})).into_response()
}
#[derive(serde::Deserialize, Default)]
struct LogQuery {
    after: Option<u64>,
    server: Option<String>,
}
fn filter_logs(
    logs: &[crate::upstream::LogEntry],
    query: &LogQuery,
) -> Vec<crate::upstream::LogEntry> {
    logs.iter()
        .filter(|entry| query.after.is_none_or(|id| entry.id > id))
        .filter(|entry| {
            query
                .server
                .as_ref()
                .is_none_or(|id| entry.server.as_ref() == Some(id))
        })
        .cloned()
        .collect()
}
async fn logs(
    State(s): State<Api>,
    h: HeaderMap,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    if let Err(e) = auth(&h, &s) {
        return e.into_response();
    }
    Json(json!({"logs":filter_logs(&s.runtime.logs.read().await, &query)})).into_response()
}
async fn reload(State(s): State<Api>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth(&h, &s) {
        return e.into_response();
    }
    let _mutation = s.mutation.lock().await;
    match Config::load(&s.path) {
        Ok(c) => match s.runtime.reload(c).await {
            Ok(()) => Json(json!({"ok":true})).into_response(),
            Err(e) => (StatusCode::CONFLICT, Json(json!({"error":e.to_string()}))).into_response(),
        },
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

async fn mutate(
    State(s): State<Api>,
    h: HeaderMap,
    AxumPath((id, action)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = auth(&h, &s) {
        return e.into_response();
    }
    if !matches!(action.as_str(), "enable" | "disable" | "restart") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"action must be enable, disable, or restart"})),
        )
            .into_response();
    }
    let _mutation = s.mutation.lock().await;
    if action == "restart" {
        return match s.runtime.restart(&id).await {
            Ok(()) => Json(json!({"ok":true})).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e.to_string()}))).into_response(),
        };
    }
    let updated = crate::config::update_atomic(&s.path, |c| {
        let server = c.servers.get_mut(&id).context("unknown server")?;
        server.enabled = action == "enable";
        Ok(c.clone())
    });
    let c = match updated {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error":e.to_string()}))).into_response();
        }
    };
    match s.runtime.reload(c).await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error":e.to_string()}))).into_response(),
    }
}

pub async fn serve_path(path: Option<PathBuf>) -> Result<()> {
    let path = path.map_or_else(default_path, Ok)?;
    let config = if path.exists() {
        Config::load(&path)?
    } else {
        tracing::info!(path = %path.display(), "config not found; using defaults");
        Config::default()
    };
    serve(config, path).await
}

pub async fn serve(config: Config, path: PathBuf) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let address = SocketAddr::new(config.daemon.bind, config.daemon.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let watched_name = path.file_name().map(ToOwned::to_owned);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|event| {
            event
                .paths
                .iter()
                .any(|p| p.file_name() == watched_name.as_deref())
        }) {
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(parent, RecursiveMode::NonRecursive)?;
    let token = secrets::control_token()?;
    let runtime = Runtime::new(config.clone()).await;
    let registry: McpRouters = Arc::new(RwLock::new(HashMap::new()));
    sync_mcp_routers(&registry, &runtime, None).await;
    let api = Api {
        runtime: runtime.clone(),
        mcp_routers: registry.clone(),
        token: token.into(),
        path: path.clone(),
        mutation: Arc::new(Mutex::new(())),
    };
    let mutation = api.mutation.clone();
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/mcp",
            post(aggregate_removed)
                .delete(aggregate_removed)
                .get(aggregate_removed),
        )
        .route(
            "/mcp/{server}",
            post(dedicated_mcp).delete(dedicated_mcp).get(dedicated_mcp),
        )
        .route("/api/v1/status", get(status))
        .route("/api/v1/servers", get(status))
        .route("/api/v1/logs", get(logs))
        .route("/api/v1/reload", post(reload))
        .route("/api/v1/servers/{id}/{action}", post(mutate))
        .with_state(api);
    let rt = runtime.clone();
    let watch_path = path.clone();
    let watch_mutation = mutation.clone();
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            while rx.try_recv().is_ok() {}
            let _mutation = watch_mutation.lock().await;
            if let Ok(c) = Config::load(&watch_path) {
                if let Err(e) = rt.reload(c).await {
                    tracing::error!("reload failed: {e}");
                }
            }
        }
    });
    let mut changes = runtime.subscribe();
    let registry_changes = registry.clone();
    let runtime_changes = runtime.clone();
    tokio::spawn(async move {
        loop {
            let restart = match changes.recv().await {
                Ok(RuntimeChange::SyncConfig) => None,
                Ok(RuntimeChange::Restart(id)) => Some(id),
                Ok(RuntimeChange::Shutdown) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            sync_mcp_routers(&registry_changes, &runtime_changes, restart.as_deref()).await;
        }
    });
    tracing::info!(%address,"mcplex ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(path.clone(), runtime.clone(), mutation))
        .await?;
    drop(watcher);
    runtime.shutdown().await;
    Ok(())
}
async fn shutdown(path: PathBuf, runtime: Arc<Runtime>, mutation: Arc<Mutex<()>>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hup = signal(SignalKind::hangup()).unwrap();
        let mut term = signal(SignalKind::terminate()).unwrap();
        loop {
            tokio::select! {_=hup.recv()=>{let _guard=mutation.lock().await;if let Ok(c)=Config::load(&path){if let Err(e)=runtime.reload(c).await {tracing::error!("reload failed: {e}")}}},_=term.recv()=>break,r=tokio::signal::ctrl_c()=>{let _=r;break}}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_cursor_and_server_filter_compose() {
        let entries = vec![
            crate::upstream::LogEntry {
                id: 1,
                timestamp: 1,
                server: Some("a".into()),
                message: "one".into(),
            },
            crate::upstream::LogEntry {
                id: 2,
                timestamp: 2,
                server: Some("b".into()),
                message: "two".into(),
            },
            crate::upstream::LogEntry {
                id: 3,
                timestamp: 3,
                server: Some("a".into()),
                message: "three".into(),
            },
        ];
        let result = filter_logs(
            &entries,
            &LogQuery {
                after: Some(1),
                server: Some("a".into()),
            },
        );
        assert_eq!(result.iter().map(|e| e.id).collect::<Vec<_>>(), [3]);
    }
    #[tokio::test]
    async fn aggregate_endpoint_returns_gone() {
        let response = aggregate_removed().await.into_response();
        assert_eq!(response.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn protocol_2026_07_28_is_advertised() {
        let proxy = SessionProxy::new(
            Runtime::new(Config::default()).await,
            "unused".into(),
            ServerConfig {
                transport: crate::config::TransportConfig::Stdio {
                    command: "unused".into(),
                    args: Vec::new(),
                    env: Default::default(),
                },
                enabled: true,
                tags: Vec::new(),
            },
            Arc::new(SharedUpstream {
                upstream: Mutex::new(None),
                info: StdRwLock::new(None),
                bridge: Arc::new(BridgeState::new(true)),
            }),
        );
        let versions = proxy.supported_protocol_versions();
        assert!(versions.contains(&ProtocolVersion::LATEST));
        assert!(versions.contains(&ProtocolVersion::V_2026_07_28));
    }
}
