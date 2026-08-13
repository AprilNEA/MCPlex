use std::path::Path;

#[cfg(any(target_os = "macos", test))]
use std::{fs, io::Write};

use anyhow::{Context, Result, bail};
#[cfg(any(target_os = "macos", test))]
use fs2::FileExt;
use rand::RngCore;

pub fn parse_keychain_reference(value: &str) -> Result<(&str, &str)> {
    let spec = value.strip_prefix("keychain:").unwrap_or(value);
    let Some((service, account)) = spec.split_once('/') else {
        bail!("secret reference must be service/account (optionally prefixed keychain:)")
    };
    if service.is_empty() || account.is_empty() || account.contains('/') {
        bail!("secret reference must contain one non-empty service and account")
    }
    Ok((service, account))
}

pub fn resolve(value: &str) -> Result<String> {
    if let Some(name) = value.strip_prefix("env:") {
        return std::env::var(name)
            .with_context(|| format!("environment variable {name} is not set"));
    }
    if let Some(spec) = value.strip_prefix("keychain:") {
        let (service, account) = parse_keychain_reference(spec)?;
        return keyring::Entry::new(service, account)?
            .get_password()
            .context("keychain lookup failed");
    }
    Ok(value.to_owned())
}

pub fn control_token(config_path: &Path) -> Result<String> {
    if let Ok(token) = std::env::var("MCPLEX_CONTROL_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    #[cfg(target_os = "macos")]
    {
        file_control_token(config_path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config_path;
        keyring_control_token()
    }
}

#[cfg(not(target_os = "macos"))]
fn keyring_control_token() -> Result<String> {
    let entry = keyring::Entry::new("mcplex", "control-token")?;
    match entry.get_password() {
        Ok(token) if !token.is_empty() => Ok(token),
        Ok(_) | Err(keyring::Error::NoEntry) => {
            let mut bytes = [0_u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            let token = hex::encode(bytes);
            entry
                .set_password(&token)
                .context("could not store control token")?;
            Ok(token)
        }
        Err(error) => Err(error).context("could not access control token"),
    }
}

#[cfg(any(target_os = "macos", test))]
fn file_control_token(config_path: &Path) -> Result<String> {
    let path = config_path.with_extension("token");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    let lock_path = config_path.with_extension("token.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&lock_path).with_context(|| {
        format!(
            "failed to open control token lock at {}",
            lock_path.display()
        )
    })?;
    set_private_permissions(&lock_path)?;
    lock.lock_exclusive()
        .context("failed to lock control token")?;

    let result = (|| {
        if path.exists() {
            set_private_permissions(&path)?;
            let token = fs::read_to_string(&path)
                .with_context(|| format!("failed to read control token at {}", path.display()))?;
            if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!(
                    "invalid control token at {}; remove it to regenerate",
                    path.display()
                )
            }
            return Ok(token);
        }

        let mut bytes = [0_u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        persist_private(&path, token.as_bytes())?;
        Ok(token)
    })();
    FileExt::unlock(&lock).context("failed to unlock control token")?;
    result
}

#[cfg(any(target_os = "macos", test))]
fn persist_private(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create control token in {}", parent.display()))?;
    temp.write_all(contents)?;
    set_private_permissions(temp.path())?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("failed to save control token at {}", path.display()))
}

#[cfg(any(target_os = "macos", test))]
fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_env() {
        unsafe { std::env::set_var("MCPLEX_TEST_SECRET", "resolved") };
        assert_eq!(resolve("env:MCPLEX_TEST_SECRET").unwrap(), "resolved");
        assert_eq!(resolve("plain").unwrap(), "plain");
        unsafe { std::env::remove_var("MCPLEX_TEST_SECRET") };
    }
    #[test]
    fn parses_keychain_references_without_accessing_keyring() {
        assert_eq!(
            parse_keychain_reference("svc/acct").unwrap(),
            ("svc", "acct")
        );
        assert_eq!(
            parse_keychain_reference("keychain:svc/acct").unwrap(),
            ("svc", "acct")
        );
        for invalid in ["svc", "/acct", "svc/", "svc/a/b"] {
            assert!(parse_keychain_reference(invalid).is_err());
        }
    }

    #[test]
    fn control_token_file_is_stable_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("custom.toml");

        let first = file_control_token(&config).unwrap();
        let second = file_control_token(&config).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(config.with_extension("token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
