use std::{collections::BTreeMap, fs, io::Read, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use mcplex::config::{
    Config, ServerConfig, TransportConfig, default_path, persist_atomic, update_atomic,
    validate_server_id,
};
use mcplex::control::{ControlClient, StatusResponse};
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
    /// Run the local multiplexer daemon.
    Serve {
        /// Keep the process attached to this terminal.
        #[arg(long)]
        foreground: bool,
    },
    Status,
    Ls {
        #[arg(long)]
        tools: bool,
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
        server: Option<String>,
    },
    /// Manage secrets in the OS keyring.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
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
    #[arg(long)]
    alias: Option<String>,
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
        Command::Serve { .. } => {
            let path = cli.config.map_or_else(default_path, Ok)?;
            let config = if path.exists() {
                Config::load(&path)?
            } else {
                tracing::info!(path = %path.display(), "config not found; using defaults");
                Config::default()
            };
            mcplex::server::serve(config, path).await
        }
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
            if let Some(id) = &server {
                validate_server_id(id)?;
                if !config.servers.contains_key(id) {
                    bail!("unknown server '{id}'");
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&snippet(&client, &config, server.as_deref()))?
            );
            Ok(())
        }
        Command::Status => ControlClient::load(cli.config)?
            .status()
            .await
            .map(|v| print_status(&v)),
        Command::Ls { tools } => {
            let client = ControlClient::load(cli.config)?;
            if tools {
                println!("{}", serde_json::to_string_pretty(&client.tools().await?)?);
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&client.servers().await?)?
                );
            }
            Ok(())
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
        Command::Doctor => doctor(cli.config).await,
        Command::Tui => mcplex::tui::run(cli.config).await,
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
        },
        _ => bail!("exactly one of --command or --url is required"),
    };
    Ok(ServerConfig {
        transport,
        alias: args.alias.clone(),
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
    mcplex::secrets::control_token()
        .context("control token unavailable; configure MCPLEX_CONTROL_TOKEN or keyring")?;
    let v = ControlClient::load(path)?.status().await?;
    println!(
        "ok control token: {}",
        if std::env::var("MCPLEX_CONTROL_TOKEN").is_ok() {
            "MCPLEX_CONTROL_TOKEN"
        } else {
            "OS keyring"
        }
    );
    println!("ok daemon: reachable");
    for (id, status) in &v.servers {
        match status.state {
            mcplex::upstream::State::Disabled => {
                println!("info {id}: disabled; run `mcplex enable {id}` when needed")
            }
            mcplex::upstream::State::Degraded => println!(
                "fail {id}: degraded; check `mcplex logs --server {id}` and its command/URL"
            ),
            mcplex::upstream::State::Starting => {
                println!("fail {id}: still starting; check `mcplex logs --server {id}`")
            }
            mcplex::upstream::State::Ready => println!(
                "ok {id}: ready ({} tools, {} resources, {} prompts)",
                status.tools, status.resources, status.prompts
            ),
        }
    }
    if v.servers.values().any(|s| {
        matches!(
            s.state,
            mcplex::upstream::State::Degraded | mcplex::upstream::State::Starting
        )
    }) {
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
                    alias: None,
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

fn endpoint(config: &Config, server: Option<&str>) -> String {
    let suffix = server.map_or_else(|| "/mcp".to_owned(), |id| format!("/mcp/{id}"));
    format!(
        "http://{}{suffix}",
        std::net::SocketAddr::new(config.daemon.bind, config.daemon.port)
    )
}

fn snippet(client: &Client, config: &Config, server: Option<&str>) -> Value {
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
            alias: Some("d".into()),
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
                alias: None,
                enabled: false,
                tags: vec![],
            },
        );
        assert_eq!(
            snippet(&Client::ClaudeCode, &config, None)["mcpServers"]["mcplex"]["type"],
            "http"
        );
        assert!(
            snippet(&Client::Cursor, &config, None)["mcpServers"]["mcplex"]
                .get("type")
                .is_none()
        );
        assert_eq!(
            snippet(&Client::ClaudeDesktop, &config, None)["mcpServers"]["mcplex"]["command"],
            "npx"
        );
        assert_eq!(
            snippet(&Client::ClaudeCode, &config, Some("github"))["mcpServers"]["mcplex"]["url"],
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
