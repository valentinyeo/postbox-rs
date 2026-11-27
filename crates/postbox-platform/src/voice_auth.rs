use crate::{EnvRegistry, SecretLoadOutcome, SecretStore};

pub const ENV_DEEPGRAM_API_KEY: &str = "DEEPGRAM_API_KEY";
pub const DEEPGRAM_API_KEY_SECRET_NAME: &str = "DeepgramApiKey";

pub const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
pub const ANTHROPIC_API_KEY_SECRET_NAME: &str = "AnthropicApiKey";
pub const ENV_ANTHROPIC_WORKSPACE_ID: &str = "ANTHROPIC_WORKSPACE_ID";
pub const ANTHROPIC_WORKSPACE_ID_SECRET_NAME: &str = "AnthropicWorkspaceId";

pub const ENV_OPENROUTER_API_KEY: &str = "OPENROUTER_API_KEY";
pub const OPENROUTER_API_KEY_SECRET_NAME: &str = "OpenRouterApiKey";
pub const ENV_OPENROUTER_MODEL: &str = "OPENROUTER_MODEL";
pub const OPENROUTER_MODEL_SECRET_NAME: &str = "OpenRouterModel";

pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.6-luna";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-5-5";
pub const MAX_API_KEY_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeyOutcome {
    Ready(String),
    Missing,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatProviderConfig {
    Anthropic {
        api_key: String,
        model: String,
        workspace_id: Option<String>,
    },
    OpenRouter {
        api_key: String,
        model: String,
    },
}

fn trim_api_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|c| matches!(c, ' ' | '\r' | '\n' | '\t'));
    if trimmed.is_empty() || trimmed.len() > MAX_API_KEY_BYTES {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn discover_seeded_secret(
    env_name: &str,
    secret_name: &str,
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> ApiKeyOutcome {
    let env_candidate = env_reg
        .load_user_env(env_name)
        .ok()
        .flatten()
        .and_then(|v| trim_api_key(&v))
        .or_else(|| process_env(env_name).and_then(|v| trim_api_key(&v)));

    if let Some(env_key) = env_candidate {
        let existing = match secrets.load_secret_with_outcome(secret_name) {
            Ok(Some(loaded)) if loaded.outcome == SecretLoadOutcome::MigrationFailed => {
                return ApiKeyOutcome::Error(format!(
                    "Stored {secret_name} failed DPAPI migration; refusing insecure fallback"
                ));
            }
            Ok(Some(loaded)) => trim_api_key(&loaded.value),
            Ok(None) => None,
            Err(_) => None,
        };
        if existing.as_deref() != Some(env_key.as_str())
            && let Err(err) = secrets.save_secret(secret_name, &env_key)
        {
            return ApiKeyOutcome::Error(format!("Failed to persist {secret_name}: {err}"));
        }
        return ApiKeyOutcome::Ready(env_key);
    }

    match secrets.load_secret_with_outcome(secret_name) {
        Ok(Some(loaded)) => {
            if loaded.outcome == SecretLoadOutcome::MigrationFailed {
                return ApiKeyOutcome::Error(format!(
                    "Stored {secret_name} failed DPAPI migration; refusing insecure fallback"
                ));
            }
            match trim_api_key(&loaded.value) {
                Some(key) => ApiKeyOutcome::Ready(key),
                None => ApiKeyOutcome::Missing,
            }
        }
        Ok(None) => ApiKeyOutcome::Missing,
        Err(err) => ApiKeyOutcome::Error(format!("Failed to read {secret_name}: {err}")),
    }
}

/// Discovers and seeds the Deepgram API key (`HKCU\Environment\DEEPGRAM_API_KEY` -> process env
/// -> DPAPI `DeepgramApiKey`, fail-closed on `MigrationFailed`).
pub fn discover_deepgram_api_key(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> ApiKeyOutcome {
    discover_seeded_secret(
        ENV_DEEPGRAM_API_KEY,
        DEEPGRAM_API_KEY_SECRET_NAME,
        env_reg,
        secrets,
        &process_env,
    )
}

pub fn discover_deepgram_api_key_from_system(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
) -> ApiKeyOutcome {
    discover_deepgram_api_key(env_reg, secrets, |name| std::env::var(name).ok())
}

/// Single `401`/`403` refresh check: reloads the Deepgram key and returns `Some(new_key)`
/// ONLY when a non-empty key was found AND it differs from `current_key`.
pub fn refresh_deepgram_api_key_if_changed(
    current_key: &str,
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    match discover_deepgram_api_key(env_reg, secrets, process_env) {
        ApiKeyOutcome::Ready(new_key) if !new_key.is_empty() && new_key != current_key => {
            Some(new_key)
        }
        _ => None,
    }
}

pub fn refresh_deepgram_api_key_if_changed_from_system(
    current_key: &str,
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
) -> Option<String> {
    refresh_deepgram_api_key_if_changed(current_key, env_reg, secrets, |name| {
        std::env::var(name).ok()
    })
}

/// Discovers optional ADHD LLM transcript formatting configuration.
/// Strictly dormant (returns `None`) unless the user explicitly provides either
/// `AnthropicApiKey` (takes precedence) or `OpenRouterApiKey`.
pub fn discover_format_provider(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> Option<FormatProviderConfig> {
    if let ApiKeyOutcome::Ready(api_key) = discover_seeded_secret(
        ENV_ANTHROPIC_API_KEY,
        ANTHROPIC_API_KEY_SECRET_NAME,
        env_reg,
        secrets,
        &process_env,
    ) {
        let workspace_id = match discover_seeded_secret(
            ENV_ANTHROPIC_WORKSPACE_ID,
            ANTHROPIC_WORKSPACE_ID_SECRET_NAME,
            env_reg,
            secrets,
            &process_env,
        ) {
            ApiKeyOutcome::Ready(ws) => Some(ws),
            _ => None,
        };
        return Some(FormatProviderConfig::Anthropic {
            api_key,
            model: DEFAULT_ANTHROPIC_MODEL.to_owned(),
            workspace_id,
        });
    }

    if let ApiKeyOutcome::Ready(api_key) = discover_seeded_secret(
        ENV_OPENROUTER_API_KEY,
        OPENROUTER_API_KEY_SECRET_NAME,
        env_reg,
        secrets,
        &process_env,
    ) {
        let model = match discover_seeded_secret(
            ENV_OPENROUTER_MODEL,
            OPENROUTER_MODEL_SECRET_NAME,
            env_reg,
            secrets,
            &process_env,
        ) {
            ApiKeyOutcome::Ready(m) => m,
            _ => DEFAULT_OPENROUTER_MODEL.to_owned(),
        };
        return Some(FormatProviderConfig::OpenRouter { api_key, model });
    }

    None
}

pub fn discover_format_provider_from_system(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
) -> Option<FormatProviderConfig> {
    discover_format_provider(env_reg, secrets, |name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryPlatform;

    #[test]
    fn deepgram_key_seeds_falls_back_and_refreshes_only_when_changed() {
        let plat = MemoryPlatform::default();
        plat.set_user_env(ENV_DEEPGRAM_API_KEY, "  dg-key-1 \r\n");

        assert_eq!(
            discover_deepgram_api_key(&plat, &plat, |_| None),
            ApiKeyOutcome::Ready("dg-key-1".into())
        );
        assert!(plat.has_encrypted_secret(DEEPGRAM_API_KEY_SECRET_NAME));

        // Unchanged -> refresh returns None.
        assert_eq!(
            refresh_deepgram_api_key_if_changed("dg-key-1", &plat, &plat, |_| None),
            None
        );

        // Remove env var -> loads from DPAPI.
        plat.remove_user_env(ENV_DEEPGRAM_API_KEY);
        assert_eq!(
            discover_deepgram_api_key(&plat, &plat, |_| None),
            ApiKeyOutcome::Ready("dg-key-1".into())
        );

        // New key in HKCU\Environment -> refresh returns Some(new_key).
        plat.set_user_env(ENV_DEEPGRAM_API_KEY, "dg-key-2");
        assert_eq!(
            refresh_deepgram_api_key_if_changed("dg-key-1", &plat, &plat, |_| None),
            Some("dg-key-2".into())
        );
    }

    #[test]
    fn deepgram_key_fails_closed_on_migration_failure() {
        let broken = MemoryPlatform::default();
        broken.set_legacy_plaintext_secret(DEEPGRAM_API_KEY_SECRET_NAME, "legacy-dg");
        broken.set_fail_migration(true);
        assert!(matches!(
            discover_deepgram_api_key(&broken, &broken, |_| None),
            ApiKeyOutcome::Error(_)
        ));
    }

    #[test]
    fn format_provider_is_dormant_by_default_and_prefers_anthropic_over_openrouter() {
        let plat = MemoryPlatform::default();
        assert_eq!(discover_format_provider(&plat, &plat, |_| None), None);

        plat.set_user_env(ENV_OPENROUTER_API_KEY, "or-key-1");
        assert_eq!(
            discover_format_provider(&plat, &plat, |_| None),
            Some(FormatProviderConfig::OpenRouter {
                api_key: "or-key-1".into(),
                model: DEFAULT_OPENROUTER_MODEL.into(),
            })
        );

        plat.set_user_env(ENV_ANTHROPIC_API_KEY, "sk-ant-123");
        plat.set_user_env(ENV_ANTHROPIC_WORKSPACE_ID, "ws-99");
        assert_eq!(
            discover_format_provider(&plat, &plat, |_| None),
            Some(FormatProviderConfig::Anthropic {
                api_key: "sk-ant-123".into(),
                model: DEFAULT_ANTHROPIC_MODEL.into(),
                workspace_id: Some("ws-99".into()),
            })
        );
    }
}
