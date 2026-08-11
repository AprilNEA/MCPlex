use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, Request, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
};
use notify::{RecursiveMode, Watcher};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::{NotificationContext, RequestContext},
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
    sync::Arc,
    time::Duration,
    time::Instant,
};
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt as _;

use crate::{config::Config, secrets, upstream::Runtime};

#[derive(Clone)]
struct Mux {
    runtime: Arc<Runtime>,
    peers: Arc<RwLock<Vec<rmcp::Peer<RoleServer>>>>,
    filter: Option<String>,
}
impl Mux {
    fn owns(&self, server: &str) -> bool {
        self.filter.as_ref().is_none_or(|filter| filter == server)
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
impl ServerHandler for Mux {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ])
    }
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_prompts()
                .enable_prompts_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("mcplex", env!("CARGO_PKG_VERSION")))
    }
    async fn on_initialized(&self, c: NotificationContext<RoleServer>) {
        self.peers.write().await.push(c.peer)
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let catalog = self.runtime.catalog.read().await;
        Ok(ListToolsResult::with_all_items(
            catalog
                .tools
                .iter()
                .filter(|tool| {
                    catalog
                        .tool_routes
                        .get(tool.name.as_ref())
                        .is_some_and(|route| self.owns(&route.2))
                })
                .cloned()
                .collect(),
        ))
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let catalog = self.runtime.catalog.read().await;
        Ok(ListResourcesResult::with_all_items(
            catalog
                .resources
                .iter()
                .filter(|resource| {
                    catalog
                        .resource_routes
                        .get(&resource.uri)
                        .is_some_and(|route| self.owns(&route.2))
                })
                .cloned()
                .collect(),
        ))
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let catalog = self.runtime.catalog.read().await;
        Ok(ListPromptsResult::with_all_items(
            catalog
                .prompts
                .iter()
                .filter(|prompt| {
                    catalog
                        .prompt_routes
                        .get(&prompt.name)
                        .is_some_and(|route| self.owns(&route.2))
                })
                .cloned()
                .collect(),
        ))
    }
    async fn call_tool(
        &self,
        mut r: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let route = self
            .runtime
            .catalog
            .read()
            .await
            .tool_routes
            .get(r.name.as_ref())
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params("unknown tool", None))?;
        if !self.owns(&route.2) {
            return Err(ErrorData::invalid_params("unknown tool", None));
        }
        r.name = route.1.into();
        let id = route.2.clone();
        let start = Instant::now();
        let result = route
            .0
            .call_tool(r)
            .await
            .map(Into::into)
            .map_err(service_error);
        self.runtime
            .record_call(&id, start.elapsed().as_millis() as u64)
            .await;
        result
    }
    async fn get_prompt(
        &self,
        mut r: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let route = self
            .runtime
            .catalog
            .read()
            .await
            .prompt_routes
            .get(&r.name)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params("unknown prompt", None))?;
        if !self.owns(&route.2) {
            return Err(ErrorData::invalid_params("unknown prompt", None));
        }
        r.name = route.1;
        let id = route.2.clone();
        let start = Instant::now();
        let result = route
            .0
            .get_prompt(r)
            .await
            .map(Into::into)
            .map_err(service_error);
        self.runtime
            .record_call(&id, start.elapsed().as_millis() as u64)
            .await;
        result
    }
    async fn read_resource(
        &self,
        mut r: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let public = r.uri.clone();
        let route = self
            .runtime
            .catalog
            .read()
            .await
            .resource_routes
            .get(&public)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params("unknown resource", None))?;
        if !self.owns(&route.2) {
            return Err(ErrorData::invalid_params("unknown resource", None));
        }
        r.uri = route.1;
        let id = route.2.clone();
        let start = Instant::now();
        let response = route.0.read_resource(r).await.map_err(service_error);
        self.runtime
            .record_call(&id, start.elapsed().as_millis() as u64)
            .await;
        let mut result: ReadResourceResult = response?;
        for c in &mut result.contents {
            match c {
                ResourceContents::TextResourceContents { uri, .. }
                | ResourceContents::BlobResourceContents { uri, .. } => *uri = public.clone(),
                _ => {}
            }
        }
        Ok(result.into())
    }
}

type McpRouters = Arc<RwLock<HashMap<String, Router>>>;

fn mcp_router(
    runtime: Arc<Runtime>,
    peers: Arc<RwLock<Vec<rmcp::Peer<RoleServer>>>>,
    filter: Option<String>,
) -> Router {
    let mux = Mux {
        runtime,
        peers,
        filter,
    };
    let service = StreamableHttpService::new(
        move || Ok(mux.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_json_response(true),
    );
    Router::new().route_service("/", service)
}

async fn sync_mcp_routers(
    registry: &McpRouters,
    runtime: &Arc<Runtime>,
    peers: &Arc<RwLock<Vec<rmcp::Peer<RoleServer>>>>,
) {
    let ids: std::collections::HashSet<_> = runtime.config().await.servers.into_keys().collect();
    let mut routers = registry.write().await;
    routers.retain(|id, _| ids.contains(id));
    for id in ids {
        routers
            .entry(id.clone())
            .or_insert_with(|| mcp_router(runtime.clone(), peers.clone(), Some(id)));
    }
}

async fn dedicated_mcp(
    State(api): State<Api>,
    AxumPath(server): AxumPath<String>,
    mut request: Request<Body>,
) -> impl IntoResponse {
    let router = api.mcp_routers.read().await.get(&server).cloned();
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
async fn tools(State(s): State<Api>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth(&h, &s) {
        return e.into_response();
    }
    Json(json!(s.runtime.catalog.read().await.tools)).into_response()
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
    let peers = Arc::new(RwLock::new(Vec::new()));
    let mux = Mux {
        runtime: runtime.clone(),
        peers: peers.clone(),
        filter: None,
    };
    let mcp = StreamableHttpService::new(
        move || Ok(mux.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_json_response(true),
    );
    let registry: McpRouters = Arc::new(RwLock::new(HashMap::new()));
    sync_mcp_routers(&registry, &runtime, &peers).await;
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
        .route_service("/mcp", mcp)
        .route(
            "/mcp/{server}",
            post(dedicated_mcp).delete(dedicated_mcp).get(dedicated_mcp),
        )
        .route("/api/v1/status", get(status))
        .route("/api/v1/servers", get(status))
        .route("/api/v1/tools", get(tools))
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
    let peers_changes = peers.clone();
    tokio::spawn(async move {
        loop {
            match changes.recv().await {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
            sync_mcp_routers(&registry_changes, &runtime_changes, &peers_changes).await;
            let mut keep = Vec::new();
            let snapshot = peers.read().await.clone();
            for p in snapshot {
                let sent = tokio::time::timeout(Duration::from_secs(2), async {
                    p.notify_tool_list_changed().await?;
                    p.notify_resource_list_changed().await?;
                    p.notify_prompt_list_changed().await
                })
                .await;
                if matches!(sent, Ok(Ok(()))) {
                    keep.push(p.clone())
                }
            }
            *peers.write().await = keep;
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
    async fn modern_subscription_protocol_is_not_advertised() {
        let mux = Mux {
            runtime: Runtime::new(Config::default()).await,
            peers: Arc::new(RwLock::new(Vec::new())),
            filter: None,
        };
        let versions = mux.supported_protocol_versions();
        assert!(!versions.contains(&ProtocolVersion::V_2026_07_28));
        assert_eq!(versions.len(), 3);
        mux.runtime.shutdown().await;
    }
}
