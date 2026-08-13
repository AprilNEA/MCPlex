#![allow(deprecated)]

use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use mcplex::config::{Config, DaemonConfig, ServerConfig, TransportConfig};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient, RoleServer, ServerHandler,
    ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ClientCapabilities, ClientInfo, ClientRequest, ClientResult, ContentBlock,
        CreateTaskResult, CustomRequest, CustomResult, DetailedTask, GetTaskParams, GetTaskResult,
        Implementation, InputRequest, InputRequiredResult, ListResourcesResult, ListRootsRequest,
        ListRootsResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, Resource,
        ServerCapabilities, ServerInfo, ServerNotification, ServerRequest, SubscriptionFilter,
        Task, TaskPayload, TaskStatus, Tool, UpdateTaskParams,
    },
    service::{PeerRequestOptions, RequestContext, SubscriptionContext},
    transport::{
        StreamableHttpClientTransport,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use tokio::sync::mpsc;

const TOOL_NAME: &str = "UPPER.case/tool:verbatim";
const RESOURCE_URI: &str = "custom+scheme://Host/Mixed/Path?x=A%2FB#Fragment";

fn tool(session: usize) -> Tool {
    Tool::new(
        TOOL_NAME,
        format!("upstream session {session}"),
        serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(serde_json::json!({
            "type": "object",
            "properties": {
                "region": {
                    "type": "string",
                    "x-mcp-header": "Region"
                }
            }
        }))
        .unwrap(),
    )
}

#[derive(Clone)]
struct MockUpstream {
    session: usize,
    started_tx: mpsc::UnboundedSender<usize>,
    cancelled_tx: mpsc::UnboundedSender<usize>,
}

#[derive(Clone)]
struct DownstreamClient {
    label: &'static str,
    reverse_tx: mpsc::UnboundedSender<&'static str>,
}

impl ClientHandler for DownstreamClient {
    #[allow(deprecated)]
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::builder()
                .enable_roots()
                .enable_tasks()
                .build(),
            Implementation::new(self.label, "1.0.0"),
        )
    }

    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::ErrorData> {
        self.reverse_tx
            .send(self.label)
            .expect("reverse request receiver alive");
        Ok(ListRootsResult::new(Vec::new()))
    }

    async fn on_custom_request(
        &self,
        _: CustomRequest,
        _: RequestContext<RoleClient>,
    ) -> Result<CustomResult, rmcp::ErrorData> {
        self.reverse_tx
            .send(self.label)
            .expect("reverse request receiver alive");
        Ok(CustomResult::new(
            serde_json::json!({ "client": self.label }),
        ))
    }
}

