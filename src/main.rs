use std::{collections::BTreeMap, fs, io::Read, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use mcplex::config::{
    Config, OAuthConfig, ServerConfig, TransportConfig, default_path, persist_atomic,
    update_atomic, validate_server_id,
};
use mcplex::control::{ControlClient, StatusResponse};
use rmcp::{ServiceExt, model::Tool, transport::StreamableHttpClientTransport};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Use a config file other than the platform default.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local MCP gateway daemon.
    Serve {
        /// Keep the process attached to this terminal.
        #[arg(long)]
        foreground: bool,
    },
    Status,
    Ls {
        #[arg(long, requires = "server")]
        tools: bool,
        /// List tools from one dedicated server endpoint.
        #[arg(long, requires = "tools")]
        server: Option<String>,
    },
    /// Add a server to the configuration.
    Add(AddArgs),
    /// Remove a server from the configuration.
    Rm {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Import {
        path: Option<PathBuf>,
    },
    Snippet {
        client: Client,
        /// Generate a dedicated endpoint for one configured server.
        #[arg(long)]
        server: String,
    },
    /// Manage secrets in the OS keyring.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Authorize OAuth 2.1 HTTP upstreams.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Logs {
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long)]
        server: Option<String>,
    },
    Reload,
    Doctor,
    Tui,
}

#[derive(Debug, clap::Args)]
struct AddArgs {
    id: String,
    #[arg(long)]
    command: Option<String>,
    #[arg(long)]
    url: Option<String>,
    #[arg(long, requires = "command")]
    arg: Vec<String>,
    #[arg(long, requires = "command")]
    env: Vec<String>,
    #[arg(long, requires = "url")]
    header: Vec<String>,
    /// Enable OAuth 2.1 authorization for an HTTP upstream.
    #[arg(long, requires = "url")]
    oauth: bool,
    /// OAuth scope to request (repeatable; server defaults are used when omitted).
    #[arg(long, requires = "oauth")]
    scope: Vec<String>,
    #[arg(long)]
    tag: Vec<String>,
    #[arg(long)]
    disabled: bool,
}

#[derive(Debug, Subcommand)]
enum SecretCommand {
    /// Store a secret, prompting without echo by default.
    Set {
        reference: String,
        #[arg(long)]
        stdin: bool,
    },
    /// Remove a secret.
    Rm { reference: String },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Open a browser and authorize one configured OAuth upstream.
    Login { id: String },
    /// Delete the stored OAuth credentials for one upstream.
    Logout { id: String },
}

#[derive(Clone, Debug, ValueEnum)]
enum Client {
    ClaudeCode,
    Cursor,
    ClaudeDesktop,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { .. } => mcplex::server::serve_path(cli.config).await,
        Command::Import { path } => import_config(path, cli.config).map(|count| {
            eprintln!("imported {count} MCP server(s)");
        }),
        Command::Snippet { client, server } => {
            let path = cli.config.map_or_else(default_path, Ok)?;
            let config = if path.exists() {
                Config::load(&path)?
            } else {
                Config::default()
            };
            validate_server_id(&server)?;
            if !config.servers.contains_key(&server) {
                bail!("unknown server '{server}'");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&snippet(&client, &config, &server))?
            );
            Ok(())
        }
        Command::Status => ControlClient::load(cli.config)?
            .status()
            .await
            .map(|v| print_status(&v)),
        Command::Ls { tools, server } => {
            if tools {
                let server = server.context("--tools requires --server ID")?;
                list_tools(cli.config, &server).await
            } else {
                let client = ControlClient::load(cli.config)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&client.servers().await?)?
                );
                Ok(())
            }
        }
        Command::Logs { follow, server } => logs(cli.config, follow, server).await,
        Command::Reload => ControlClient::load(cli.config)?
            .reload()
            .await
            .map(|()| println!("reloaded")),
        Command::Enable { id } => ControlClient::load(cli.config)?
            .enable(&id)
            .await
            .map(|()| println!("enabled {id}")),
        Command::Disable { id } => ControlClient::load(cli.config)?
            .disable(&id)
            .await
            .map(|()| println!("disabled {id}")),
        Command::Add(args) => edit_add(cli.config, args).await,
        Command::Rm { id } => edit_remove(cli.config, &id).await,
        Command::Secret { command } => secret(command),
        Command::Auth { command } => oauth(command, cli.config).await,
        Command::Doctor => doctor(cli.config).await,
        Command::Tui => mcplex::tui::run(cli.config).await,
    }
}

