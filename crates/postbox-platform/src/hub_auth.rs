use crate::{EnvRegistry, SecretLoadOutcome, SecretStore};

pub const HUB_TOKEN_SECRET_NAME: &str = "HubToken";
pub const ENV_POSTBOX_HUB_URL: &str = "POSTBOX_HUB_URL";
pub const ENV_WAZIG_HUB_URL: &str = "WAZIG_HUB_URL";
pub const ENV_POSTBOX_HUB_TOKEN: &str = "POSTBOX_HUB_TOKEN";
pub const ENV_WAZIG_HUB_TOKEN: &str = "WAZIG_HUB_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    CliFlag,
    PostboxEnv,
    WazigEnv,
    DpapiStore,
    DpapiMigrated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHubConfig {
    pub url: String,
    pub token: String,
    pub url_source: CredentialSource,
    pub token_source: CredentialSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubDiscoveryOutcome {
    Configured(DiscoveredHubConfig),
    UnconfiguredMissingUrl,
    UnconfiguredMissingToken { url: String },
    InvalidToken { url: String, reason: String },
    SecretStoreError { url: String, reason: String },
}

/// Validates Hub Bearer token bytes per `main.zig:25203` & `winhttp.zig:335`:
/// non-empty, every byte in printable non-whitespace ASCII `0x21..=0x7E` (no `\r`/`\n`/spaces).
pub fn valid_hub_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

fn trim_non_empty(raw: Option<String>) -> Option<String> {
    raw.and_then(|value| {
        let trimmed = value.trim_ascii();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

/// Per `ISSUE-517` (`environment_keys.zig:4-18`), a non-empty user environment value in
/// `HKCU\Environment` takes precedence over an inherited process environment variable.
fn choose_env_value(
    name: &str,
    env_reg: &dyn EnvRegistry,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Ok(Some(reg_val)) = env_reg.load_user_env(name) {
        let trimmed = reg_val.trim_ascii();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    trim_non_empty(process_env(name))
}

/// Discovers Hub URL and Bearer token matching Zig Postbox `configureHub` (`main.zig:25181-25205`):
///
/// 1. **URL**: `cli_url` -> `POSTBOX_HUB_URL` -> `WAZIG_HUB_URL` (each checking `HKCU\Environment` before process env).
/// 2. **Token**: `POSTBOX_HUB_TOKEN` -> `WAZIG_HUB_TOKEN` (each checking `HKCU\Environment` before process env).
///    - If found in env/registry: validate bytes (`0x21..=0x7E`), seed/refresh DPAPI `HKCU\Software\Messages\HubToken`.
///    - If unset/empty: load from DPAPI `HKCU\Software\Messages\HubToken` (with legacy `REG_SZ` migration;
///      fails closed on `SecretLoadOutcome::MigrationFailed`).
pub fn discover_hub_credentials(
    cli_url: Option<&str>,
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> HubDiscoveryOutcome {
    let (url, url_source) = if let Some(u) = trim_non_empty(cli_url.map(str::to_owned)) {
        (u, CredentialSource::CliFlag)
    } else if let Some(u) = choose_env_value(ENV_POSTBOX_HUB_URL, env_reg, &process_env) {
        (u, CredentialSource::PostboxEnv)
    } else if let Some(u) = choose_env_value(ENV_WAZIG_HUB_URL, env_reg, &process_env) {
        (u, CredentialSource::WazigEnv)
    } else {
        return HubDiscoveryOutcome::UnconfiguredMissingUrl;
    };

    let seed = if let Some(t) = choose_env_value(ENV_POSTBOX_HUB_TOKEN, env_reg, &process_env) {
        Some((t, CredentialSource::PostboxEnv))
    } else {
        choose_env_value(ENV_WAZIG_HUB_TOKEN, env_reg, &process_env)
            .map(|t| (t, CredentialSource::WazigEnv))
    };

    if let Some((token, token_source)) = seed {
        if !valid_hub_token(&token) {
            return HubDiscoveryOutcome::InvalidToken {
                url,
                reason: "Hub token contains invalid whitespace or non-ASCII characters".to_owned(),
            };
        }
        // Seed or refresh encrypted HubToken store (matching configureHub in main.zig:25192-25194).
        if let Err(err) = secrets.save_secret(HUB_TOKEN_SECRET_NAME, &token) {
            return HubDiscoveryOutcome::SecretStoreError {
                url,
                reason: format!("Failed to persist encrypted HubToken: {err}"),
            };
        }
        return HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
            url,
            token,
            url_source,
            token_source,
        });
    }

    match secrets.load_secret_with_outcome(HUB_TOKEN_SECRET_NAME) {
        Ok(None) => HubDiscoveryOutcome::UnconfiguredMissingToken { url },
        Ok(Some(loaded)) => match loaded.outcome {
            SecretLoadOutcome::MigrationFailed => HubDiscoveryOutcome::SecretStoreError {
                url,
                reason: "Legacy HubToken DPAPI migration failed; refusing plaintext credential"
                    .to_owned(),
            },
            SecretLoadOutcome::Encrypted | SecretLoadOutcome::Migrated => {
                let token = loaded.value.trim_ascii().to_owned();
                if !valid_hub_token(&token) {
                    return HubDiscoveryOutcome::InvalidToken {
                        url,
                        reason: "Stored HubToken contains invalid characters".to_owned(),
                    };
                }
                let token_source = if loaded.outcome == SecretLoadOutcome::Migrated {
                    CredentialSource::DpapiMigrated
                } else {
                    CredentialSource::DpapiStore
                };
                HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
                    url,
                    token,
                    url_source,
                    token_source,
                })
            }
        },
        Err(err) => HubDiscoveryOutcome::SecretStoreError {
            url,
            reason: format!("Failed to load HubToken from secret store: {err}"),
        },
    }
}

/// Convenience wrapper reading process environment via `std::env::var`.
pub fn discover_hub_credentials_from_system(
    cli_url: Option<&str>,
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
) -> HubDiscoveryOutcome {
    discover_hub_credentials(cli_url, env_reg, secrets, |name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryPlatform;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn hub_credential_discovery_priority_postbox_over_wazig_registry_over_process() {
        let plat = MemoryPlatform::default();
        // Process has stale WAZIG and POSTBOX values.
        let proc_env = env_map(&[
            (ENV_WAZIG_HUB_URL, "https://stale-wazig.example"),
            (ENV_POSTBOX_HUB_URL, "https://stale-postbox.example"),
            (ENV_WAZIG_HUB_TOKEN, "stale-wazig-tok"),
            (ENV_POSTBOX_HUB_TOKEN, "stale-postbox-tok"),
        ]);
        // HKCU\Environment has updated POSTBOX values (ISSUE-517: registry beats process env).
        plat.set_user_env(ENV_POSTBOX_HUB_URL, "  https://fresh-hkcu.example \r\n");
        plat.set_user_env(ENV_POSTBOX_HUB_TOKEN, " fresh-hkcu-tok \t");

        let outcome = discover_hub_credentials(None, &plat, &plat, &proc_env);
        assert_eq!(
            outcome,
            HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
                url: "https://fresh-hkcu.example".into(),
                token: "fresh-hkcu-tok".into(),
                url_source: CredentialSource::PostboxEnv,
                token_source: CredentialSource::PostboxEnv,
            })
        );
        // Verified DPAPI seed persisted.
        assert_eq!(
            plat.load_secret(HUB_TOKEN_SECRET_NAME).unwrap().as_deref(),
            Some("fresh-hkcu-tok")
        );

        // CLI flag beats everything for URL.
        let cli_outcome =
            discover_hub_credentials(Some("https://cli.example"), &plat, &plat, &proc_env);
        match cli_outcome {
            HubDiscoveryOutcome::Configured(cfg) => {
                assert_eq!(cfg.url, "https://cli.example");
                assert_eq!(cfg.url_source, CredentialSource::CliFlag);
            }
            other => panic!("expected Configured, got {other:?}"),
        }

        // Falling back to WAZIG_* when POSTBOX_* absent.
        let plat2 = MemoryPlatform::default();
        let wazig_env = env_map(&[
            (ENV_WAZIG_HUB_URL, "https://wazig.example"),
            (ENV_WAZIG_HUB_TOKEN, "wazig-token-123"),
        ]);
        let wazig_out = discover_hub_credentials(None, &plat2, &plat2, wazig_env);
        assert_eq!(
            wazig_out,
            HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
                url: "https://wazig.example".into(),
                token: "wazig-token-123".into(),
                url_source: CredentialSource::WazigEnv,
                token_source: CredentialSource::WazigEnv,
            })
        );
    }

    #[test]
    fn hub_token_seeds_dpapi_and_falls_back_when_env_removed() {
        let plat = MemoryPlatform::default();
        let initial_env = env_map(&[
            (ENV_WAZIG_HUB_URL, "https://hub.example"),
            (ENV_WAZIG_HUB_TOKEN, "seeded-secret-token"),
        ]);
        let first = discover_hub_credentials(None, &plat, &plat, initial_env);
        assert!(matches!(first, HubDiscoveryOutcome::Configured(_)));

        // Remove token env var; later launch reads encrypted DPAPI HubToken automatically.
        let later_env = env_map(&[(ENV_WAZIG_HUB_URL, "https://hub.example")]);
        let second = discover_hub_credentials(None, &plat, &plat, later_env);
        assert_eq!(
            second,
            HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
                url: "https://hub.example".into(),
                token: "seeded-secret-token".into(),
                url_source: CredentialSource::WazigEnv,
                token_source: CredentialSource::DpapiStore,
            })
        );

        // Changing env token refreshes DPAPI HubToken.
        let refreshed_env = env_map(&[
            (ENV_WAZIG_HUB_URL, "https://hub.example"),
            (ENV_POSTBOX_HUB_TOKEN, "rotated-secret-token"),
        ]);
        let third = discover_hub_credentials(None, &plat, &plat, refreshed_env);
        assert!(matches!(
            third,
            HubDiscoveryOutcome::Configured(DiscoveredHubConfig { ref token, .. })
                if token == "rotated-secret-token"
        ));
        assert_eq!(
            plat.load_secret(HUB_TOKEN_SECRET_NAME).unwrap().as_deref(),
            Some("rotated-secret-token")
        );
    }

    #[test]
    fn hub_token_legacy_plaintext_migration_and_invalid_bytes_rejected() {
        // Legacy plaintext REG_SZ migrates cleanly to encrypted storage.
        let plat = MemoryPlatform::default();
        plat.set_legacy_plaintext_secret(HUB_TOKEN_SECRET_NAME, "legacy-dpapi-token");
        let env = env_map(&[(ENV_WAZIG_HUB_URL, "https://hub.example")]);
        let migrated = discover_hub_credentials(None, &plat, &plat, &env);
        assert_eq!(
            migrated,
            HubDiscoveryOutcome::Configured(DiscoveredHubConfig {
                url: "https://hub.example".into(),
                token: "legacy-dpapi-token".into(),
                url_source: CredentialSource::WazigEnv,
                token_source: CredentialSource::DpapiMigrated,
            })
        );

        // Migration failure MUST fail closed (never use unencrypted credential).
        let broken = MemoryPlatform::default();
        broken.set_legacy_plaintext_secret(HUB_TOKEN_SECRET_NAME, "legacy-dpapi-token");
        broken.set_fail_migration(true);
        let fail_closed = discover_hub_credentials(None, &broken, &broken, &env);
        assert!(matches!(
            fail_closed,
            HubDiscoveryOutcome::SecretStoreError { .. }
        ));

        // Token with interior spaces, CR/LF, or non-ASCII rejected.
        for bad in ["has space", "line\r\nbreak", "tok\x00nul", "ümlaut", ""] {
            assert!(!valid_hub_token(bad), "expected invalid: {bad:?}");
        }
        assert!(valid_hub_token("Abc_123-!~#$%^&*()+="));

        let bad_env = env_map(&[
            (ENV_POSTBOX_HUB_URL, "https://hub.example"),
            (ENV_POSTBOX_HUB_TOKEN, "bad\r\ntoken"),
        ]);
        let invalid = discover_hub_credentials(None, &plat, &plat, bad_env);
        assert!(matches!(invalid, HubDiscoveryOutcome::InvalidToken { .. }));

        // Missing URL / missing token outcomes.
        let empty_plat = MemoryPlatform::default();
        let empty_env = env_map(&[]);
        assert_eq!(
            discover_hub_credentials(None, &empty_plat, &empty_plat, &empty_env),
            HubDiscoveryOutcome::UnconfiguredMissingUrl
        );
        assert_eq!(
            discover_hub_credentials(
                Some("https://hub.example"),
                &empty_plat,
                &empty_plat,
                &empty_env
            ),
            HubDiscoveryOutcome::UnconfiguredMissingToken {
                url: "https://hub.example".into(),
            }
        );
    }
}
