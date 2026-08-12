# MCP protocol support

MCPlex targets a dedicated transparent gateway model. `/mcp/{server-id}` binds a
downstream session to an independent session with exactly one upstream. The old `/mcp`
aggregate endpoint has been removed. This one-to-one session ownership is a prerequisite
for transparent bidirectional features; sharing one upstream session among clients would
make request IDs, capabilities, cancellation, and server-to-client requests ambiguous.

The matrix distinguishes current behavior from the target rather than treating planned
passthrough as implemented.

| Area | Current status | Transparent-gateway target |
| --- | --- | --- |
| Endpoints | Dedicated `/mcp/{server-id}` only | Keep one upstream per downstream session; no aggregate endpoint. |
| Tools | Supported | Preserve upstream names and payloads unchanged. |
| Resources | Supported | Preserve upstream URIs and payloads unchanged. |
| Resource templates | Supported | Forward list requests and results unchanged. |
| Prompts | Supported | Preserve upstream names and payloads unchanged. |
| Authorization | Supported for HTTP upstreams | Continue OAuth 2.1 discovery, DCR, PKCE, resource binding, issuer validation, keyring persistence, refresh, and static-header alternatives. |
| Ping | Terminated locally | Transparent forwarding is not currently promised. |
| Cancellation and progress | Supported | Request IDs and progress tokens are translated independently in both directions. |
| Resource subscriptions/updates | Supported for negotiated versions | Legacy subscribe/unsubscribe is active. Modern `subscriptions/listen` bridging is implemented but the future 2026-07-28 revision is not advertised yet. |
| Completion | Supported | Forward `completion/complete`. |
| MCP logging | Supported | Forward level requests and log notifications. |
| Roots | Supported | Forward client roots and list-change notifications. |
| Sampling | Supported | Delegate upstream sampling requests to the bound client. |
| Elicitation | Supported | Delegate form and URL elicitation to the bound client. |
| Tasks | Supported | Forward task-augmented requests and notifications when negotiated. |
| Custom methods | Supported | Forward custom requests, results, and notifications in both directions. |
| Discovery | Prepared | Post-initialization forwarding exists, but the future protocol revision that standardizes startup discovery is not advertised. |
| Transports | Partial | Current: upstream stdio and Streamable HTTP; downstream Streamable HTTP. Legacy SSE and WebSocket are not implemented. |

Current support uses rmcp 3.1.2 and advertises revisions through its stable
`2025-11-25` latest version. The SDK also knows the future `2026-07-28` draft, but MCPlex
does not advertise it: transparent SEP-2260 request-stream association cannot yet cross
two independently terminated rmcp connections. Ping is intentionally terminated at each
local connection because it is a connection-liveness operation, not an application
request. Legacy SSE and WebSocket are not part of the current Streamable HTTP product
surface.