async fn list_tools(path: Option<PathBuf>, server: &str) -> Result<()> {
    let tools = fetch_tools(path, server).await?;
    println!("{}", serde_json::to_string_pretty(&tools)?);
    Ok(())
}

async fn fetch_tools(path: Option<PathBuf>, server: &str) -> Result<Vec<Tool>> {
    validate_server_id(server)?;
    let config_path = path.map_or_else(default_path, Ok)?;
    let config = Config::load(&config_path)?;
    let configured = config
        .servers
        .get(server)
        .with_context(|| format!("unknown server '{server}'"))?;
    if !configured.enabled {
        bail!("server '{server}' is disabled")
    }
    let endpoint = format!(
        "http://{}/mcp/{server}",
        socket_authority(config.daemon.bind, config.daemon.port)
    );
    let service = ().serve(StreamableHttpClientTransport::from_uri(endpoint)).await?;
    let tools = service.list_all_tools().await?;
    service.cancel().await?;
    Ok(tools)
}

fn socket_authority(bind: std::net::IpAddr, port: u16) -> String {
    match bind {
        std::net::IpAddr::V4(address) => format!("{address}:{port}"),
        std::net::IpAddr::V6(address) => format!("[{address}]:{port}"),
    }
}

fn print_status(v: &StatusResponse) {
    for (id, s) in &v.servers {
        println!(
            "{id:20} {:10} tools={} resources={} prompts={}",
            format!("{:?}", s.state).to_lowercase(),
            s.tools,
            s.resources,
            s.prompts
        );
    }
}
async fn logs(path: Option<PathBuf>, follow: bool, server: Option<String>) -> Result<()> {
    let client = ControlClient::load(path)?;
    let mut after = None;
    loop {
        let entries = client.logs(after, server.as_deref()).await?.logs;
        for entry in entries {
            after = Some(entry.id);
            println!("{} {}", entry.timestamp, entry.message)
        }
        if !follow {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn parse_pairs(values: &[String]) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let Some((key, value)) = value.split_once('=') else {
                bail!("'{value}' must be KEY=VALUE")
            };
            if key.trim().is_empty() {
                bail!("key must not be empty")
            }
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn server_from_args(args: &AddArgs) -> Result<ServerConfig> {
    validate_server_id(&args.id)?;
    let transport = match (&args.command, &args.url) {
        (Some(command), None) => TransportConfig::Stdio {
            command: command.clone(),
            args: args.arg.clone(),
            env: parse_pairs(&args.env)?,
        },
        (None, Some(url)) => TransportConfig::Http {
            url: url.clone(),
            headers: parse_pairs(&args.header)?,
            oauth: args.oauth.then(|| OAuthConfig {
                scopes: args.scope.clone(),
            }),
        },
        _ => bail!("exactly one of --command or --url is required"),
    };
    Ok(ServerConfig {
        transport,
        enabled: !args.disabled,
        tags: args.tag.clone(),
    })
}

async fn reload_if_reachable(path: Option<PathBuf>) -> Result<()> {
    let client = match ControlClient::load(path) {
        Ok(client) => client,
        Err(_) => return Ok(()),
    };
    match client.reload().await {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("not reachable") => Ok(()),
        Err(error) => Err(error).context("config saved, but reachable daemon rejected reload"),
    }
}
async fn edit_add(path: Option<PathBuf>, args: AddArgs) -> Result<()> {
    let destination = path.clone().map_or_else(default_path, Ok)?;
    update_atomic(&destination, |config| {
        if config.servers.contains_key(&args.id) {
            bail!("server '{}' already exists", args.id)
        }
        config
            .servers
            .insert(args.id.clone(), server_from_args(&args)?);
        Ok(())
    })?;
    reload_if_reachable(path).await?;
    println!("added {}", args.id);
    Ok(())
}
async fn edit_remove(path: Option<PathBuf>, id: &str) -> Result<()> {
    validate_server_id(id)?;
    let destination = path.clone().map_or_else(default_path, Ok)?;
    update_atomic(&destination, |config| {
        if config.servers.remove(id).is_none() {
            bail!("unknown server '{id}'")
        }
        Ok(())
    })?;
    reload_if_reachable(path).await?;
    println!("removed {id}");
    Ok(())
}
fn secret(command: SecretCommand) -> Result<()> {
    match command {
        SecretCommand::Set { reference, stdin } => {
            let (service, account) = mcplex::secrets::parse_keychain_reference(&reference)?;
            let mut value = if stdin {
                let mut value = String::new();
                std::io::stdin().read_to_string(&mut value)?;
                value
            } else {
                rpassword::prompt_password("Secret: ")?
            };
            while value.ends_with(['\n', '\r']) {
                value.pop();
            }
            if value.is_empty() {
                bail!("secret value must not be empty")
            }
            keyring::Entry::new(service, account)?
                .set_password(&value)
                .context("could not store secret")?;
            println!("secret stored");
        }
        SecretCommand::Rm { reference } => {
            let (service, account) = mcplex::secrets::parse_keychain_reference(&reference)?;
            match keyring::Entry::new(service, account)?.delete_credential() {
                Ok(()) => println!("secret removed"),
                Err(keyring::Error::NoEntry) => println!("secret was not present"),
                Err(error) => return Err(error).context("could not remove secret"),
            }
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct OAuthCallback {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone)]
struct OAuthCallbackState {
    sender: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<OAuthCallback>>>>,
}

async fn oauth_callback(
    axum::extract::State(state): axum::extract::State<OAuthCallbackState>,
    axum::extract::Query(callback): axum::extract::Query<OAuthCallback>,
) -> axum::response::Html<&'static str> {
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(callback);
    }
    axum::response::Html("OAuth authorization received. You can close this window.")
}

fn oauth_server(config: &Config, id: &str) -> Result<(String, OAuthConfig)> {
    validate_server_id(id)?;
    let server = config
        .servers
        .get(id)
        .with_context(|| format!("unknown server '{id}'"))?;
    match &server.transport {
        TransportConfig::Http {
            url,
            oauth: Some(oauth),
            ..
        } => Ok((url.clone(), oauth.clone())),
        TransportConfig::Http { .. } => bail!("server '{id}' does not have OAuth enabled"),
        TransportConfig::Stdio { .. } => bail!("server '{id}' is not an HTTP upstream"),
    }
}

async fn restart_if_reachable(path: Option<PathBuf>, id: &str) -> Result<()> {
    let client = ControlClient::load(path)?;
    match client.restart(id).await {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("not reachable") => Ok(()),
        Err(error) => Err(error).context("OAuth credentials changed, but daemon restart failed"),
    }
}

async fn oauth(command: AuthCommand, path: Option<PathBuf>) -> Result<()> {
    let config_path = path.clone().map_or_else(default_path, Ok)?;
    let config = Config::load(&config_path).with_context(|| {
        format!(
            "OAuth requires a configured server in {}",
            config_path.display()
        )
    })?;
    match command {
        AuthCommand::Logout { id } => {
            let (url, _) = oauth_server(&config, &id)?;
            mcplex::oauth::clear(&id, &url).await?;
            restart_if_reachable(path, &id).await?;
            println!("OAuth credentials removed for {id}");
        }
        AuthCommand::Login { id } => {
            let (url, oauth_config) = oauth_server(&config, &id)?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .context("could not bind OAuth callback listener")?;
            let callback_url = format!("http://{}/oauth/callback", listener.local_addr()?);
            let mut oauth = mcplex::oauth::authorization_state(&id, &url).await?;
            let request = rmcp::transport::auth::AuthorizationRequest::new(&callback_url)
                .with_client_name("MCPlex")
                .with_scopes(oauth_config.scopes);
            oauth
                .start_authorization(request)
                .await
                .context("could not start OAuth authorization")?;
            let authorization_url = oauth.get_authorization_url().await?;

            let (sender, receiver) = tokio::sync::oneshot::channel();
            let state = OAuthCallbackState {
                sender: std::sync::Arc::new(tokio::sync::Mutex::new(Some(sender))),
            };
            let app = axum::Router::new()
                .route("/oauth/callback", axum::routing::get(oauth_callback))
                .with_state(state);
            let callback_server = tokio::spawn(async move { axum::serve(listener, app).await });

            println!("Open this URL to authorize {id}:\n{authorization_url}");
            if let Err(error) = webbrowser::open(&authorization_url) {
                tracing::warn!(%error, "could not open a browser automatically");
            }
            let callback = tokio::time::timeout(std::time::Duration::from_secs(300), receiver)
                .await
                .context("OAuth authorization timed out after five minutes")?
                .context("OAuth callback listener stopped")?;
            callback_server.abort();

            if let Some(error) = callback.error {
                bail!(
                    "OAuth provider returned {error}: {}",
                    callback.error_description.unwrap_or_default()
                )
            }
            let code = callback.code.context("OAuth callback omitted code")?;
            let state = callback.state.context("OAuth callback omitted state")?;
            oauth
                .handle_callback_with_issuer(&code, &state, callback.iss.as_deref())
                .await
                .context("OAuth callback validation or token exchange failed")?;
            restart_if_reachable(path, &id).await?;
            println!("OAuth authorization stored for {id}");
        }
    }
    Ok(())
}
async fn doctor(path: Option<PathBuf>) -> Result<()> {
    let p = path.clone().map_or_else(default_path, Ok)?;
    let config = if p.exists() {
        Config::load(&p)
    } else {
        Ok(Config::default())
    };
    match config {
        Ok(c) => println!(
            "ok config: {} server(s), loopback {}:{}",
            c.servers.len(),
            c.daemon.bind,
            c.daemon.port
        ),
        Err(e) => bail!("config: {e}; fix {}", p.display()),
    }
    println!(
        "ok path: {}{}",
        p.display(),
        if p.exists() {
            ""
        } else {
            " (missing; defaults are valid)"
        }
    );
    mcplex::secrets::control_token(&p)
        .context("control token unavailable; configure MCPLEX_CONTROL_TOKEN or fix its storage")?;
    let v = ControlClient::load(path)?.status().await?;
    println!(
        "ok control token: {}",
        if std::env::var("MCPLEX_CONTROL_TOKEN").is_ok() {
            "MCPLEX_CONTROL_TOKEN"
        } else if cfg!(target_os = "macos") {
            "private file"
        } else {
            "OS keyring"
        }
    );
    println!("ok daemon: reachable");
    let mut failed = false;
    for (id, status) in &v.servers {
        match status.state {
            mcplex::upstream::State::Idle | mcplex::upstream::State::Ready => {
                match fetch_tools(Some(p.clone()), id).await {
                    Ok(tools) => println!(
                        "ok {id}: dedicated endpoint connected ({} tools)",
                        tools.len()
                    ),
                    Err(error) => {
                        failed = true;
                        println!("fail {id}: {error:#}");
                    }
                }
            }
            mcplex::upstream::State::Disabled => {
                println!("info {id}: disabled; run `mcplex enable {id}` when needed")
            }
            mcplex::upstream::State::Degraded => {
                failed = true;
                println!(
                    "fail {id}: degraded; check `mcplex logs --server {id}` and its command/URL"
                )
            }
            mcplex::upstream::State::Starting => {
                failed = true;
                println!("fail {id}: still starting; check `mcplex logs --server {id}`")
            }
        }
    }
    if failed {
        bail!("doctor found required checks that failed")
    }
    Ok(())
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ClaudeServer>,
}

#[derive(Deserialize)]
struct ClaudeServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn normalize_id(name: &str) -> String {
    let mut id = String::new();
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            id.push(character);
        } else if !id.ends_with('-') {
            id.push('-');
        }
        if id.len() == 32 {
            break;
        }
    }
    id.trim_matches('-').to_owned()
}

fn import_candidates() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(".mcp.json")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".config/Claude/claude_desktop_config.json"));
        paths.push(home.join("Library/Application Support/Claude/claude_desktop_config.json"));
        paths.push(home.join("AppData/Roaming/Claude/claude_desktop_config.json"));
    }
    paths
}