impl ServerHandler for MockUpstream {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_tasks()
                .build(),
        )
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == TOOL_NAME).then(|| tool(self.session))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), rmcp::ErrorData> {
        let client = context
            .request_context()
            .client_info()
            .ok_or_else(|| rmcp::ErrorData::invalid_request("missing client info", None))?;
        if client.name != "modern-first" {
            return Err(rmcp::ErrorData::invalid_request(
                "subscription client metadata was not preserved",
                None,
            ));
        }
        context
            .sink()
            .notify_tool_list_changed()
            .await
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        context.cancelled().await;
        Ok(())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: vec![tool(self.session)],
            ..Default::default()
        })
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![Resource::new(RESOURCE_URI, "verbatim resource")],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        if request.name == "cancel" {
            self.started_tx
                .send(self.session)
                .expect("started receiver alive");
            context.ct.cancelled().await;
            self.cancelled_tx
                .send(self.session)
                .expect("cancelled receiver alive");
            return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text("cancelled"),
            ])));
        }
        if request.name == "mrtr" {
            let client = context
                .client_info()
                .ok_or_else(|| rmcp::ErrorData::invalid_request("missing client info", None))?;
            let state = format!("state-for-{}", client.name);
            if request.input_responses.is_none() {
                let requests = BTreeMap::from([(
                    "roots".to_owned(),
                    InputRequest::ListRoots(ListRootsRequest::default()),
                )]);
                return Ok(CallToolResponse::InputRequired(InputRequiredResult::new(
                    Some(requests),
                    Some(state),
                )));
            }
            if request.request_state.as_deref() != Some(state.as_str())
                || !request
                    .input_responses
                    .as_ref()
                    .is_some_and(|responses| responses.contains_key("roots"))
            {
                return Err(rmcp::ErrorData::invalid_params(
                    "MRTR response or request state was not preserved",
                    None,
                ));
            }
            return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(format!("MRTR complete for {}", client.name)),
            ])));
        }
        if request.name == "task" {
            let client = context
                .client_info()
                .ok_or_else(|| rmcp::ErrorData::invalid_request("missing client info", None))?;
            return Ok(CallToolResponse::Task(CreateTaskResult::new(
                Task::new(
                    format!("task-for-{}", client.name),
                    TaskStatus::Working,
                    "2026-08-12T00:00:00Z",
                    "2026-08-12T00:00:00Z",
                )
                .with_ttl_ms(60_000)
                .with_poll_interval_ms(50),
            )));
        }
        if request.name == TOOL_NAME {
            let region = context
                .extensions
                .get::<http::request::Parts>()
                .and_then(|parts| parts.headers.get("mcp-param-region"))
                .and_then(|value| value.to_str().ok());
            if region != Some("us-east-1") {
                return Err(rmcp::ErrorData::invalid_params(
                    "Mcp-Param-Region was not preserved",
                    None,
                ));
            }
            return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text("parameter header preserved"),
            ])));
        }
        let result = context
            .peer
            .send_request(ServerRequest::CustomRequest(CustomRequest::new(
                "test/reverse",
                None,
            )))
            .await
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        let ClientResult::CustomResult(result) = result else {
            return Err(rmcp::ErrorData::internal_error(
                "unexpected reverse response",
                None,
            ));
        };
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(result.0.to_string()),
        ])))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, rmcp::ErrorData> {
        let task = Task::new(
            request.task_id,
            TaskStatus::Completed,
            "2026-08-12T00:00:00Z",
            "2026-08-12T00:00:01Z",
        );
        Ok(GetTaskResult::new(DetailedTask::new(
            task,
            TaskPayload::Completed {
                result: serde_json::Map::from_iter([("content".to_owned(), serde_json::json!([]))]),
            },
        )))
    }

    async fn update_task(
        &self,
        _: UpdateTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        Ok(())
    }

    async fn cancel_task(
        &self,
        _: CancelTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        Ok(())
    }
}

async fn wait_until_ready(base: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(format!("{base}/healthz"))
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mcplex did not become ready");
}

