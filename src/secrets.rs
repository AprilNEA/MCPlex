use anyhow::{Context, Result, bail};
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

pub fn control_token() -> Result<String> {
    if let Ok(token) = std::env::var("MCPLEX_CONTROL_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let entry = keyring::Entry::new("mcplex", "control-token")?;
    match entry.get_password() {
        Ok(token) if !token.is_empty() => Ok(token),
        Ok(_) | Err(keyring::Error::NoEntry) => {
            let mut bytes = [0_u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            entry
                .set_password(&token)
                .context("could not store control token")?;
            Ok(token)
        }
        Err(error) => Err(error).context("could not access control token"),
    }
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
}