fn import_config(source: Option<PathBuf>, destination: Option<PathBuf>) -> Result<usize> {
    let sources = source.map_or_else(import_candidates, |path| vec![path]);
    let destination = destination.map_or_else(default_path, Ok)?;
    let mut config = if destination.exists() {
        Config::load(&destination)?
    } else {
        Config::default()
    };
    let mut imported = 0;
    for source in sources.into_iter().filter(|path| path.exists()) {
        let input: ClaudeConfig = serde_json::from_str(
            &fs::read_to_string(&source)
                .with_context(|| format!("failed to read {}", source.display()))?,
        )
        .with_context(|| format!("invalid Claude MCP JSON at {}", source.display()))?;
        for (name, entry) in input.mcp_servers {
            let id = normalize_id(&name);
            validate_server_id(&id)
                .with_context(|| format!("cannot normalize imported server id '{name}'"))?;
            let Some(command) = entry.command else {
                eprintln!("skipping unsupported non-stdio server '{name}'");
                continue;
            };
            if config.servers.contains_key(&id) {
                eprintln!("skipping existing server '{id}'");
                continue;
            }
            config.servers.insert(
                id,
                ServerConfig {
                    transport: TransportConfig::Stdio {
                        command,
                        args: entry.args,
                        env: entry.env,
                    },
                    enabled: true,
                    tags: Vec::new(),
                },
            );
            imported += 1;
        }
    }
    config.validate()?;
    persist_atomic(&destination, &config)?;
    Ok(imported)
}

