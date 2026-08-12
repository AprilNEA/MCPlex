use anyhow::{Context, Result, bail};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, CredentialStore, OAuthState, StoredCredentials,
};

/// Persistent OAuth credentials scoped to one configured upstream and URL.
#[derive(Clone, Debug)]
pub struct KeychainCredentialStore {
    account: String,
}

impl KeychainCredentialStore {
    pub fn new(server_id: &str, url: &str) -> Self {
        Self {
            account: format!("oauth:{server_id}:{url}"),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new("mcplex", &self.account).map_err(|error| storage_error("open", error))
    }
}

#[async_trait::async_trait]
impl CredentialStore for KeychainCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match self.entry()?.get_password() {
            Ok(value) => serde_json::from_str(&value).map(Some).map_err(|error| {
                AuthError::InternalError(format!(
                    "OAuth credential storage contains invalid data: {error}"
                ))
            }),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(storage_error("read", error)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let value = serde_json::to_string(&credentials).map_err(|error| {
            AuthError::InternalError(format!("could not encode OAuth credentials: {error}"))
        })?;
        self.entry()?
            .set_password(&value)
            .map_err(|error| storage_error("write", error))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(storage_error("delete", error)),
        }
    }
}

fn storage_error(operation: &str, error: keyring::Error) -> AuthError {
    AuthError::InternalError(format!(
        "could not {operation} OAuth credentials in OS keyring: {error}"
    ))
}

pub async fn authorization_manager(server_id: &str, url: &str) -> Result<AuthorizationManager> {
    let mut manager = AuthorizationManager::new(url)
        .await
        .context("OAuth metadata initialization failed")?;
    manager.set_credential_store(KeychainCredentialStore::new(server_id, url));
    if !manager
        .initialize_from_store()
        .await
        .context("OAuth credential restoration failed")?
    {
        bail!("OAuth authorization required; run `mcplex auth login {server_id}`")
    }
    Ok(manager)
}

pub async fn authorization_state(server_id: &str, url: &str) -> Result<OAuthState> {
    let mut state = OAuthState::new(url, None)
        .await
        .context("OAuth metadata initialization failed")?;
    match &mut state {
        OAuthState::Unauthorized(manager) => {
            manager.set_credential_store(KeychainCredentialStore::new(server_id, url));
        }
        _ => unreachable!("new OAuth state must be unauthorized"),
    }
    Ok(state)
}

pub async fn clear(server_id: &str, url: &str) -> Result<()> {
    KeychainCredentialStore::new(server_id, url)
        .clear()
        .await
        .context("could not clear OAuth credentials")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_accounts_are_isolated_by_server_and_url() {
        let first = KeychainCredentialStore::new("linear", "https://mcp.linear.app/mcp");
        let second = KeychainCredentialStore::new("linear", "https://example.test/mcp");
        assert_ne!(first.account, second.account);
        assert!(first.account.contains("linear"));
    }
}
