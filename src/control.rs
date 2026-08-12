use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    config::{Config, default_path},
    secrets,
    upstream::{LogEntry, ServerStatus},
};

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StatusResponse {
    pub servers: BTreeMap<String, ServerStatus>,
}

#[derive(Debug, Deserialize)]
pub struct LogsResponse {
    pub logs: Vec<LogEntry>,
}

#[derive(Clone)]
pub struct ControlClient {
    endpoint: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl ControlClient {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = path.map_or_else(default_path, Ok)?;
        let config = if path.exists() {
            Config::load(&path)?
        } else {
            Config::default()
        };
        Ok(Self {
            endpoint: endpoint_for(std::net::SocketAddr::new(
                config.daemon.bind,
                config.daemon.port,
            )),
            token: secrets::control_token()?,
            http: reqwest::Client::new(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn request<T: DeserializeOwned>(&self, method: Method, route: &str) -> Result<T> {
        let response = self
            .http
            .request(method, format!("{}/{route}", self.endpoint))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("daemon is not reachable")?;
        let status = response.status();
        if !status.is_success() {
            let error = response
                .json::<ErrorResponse>()
                .await
                .map(|body| body.error)
                .unwrap_or_else(|_| "invalid error response".into());
            bail!("daemon returned {status}: {error}")
        }
        response.json().await.context("invalid daemon response")
    }

    pub async fn status(&self) -> Result<StatusResponse> {
        self.request(Method::GET, "status").await
    }
    pub async fn servers(&self) -> Result<StatusResponse> {
        self.request(Method::GET, "servers").await
    }
    pub async fn logs(&self, after: Option<u64>, server: Option<&str>) -> Result<LogsResponse> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(id) = after {
            query.append_pair("after", &id.to_string());
        }
        if let Some(server) = server {
            query.append_pair("server", server);
        }
        let query = query.finish();
        let suffix = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        self.request(Method::GET, &format!("logs{suffix}")).await
    }
    pub async fn reload(&self) -> Result<()> {
        self.action("reload").await
    }
    pub async fn enable(&self, id: &str) -> Result<()> {
        self.action(&format!("servers/{id}/enable")).await
    }
    pub async fn disable(&self, id: &str) -> Result<()> {
        self.action(&format!("servers/{id}/disable")).await
    }
    pub async fn restart(&self, id: &str) -> Result<()> {
        self.action(&format!("servers/{id}/restart")).await
    }
    async fn action(&self, route: &str) -> Result<()> {
        let _: serde_json::Value = self.request(Method::POST, route).await?;
        Ok(())
    }
}

fn endpoint_for(address: std::net::SocketAddr) -> String {
    format!("http://{address}/api/v1")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipv6_endpoint_is_bracketed() {
        assert_eq!(
            endpoint_for("[::1]:45850".parse().unwrap()),
            "http://[::1]:45850/api/v1"
        );
    }
}