fn endpoint(config: &Config, server: &str) -> String {
    format!(
        "http://{}/mcp/{server}",
        std::net::SocketAddr::new(config.daemon.bind, config.daemon.port)
    )
}

fn snippet(client: &Client, config: &Config, server: &str) -> Value {
    let url = endpoint(config, server);
    match client {
        Client::ClaudeCode => json!({"mcpServers":{"mcplex":{"type":"http","url":url}}}),
        Client::Cursor => json!({"mcpServers":{"mcplex":{"url":url}}}),
        Client::ClaudeDesktop => {
            json!({"mcpServers":{"mcplex":{"command":"npx","args":["-y","mcp-remote",url]}}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn snippet_requires_a_server() {
        assert!(Cli::try_parse_from(["mcplex", "snippet", "claude-code"]).is_err());
        let cli = Cli::try_parse_from(["mcplex", "snippet", "claude-code", "--server", "github"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Command::Snippet { server, .. } if server == "github"
        ));
    }

    #[test]
    fn key_value_parsing_preserves_equals_and_rejects_bad_keys() {
        let map = parse_pairs(&["TOKEN=a=b".into()]).unwrap();
        assert_eq!(map["TOKEN"], "a=b");
        assert!(parse_pairs(&["missing".into()]).is_err());
        assert!(parse_pairs(&["=value".into()]).is_err());
    }

    #[test]
    fn add_constructs_both_transports_and_requires_exactly_one() {
        let mut args = AddArgs {
            id: "demo".into(),
            command: Some("node".into()),
            url: None,
            arg: vec!["x".into()],
            env: vec!["A=B".into()],
            header: vec![],
            oauth: false,
            scope: vec![],
            tag: vec!["work".into()],
            disabled: true,
        };
        assert!(matches!(
            server_from_args(&args).unwrap().transport,
            TransportConfig::Stdio { .. }
        ));
        args.command = None;
        args.url = Some("https://example.test/mcp".into());
        args.arg.clear();
        args.env.clear();
        args.header.push("Authorization=env:TOKEN".into());
        assert!(matches!(
            server_from_args(&args).unwrap().transport,
            TransportConfig::Http { .. }
        ));
        args.command = Some("x".into());
        assert!(server_from_args(&args).is_err());
    }

    #[test]
    fn snippets_have_client_specific_valid_json_shapes() {
        let mut config = Config::default();
        config.servers.insert(
            "github".into(),
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
        assert_eq!(
            snippet(&Client::ClaudeCode, &config, "github")["mcpServers"]["mcplex"]["type"],
            "http"
        );
        assert!(
            snippet(&Client::Cursor, &config, "github")["mcpServers"]["mcplex"]
                .get("type")
                .is_none()
        );
        assert_eq!(
            snippet(&Client::ClaudeDesktop, &config, "github")["mcpServers"]["mcplex"]["command"],
            "npx"
        );
        assert_eq!(
            snippet(&Client::ClaudeCode, &config, "github")["mcpServers"]["mcplex"]["url"],
            "http://127.0.0.1:45850/mcp/github"
        );
    }

    #[test]
    fn import_round_trip_preserves_plain_environment() {
        let root = std::env::temp_dir().join(format!("mcplex-test-{}", std::process::id()));
        let source = root.join("claude.json");
        let destination = root.join("nested/config.toml");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source, r#"{"mcpServers":{"My Server":{"command":"node","args":["x"],"env":{"TOKEN":"plain"}}}}"#).unwrap();
        assert_eq!(
            import_config(Some(source), Some(destination.clone())).unwrap(),
            1
        );
        let config = Config::load(&destination).unwrap();
        let TransportConfig::Stdio { env, .. } = &config.servers["my-server"].transport else {
            panic!()
        };
        assert_eq!(env["TOKEN"], "plain");
        assert_eq!(
            import_config(Some(root.join("claude.json")), Some(destination)).unwrap(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }
}