#[tokio::test]
async fn dedicated_endpoint_keeps_downstream_sessions_isolated_and_names_verbatim() {
    // This integration-test binary has a single test, so no other test can race this
    // process-global override. It also avoids dependence on the host keychain.
    unsafe { std::env::set_var("MCPLEX_CONTROL_TOKEN", "dedicated-endpoint-test") };

    let (session_tx, mut session_rx) = mpsc::unbounded_channel();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let next_session = Arc::new(AtomicUsize::new(0));
    let upstream_service = StreamableHttpService::new(
        {
            let next_session = next_session.clone();
            move || {
                let session = next_session.fetch_add(1, Ordering::SeqCst) + 1;
                session_tx.send(session).expect("session receiver alive");
                Ok(MockUpstream {
                    session,
                    started_tx: started_tx.clone(),
                    cancelled_tx: cancelled_tx.clone(),
                })
            }
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let upstream_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            axum::Router::new().nest_service("/mcp", upstream_service),
        )
        .await
    });

    let mcplex_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = mcplex_listener.local_addr().unwrap().port();
    drop(mcplex_listener);
    let config = Config {
        daemon: DaemonConfig {
            port,
            bind: IpAddr::from([127, 0, 0, 1]),
        },
        servers: BTreeMap::from([
            (
                "verbatim-id".into(),
                ServerConfig {
                    transport: TransportConfig::Http {
                        url: format!("http://{upstream_addr}/mcp"),
                        headers: BTreeMap::new(),
                        oauth: None,
                    },
                    enabled: true,
                    tags: vec![],
                },
            ),
            (
                "disabled-id".into(),
                ServerConfig {
                    transport: TransportConfig::Http {
                        url: format!("http://{upstream_addr}/mcp"),
                        headers: BTreeMap::new(),
                        oauth: None,
                    },
                    enabled: false,
                    tags: vec![],
                },
            ),
        ]),
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    mcplex::config::persist_atomic(&path, &config).unwrap();
    let mcplex = tokio::spawn(mcplex::server::serve(config, path));
    let base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&base).await;

    assert_eq!(
        reqwest::get(format!("{base}/mcp")).await.unwrap().status(),
        reqwest::StatusCode::GONE
    );
    for id in ["unknown-id", "disabled-id"] {
        assert_eq!(
            reqwest::get(format!("{base}/mcp/{id}"))
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    let endpoint = format!("{base}/mcp/verbatim-id");
    assert_eq!(
        reqwest::Client::new()
            .get(&endpoint)
            .header("origin", "https://attacker.example")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let (reverse_tx, mut reverse_rx) = mpsc::unbounded_channel();
    let first = DownstreamClient {
        label: "first",
        reverse_tx: reverse_tx.clone(),
    }
    .serve(StreamableHttpClientTransport::from_uri(endpoint.clone()))
    .await
    .unwrap();
    let second = DownstreamClient {
        label: "second",
        reverse_tx: reverse_tx.clone(),
    }
    .serve(StreamableHttpClientTransport::from_uri(endpoint.clone()))
    .await
    .unwrap();

    let initialized = tokio::time::timeout(Duration::from_secs(5), async {
        (
            session_rx.recv().await.unwrap(),
            session_rx.recv().await.unwrap(),
        )
    })
    .await
    .expect("two independent upstream sessions were not initialized");
    assert_ne!(initialized.0, initialized.1);
    assert_eq!(next_session.load(Ordering::SeqCst), 2);

    let tools = first.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, TOOL_NAME);
    let resources = first.list_all_resources().await.unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, RESOURCE_URI);

    tokio::time::timeout(
        Duration::from_secs(5),
        first.call_tool_once(CallToolRequestParams::new("reverse")),
    )
    .await
    .expect("first legacy reverse request timed out")
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        second.call_tool_once(CallToolRequestParams::new("reverse")),
    )
    .await
    .expect("second legacy reverse request timed out")
    .unwrap();
    assert_eq!(reverse_rx.recv().await, Some("first"));
    assert_eq!(reverse_rx.recv().await, Some("second"));
    assert!(reverse_rx.try_recv().is_err());

    let cancellation = first
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                "cancel",
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let cancelled_session = tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
        .await
        .expect("upstream cancellation request did not start")
        .unwrap();
    cancellation
        .cancel(Some("integration test".into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), cancelled_rx.recv())
            .await
            .expect("cancellation did not cross the gateway"),
        Some(cancelled_session)
    );

    first.cancel().await.unwrap();
    let second_tools = tokio::time::timeout(Duration::from_secs(5), second.list_all_tools())
        .await
        .expect("second session stopped responding after first closed")
        .unwrap();
    assert_eq!(second_tools[0].name, TOOL_NAME);
    assert_eq!(next_session.load(Ordering::SeqCst), 2);

    second.cancel().await.unwrap();

    let http = reqwest::Client::new();
    let incomplete = http
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/list")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "missing-capabilities",
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(incomplete.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(
        incomplete
            .text()
            .await
            .unwrap()
            .contains("clientCapabilities")
    );
    assert_eq!(next_session.load(Ordering::SeqCst), 2);

    let unsupported = http
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2099-01-01")
        .header("mcp-method", "tasks/get")
        .header("mcp-name", "future-task")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "unsupported-version",
            "method": "tasks/get",
            "params": {
                "taskId": "future-task",
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2099-01-01",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported.status(), reqwest::StatusCode::BAD_REQUEST);
    let unsupported = unsupported.text().await.unwrap();
    assert!(unsupported.contains("Unsupported protocol version"));
    assert!(unsupported.contains("2026-07-28"));
    assert_eq!(next_session.load(Ordering::SeqCst), 2);

    // Opening with initialize selects the legacy lifecycle even when the client
    // offers the modern revision. A dual-era endpoint negotiates it down.
    let legacy_offer = http
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "initialize")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "legacy-offer",
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "legacy-offer", "version": "1.0.0" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy_offer.status(), reqwest::StatusCode::OK);
    let legacy_session = legacy_offer
        .headers()
        .get("mcp-session-id")
        .expect("initialize should open a legacy session")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(legacy_offer.text().await.unwrap().contains("2025-11-25"));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), session_rx.recv())
            .await
            .expect("legacy offer did not initialize its upstream"),
        Some(3)
    );
    assert_eq!(next_session.load(Ordering::SeqCst), 3);
    assert_eq!(
        http.delete(&endpoint)
            .header("mcp-session-id", legacy_session)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::ACCEPTED
    );

    // Discovery is optional in 2026-07-28. Capability-gated methods must work
    // as the first modern request, and clientInfo remains optional.
    let direct_task = http
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tasks/get")
        .header("mcp-name", "direct-task")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "direct-task",
            "method": "tasks/get",
            "params": {
                "taskId": "direct-task",
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {
                            "io.modelcontextprotocol/tasks": {}
                        }
                    }
                }
            }
        }))
        .send()
        .await
        .unwrap();
    let direct_task_status = direct_task.status();
    assert!(direct_task.headers().get("mcp-session-id").is_none());
    let direct_task_body = direct_task.text().await.unwrap();
    assert_eq!(
        direct_task_status,
        reqwest::StatusCode::OK,
        "{direct_task_body}"
    );
    assert!(direct_task_body.contains("direct-task"));
    tokio::time::timeout(Duration::from_secs(5), async {
        session_rx.recv().await.unwrap();
        session_rx.recv().await.unwrap();
    })
    .await
    .expect("direct modern request did not discover and call its upstream");
    assert_eq!(next_session.load(Ordering::SeqCst), 5);

    let parameter_header = http
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", TOOL_NAME)
        .header("mcp-param-region", "us-east-1")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "parameter-header",
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "region": "us-east-1" },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(parameter_header.status(), reqwest::StatusCode::OK);
    assert!(
        parameter_header
            .text()
            .await
            .unwrap()
            .contains("parameter header preserved")
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        session_rx.recv().await.unwrap();
        session_rx.recv().await.unwrap();
    })
    .await
    .expect("parameter-header request did not reach upstream");
    assert_eq!(next_session.load(Ordering::SeqCst), 7);

    let modern_first = tokio::time::timeout(
        Duration::from_secs(5),
        DownstreamClient {
            label: "modern-first",
            reverse_tx: reverse_tx.clone(),
        }
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_uri(endpoint.clone()),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        ),
    )
    .await
    .expect("first 2026 discovery timed out")
    .unwrap();
    let modern_second = tokio::time::timeout(
        Duration::from_secs(5),
        DownstreamClient {
            label: "modern-second",
            reverse_tx: reverse_tx.clone(),
        }
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_uri(endpoint.clone()),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        ),
    )
    .await
    .expect("second 2026 discovery timed out")
    .unwrap();
    assert_eq!(
        modern_first.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    assert_eq!(next_session.load(Ordering::SeqCst), 7);
    assert!(session_rx.try_recv().is_err());

    // The discover lifecycle may select an older application protocol while
    // retaining self-contained request metadata and stateless routing.
    let modern_older_revision = tokio::time::timeout(
        Duration::from_secs(5),
        DownstreamClient {
            label: "modern-older-revision",
            reverse_tx: reverse_tx.clone(),
        }
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_uri(endpoint),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![
                    ProtocolVersion::V_2025_11_25,
                    ProtocolVersion::V_2026_07_28,
                ],
            },
        ),
    )
    .await
    .expect("discover lifecycle with an older application revision timed out")
    .unwrap();
    assert_eq!(
        modern_older_revision.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2025_11_25
    );
    assert_eq!(
        modern_older_revision.list_all_tools().await.unwrap()[0].name,
        TOOL_NAME
    );
    tokio::time::timeout(Duration::from_secs(5), session_rx.recv())
        .await
        .expect("older negotiated revision did not reach the shared upstream")
        .expect("upstream request observer closed");
    assert!(session_rx.try_recv().is_err());
    assert_eq!(next_session.load(Ordering::SeqCst), 8);

    let (first_mrtr, second_mrtr) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            modern_first.call_tool(CallToolRequestParams::new("mrtr")),
            modern_second.call_tool(CallToolRequestParams::new("mrtr")),
        )
    })
    .await
    .expect("concurrent 2026 MRTR requests timed out");
    let first_mrtr = first_mrtr.unwrap();
    let second_mrtr = second_mrtr.unwrap();
    assert!(
        serde_json::to_string(&first_mrtr)
            .unwrap()
            .contains("modern-first")
    );
    assert!(
        serde_json::to_string(&second_mrtr)
            .unwrap()
            .contains("modern-second")
    );
    let mut routed = vec![
        reverse_rx.recv().await.unwrap(),
        reverse_rx.recv().await.unwrap(),
    ];
    routed.sort_unstable();
    assert_eq!(routed, ["modern-first", "modern-second"]);

    let task = match modern_first
        .call_tool_once(CallToolRequestParams::new("task"))
        .await
        .unwrap()
    {
        CallToolResponse::Task(task) => task,
        other => panic!("expected task result, got {other:?}"),
    };
    assert_eq!(task.task.task_id, "task-for-modern-first");
    let task_id = task.task.task_id;
    let task = modern_first
        .get_task(GetTaskParams::new(task_id.clone()))
        .await
        .unwrap();
    assert_eq!(task.task.task.task_id, task_id);
    assert_eq!(task.task.status(), TaskStatus::Completed);
    modern_first
        .update_task(UpdateTaskParams::new(task_id.clone(), BTreeMap::new()))
        .await
        .unwrap();
    modern_first
        .cancel_task(CancelTaskParams::new(task_id))
        .await
        .unwrap();

    let mut subscription = modern_first
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(subscription.acknowledged().tools_list_changed, Some(true));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await
            .expect("2026 subscription notification timed out")
            .unwrap(),
        Some(ServerNotification::ToolListChangedNotification(_))
    ));
    subscription.cancel().await.unwrap();

    let cancellation = modern_second
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                "cancel",
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let cancelled_session = tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
        .await
        .expect("2026 cancellation request did not start")
        .unwrap();
    cancellation
        .cancel(Some("2026 integration test".into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), cancelled_rx.recv())
            .await
            .expect("2026 stream cancellation did not cross the gateway"),
        Some(cancelled_session)
    );
    assert_eq!(
        modern_second.list_all_tools().await.unwrap()[0].name,
        TOOL_NAME
    );

    modern_first.cancel().await.unwrap();
    modern_second.cancel().await.unwrap();
    modern_older_revision.cancel().await.unwrap();
    mcplex.abort();
    upstream.abort();
    unsafe { std::env::remove_var("MCPLEX_CONTROL_TOKEN") };
}
