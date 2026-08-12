use std::{collections::BTreeMap, fs, io::Write, net::IpAddr, path::Path};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 45_850;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub port: u16,
    pub bind: IpAddr,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            bind: IpAddr::from([127, 0, 0, 1]),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfig {
    #[serde(flatten)]
    pub transport: TransportConfig,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthConfig {
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum TransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<OAuthConfig>,
    },
}

const fn enabled_by_default() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config = config_rs::Config::builder()
            .add_source(
                config_rs::File::from(path)
                    .format(config_rs::FileFormat::Toml)
                    .required(true),
            )
            .build()
            .with_context(|| format!("failed to load config at {}", path.display()))?
            .try_deserialize::<Self>()
            .with_context(|| format!("invalid config at {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn parse(source: &str) -> Result<Self> {
        let config = config_rs::Config::builder()
            .add_source(config_rs::File::from_str(
                source,
                config_rs::FileFormat::Toml,
            ))
            .build()
            .context("invalid TOML")?
            .try_deserialize::<Self>()
            .context("invalid config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.daemon.bind.is_loopback() {
            bail!("daemon.bind must be a loopback address in v0");
        }

        for (id, server) in &self.servers {
            validate_server_id(id)?;

            match &server.transport {
                TransportConfig::Stdio { command, .. } if command.trim().is_empty() => {
                    bail!("server '{id}' has an empty command")
                }
                TransportConfig::Http { url, .. } => {
                    let parsed = url::Url::parse(url)
                        .with_context(|| format!("server '{id}' has an invalid URL"))?;
                    if !matches!(parsed.scheme(), "http" | "https") || !parsed.has_host() {
                        bail!("server '{id}' URL must be an absolute http or https URL")
                    }
                    if let TransportConfig::Http {
                        headers,
                        oauth: Some(_),
                        ..
                    } = &server.transport
                        && headers
                            .keys()
                            .any(|name| name.eq_ignore_ascii_case("authorization"))
                    {
                        bail!("server '{id}' cannot combine OAuth with an Authorization header")
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Atomically replace a configuration file without exposing a partially-written file.
pub fn persist_atomic(path: &Path, config: &Config) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let contents = toml::to_string_pretty(config)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".mcplex-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temporary config in {}", parent.display()))?;
    temp.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace config at {}", path.display()))
}

/// Serialize a read-modify-write transaction with all mcplex processes.
pub fn update_atomic<T>(path: &Path, update: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("toml.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open config lock at {}", lock_path.display()))?;
    lock.lock_exclusive().context("failed to lock config")?;
    let result = (|| {
        let mut config = if path.exists() {
            Config::load(path)?
        } else {
            Config::default()
        };
        let value = update(&mut config)?;
        config.validate()?;
        persist_atomic(path, &config)?;
        Ok(value)
    })();
    FileExt::unlock(&lock).context("failed to unlock config")?;
    result
}

pub fn default_path() -> Result<std::path::PathBuf> {
    let dirs = ProjectDirs::from("", "", "mcplex")
        .context("could not determine the user configuration directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn validate_server_id(id: &str) -> Result<()> {
    let valid = (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        bail!("server id '{id}' must match [a-z0-9-]{{1,32}}")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_endpoint() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.daemon.port, DEFAULT_PORT);
        assert_eq!(config.daemon.bind.to_string(), "127.0.0.1");
        assert!(config.servers.is_empty());
    }

    #[test]
    fn parses_stdio_and_http_servers() {
        let config = Config::parse(
            r#"
                [servers.github]
                transport = "stdio"
                command = "docker"
                args = ["run", "-i"]
                env = { TOKEN = "env:GITHUB_TOKEN" }

                [servers.linear]
                transport = "http"
                url = "https://mcp.linear.app/mcp"

                [servers.linear.oauth]
                scopes = ["read", "write"]
            "#,
        )
        .unwrap();

        assert_eq!(config.servers.len(), 2);
        assert!(config.servers.values().all(|server| server.enabled));
        let TransportConfig::Http {
            oauth: Some(oauth), ..
        } = &config.servers["linear"].transport
        else {
            panic!("linear should use OAuth")
        };
        assert_eq!(oauth.scopes, ["read", "write"]);
    }

    #[test]
    fn rejects_invalid_ids_and_non_loopback_binding() {
        let invalid_id = Config::parse(
            r#"
                [servers."Not Valid"]
                transport = "stdio"
                command = "example"
            "#,
        )
        .unwrap_err();
        assert!(invalid_id.to_string().contains("must match"));

        let public_bind = Config::parse("[daemon]\nbind = '0.0.0.0'").unwrap_err();
        assert!(public_bind.to_string().contains("loopback"));
    }

    #[test]
    fn rejects_unknown_fields_after_config_rs_extraction() {
        let error = Config::parse("[daemon]\nunknown = true").unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"));
    }

    #[test]
    fn validates_http_urls_with_url_parser() {
        for url in [
            "https://example.com/mcp",
            "HTTP://localhost:8080/mcp?client=mcplex",
            "http://[::1]:8080/mcp",
        ] {
            let source = format!(
                "[servers.remote]\ntransport='http'\nurl={}\n",
                toml::Value::String(url.into())
            );
            Config::parse(&source).unwrap();
        }
        for url in [
            "http://",
            "http://[::1",
            "http://localhost:99999",
            "/relative/mcp",
            "ftp://example.com/mcp",
        ] {
            let source = format!(
                "[servers.remote]\ntransport='http'\nurl={}\n",
                toml::Value::String(url.into())
            );
            assert!(
                Config::parse(&source).is_err(),
                "accepted invalid URL {url}"
            );
        }
    }

    #[test]
    fn rejects_oauth_with_manual_authorization_header() {
        let error = Config::parse(
            r#"
                [servers.remote]
                transport = "http"
                url = "https://example.com/mcp"
                headers = { Authorization = "env:TOKEN" }
                [servers.remote.oauth]
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot combine OAuth"));
    }

    #[test]
    fn rejects_removed_alias_field() {
        let error = Config::parse("[servers.a]\ntransport='stdio'\ncommand='x'\nalias='old-name'")
            .unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `alias`"));
    }

    #[cfg(unix)]
    #[test]
    fn persistence_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        persist_atomic(&path, &Config::default()).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn locked_updates_reload_the_latest_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config-without-extension");
        update_atomic(&path, |c| {
            c.daemon.port = 45851;
            Ok(())
        })
        .unwrap();
        update_atomic(&path, |c| {
            c.daemon.bind = "127.0.0.2".parse().unwrap();
            Ok(())
        })
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.daemon.port, 45851);
        assert_eq!(config.daemon.bind.to_string(), "127.0.0.2");
    }

    #[test]
    fn failed_update_preserves_config_and_releases_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        persist_atomic(&path, &Config::default()).unwrap();
        let original = fs::read(&path).unwrap();

        let result = update_atomic(&path, |config| -> Result<()> {
            config.daemon.port = 1234;
            bail!("rejected update")
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        update_atomic(&path, |config| {
            config.daemon.port = 45851;
            Ok(())
        })
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().daemon.port, 45851);
    }

    #[test]
    fn failed_persist_cleans_up_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(persist_atomic(&destination, &Config::default()).is_err());
        let entries = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [destination.file_name().unwrap()]);
    }
}
