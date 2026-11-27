use crate::{EnvRegistry, SecretLoadOutcome, SecretStore};

pub const ENV_GIPHY_API_KEY: &str = "GIPHY_API_KEY";
pub const GIPHY_API_KEY_SECRET_NAME: &str = "GiphyApiKey";
pub const MAX_GIPHY_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GiphyKeyOutcome {
    Ready(String),
    Missing,
    Error(String),
}

fn trim_giphy_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|c| matches!(c, ' ' | '\r' | '\n' | '\t'));
    if trimmed.is_empty() || trimmed.len() > MAX_GIPHY_KEY_BYTES {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Discovers and seeds the GIPHY API key matching Zig `seedGiphyKey`/`loadGiphyKey`
/// (`main.zig:13650-13679`):
/// 1. Check `HKCU\Environment\GIPHY_API_KEY` (takes precedence per `ISSUE-517`), then process env.
/// 2. If present and valid (`1..=256` bytes after trim), seed/refresh DPAPI `GiphyApiKey`
///    only when missing or changed.
/// 3. Load from DPAPI `GiphyApiKey` (failing closed on `SecretLoadOutcome::MigrationFailed`).
pub fn discover_giphy_api_key(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> GiphyKeyOutcome {
    let env_candidate = env_reg
        .load_user_env(ENV_GIPHY_API_KEY)
        .ok()
        .flatten()
        .and_then(|v| trim_giphy_key(&v))
        .or_else(|| process_env(ENV_GIPHY_API_KEY).and_then(|v| trim_giphy_key(&v)));

    if let Some(env_key) = env_candidate {
        let existing = match secrets.load_secret_with_outcome(GIPHY_API_KEY_SECRET_NAME) {
            Ok(Some(loaded)) if loaded.outcome == SecretLoadOutcome::MigrationFailed => {
                return GiphyKeyOutcome::Error(
                    "Stored GiphyApiKey failed DPAPI migration; refusing insecure fallback"
                        .to_owned(),
                );
            }
            Ok(Some(loaded)) => trim_giphy_key(&loaded.value),
            Ok(None) => None,
            Err(_) => None,
        };
        if existing.as_deref() != Some(env_key.as_str())
            && let Err(err) = secrets.save_secret(GIPHY_API_KEY_SECRET_NAME, &env_key)
        {
            return GiphyKeyOutcome::Error(format!("Failed to persist GiphyApiKey: {err}"));
        }
        return GiphyKeyOutcome::Ready(env_key);
    }

    match secrets.load_secret_with_outcome(GIPHY_API_KEY_SECRET_NAME) {
        Ok(Some(loaded)) => {
            if loaded.outcome == SecretLoadOutcome::MigrationFailed {
                return GiphyKeyOutcome::Error(
                    "Stored GiphyApiKey failed DPAPI migration; refusing insecure fallback"
                        .to_owned(),
                );
            }
            match trim_giphy_key(&loaded.value) {
                Some(key) => GiphyKeyOutcome::Ready(key),
                None => GiphyKeyOutcome::Missing,
            }
        }
        Ok(None) => GiphyKeyOutcome::Missing,
        Err(err) => GiphyKeyOutcome::Error(format!("Failed to read GiphyApiKey: {err}")),
    }
}

pub fn discover_giphy_api_key_from_system(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
) -> GiphyKeyOutcome {
    discover_giphy_api_key(env_reg, secrets, |name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryPlatform;

    #[test]
    fn seeds_refreshes_and_falls_back_to_dpapi_giphy_key() {
        let plat = MemoryPlatform::default();
        plat.set_user_env(ENV_GIPHY_API_KEY, "  giphy-demo-key-1 \r\n");

        assert_eq!(
            discover_giphy_api_key(&plat, &plat, |_| None),
            GiphyKeyOutcome::Ready("giphy-demo-key-1".into())
        );
        assert!(plat.has_encrypted_secret(GIPHY_API_KEY_SECRET_NAME));

        // Remove env var -> loads from DPAPI store.
        plat.remove_user_env(ENV_GIPHY_API_KEY);
        assert_eq!(
            discover_giphy_api_key(&plat, &plat, |_| None),
            GiphyKeyOutcome::Ready("giphy-demo-key-1".into())
        );

        // Updated env var refreshes stored key.
        plat.set_user_env(ENV_GIPHY_API_KEY, "giphy-demo-key-2");
        assert_eq!(
            discover_giphy_api_key(&plat, &plat, |_| None),
            GiphyKeyOutcome::Ready("giphy-demo-key-2".into())
        );
        plat.remove_user_env(ENV_GIPHY_API_KEY);
        assert_eq!(
            discover_giphy_api_key(&plat, &plat, |_| None),
            GiphyKeyOutcome::Ready("giphy-demo-key-2".into())
        );
    }

    #[test]
    fn giphy_key_rejects_oversized_and_fails_closed_on_migration_failure() {
        let plat = MemoryPlatform::default();
        let huge = "a".repeat(MAX_GIPHY_KEY_BYTES + 1);
        plat.set_user_env(ENV_GIPHY_API_KEY, huge);
        assert_eq!(
            discover_giphy_api_key(&plat, &plat, |_| None),
            GiphyKeyOutcome::Missing
        );

        let broken = MemoryPlatform::default();
        broken.set_legacy_plaintext_secret(GIPHY_API_KEY_SECRET_NAME, "legacy-giphy");
        broken.set_fail_migration(true);
        assert!(matches!(
            discover_giphy_api_key(&broken, &broken, |_| None),
            GiphyKeyOutcome::Error(_)
        ));
    }
}
