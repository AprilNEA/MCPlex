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
    ClientHandler, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
        ClientResult, ContentBlock, CustomRequest, CustomResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, Resource, ServerCapabilities, ServerInfo,
        ServerRequest, Tool,
    },
    service::{PeerRequestOptions, RequestContext},
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
                .enable_resources()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: vec![Tool::new(
                TOOL_NAME,
                format!("upstream session {}", self.session),
                serde_json::Map::new(),
            )],
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
        StreamableHttpServerConfig::default().with_json_response(true),
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
        reverse_tx,
    }
    .serve(StreamableHttpClientTransport::from_uri(endpoint))
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

    first
        .call_tool_once(CallToolRequestParams::new("reverse"))
        .await
        .unwrap();
    second
        .call_tool_once(CallToolRequestParams::new("reverse"))
        .await
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
    mcplex.abort();
    upstream.abort();
    unsafe { std::env::remove_var("MCPLEX_CONTROL_TOKEN") };
}
