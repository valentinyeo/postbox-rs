use crate::{EnvRegistry, OsRegistry, PlatformError, SecretLoadOutcome, SecretStore};
use postbox_core::dictation::{DictationLanguage, truncate_mic_name};

pub const ENV_DEEPGRAM_API_KEY: &str = "DEEPGRAM_API_KEY";
pub const DEEPGRAM_API_KEY_SECRET_NAME: &str = "DeepgramApiKey";
pub const REG_DICTATION_LANGUAGE: &str = "DictationLanguage";
pub const REG_MICROPHONE: &str = "Microphone";

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

/// Decision action matching Zig `deepgram_key.zig:3-20`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepgramKeyAction {
    None,
    Load,
    Save,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramKeyLoad {
    pub key: Option<String>,
    pub action: DeepgramKeyAction,
    pub stored_outcome: Option<SecretLoadOutcome>,
    pub seed_saved: bool,
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
    let trimmed = raw.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    if trimmed.is_empty() || trimmed.len() > MAX_API_KEY_BYTES {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Exact decision table from Zig `deepgram_key.zig:3-20`:
/// - `(None, None)` -> `(None, None)`
/// - `(None, Some(seed))` -> `(Some(seed), Save)`
/// - `(Some(stored), None)` -> `(Some(stored), Load)`
/// - `(Some(stored), Some(seed))` if `seed == stored` -> `(Some(stored), Load)`
/// - `(Some(stored), Some(seed))` if `seed != stored` -> `(Some(seed), Update)`
pub fn decide_deepgram_key<'a>(
    stored: Option<&'a str>,
    seed: Option<&'a str>,
) -> (Option<&'a str>, DeepgramKeyAction) {
    match (stored, seed) {
        (None, None) => (None, DeepgramKeyAction::None),
        (None, Some(seed)) => (Some(seed), DeepgramKeyAction::Save),
        (Some(stored), None) => (Some(stored), DeepgramKeyAction::Load),
        (Some(stored), Some(seed)) if seed == stored => (Some(stored), DeepgramKeyAction::Load),
        (Some(_), Some(seed)) => (Some(seed), DeepgramKeyAction::Update),
    }
}

/// Detailed Deepgram API key loader mirroring `winhttp.zig:169-193` while strictly preserving
/// fail-closed legacy `REG_SZ` DPAPI migration semantics.
pub fn load_deepgram_key_detailed(
    env_reg: &dyn EnvRegistry,
    secrets: &dyn SecretStore,
    process_env: impl Fn(&str) -> Option<String>,
) -> Result<DeepgramKeyLoad, String> {
    let seed = env_reg
        .load_user_env(ENV_DEEPGRAM_API_KEY)
        .ok()
        .flatten()
        .and_then(|v| trim_api_key(&v))
        .or_else(|| process_env(ENV_DEEPGRAM_API_KEY).and_then(|v| trim_api_key(&v)));

    let (stored, stored_outcome) = match secrets
        .load_secret_with_outcome(DEEPGRAM_API_KEY_SECRET_NAME)
    {
        Ok(Some(loaded)) => {
            if loaded.outcome == SecretLoadOutcome::MigrationFailed {
                return Err(format!(
                    "Stored {DEEPGRAM_API_KEY_SECRET_NAME} failed DPAPI migration; refusing insecure fallback"
                ));
            }
            (trim_api_key(&loaded.value), Some(loaded.outcome))
        }
        Ok(None) => (None, None),
        Err(err) => {
            if seed.is_none() {
                return Err(format!(
                    "Failed to read {DEEPGRAM_API_KEY_SECRET_NAME}: {err}"
                ));
            }
            (None, None)
        }
    };

    let (chosen, action) = decide_deepgram_key(stored.as_deref(), seed.as_deref());
    let chosen_owned = chosen.map(str::to_owned);
    let mut seed_saved = false;

    if matches!(action, DeepgramKeyAction::Save | DeepgramKeyAction::Update)
        && let Some(key) = chosen_owned.as_deref()
    {
        secrets
            .save_secret(DEEPGRAM_API_KEY_SECRET_NAME, key)
            .map_err(|err| format!("Failed to persist {DEEPGRAM_API_KEY_SECRET_NAME}: {err}"))?;
        seed_saved = true;
    }

    Ok(DeepgramKeyLoad {
        key: chosen_owned,
        action,
        stored_outcome,
        seed_saved,
    })
}

/// Loads `DictationLanguage` from `HKCU\Software\Messages\DictationLanguage` (`REG_DWORD`),
/// defaulting to `Automatic` when missing or `> 2`.
pub fn load_dictation_language(reg: &dyn OsRegistry) -> DictationLanguage {
    reg.load_dword(REG_DICTATION_LANGUAGE)
        .ok()
        .flatten()
        .map(DictationLanguage::from_registry_dword)
        .unwrap_or(DictationLanguage::Automatic)
}

/// Persists `DictationLanguage` as `REG_DWORD` (`0 = Automatic`, `1 = English`, `2 = German`).
pub fn save_dictation_language(
    reg: &dyn OsRegistry,
    lang: DictationLanguage,
) -> Result<(), PlatformError> {
    reg.save_dword(REG_DICTATION_LANGUAGE, lang.to_registry_dword())
}

/// Loads saved microphone friendly name from `HKCU\Software\Messages\Microphone` (`REG_SZ`).
/// Returns `None` for System default (missing or empty).
pub fn load_microphone_choice(reg: &dyn OsRegistry) -> Option<String> {
    reg.load_string(REG_MICROPHONE)
        .ok()
        .flatten()
        .map(|s| truncate_mic_name(&s).to_owned())
        .filter(|s| !s.is_empty())
}

/// Persists microphone friendly name (UTF-8-safe 128-byte cap). `None` or empty clears to `""`
/// (System default).
pub fn save_microphone_choice(
    reg: &dyn OsRegistry,
    name: Option<&str>,
) -> Result<(), PlatformError> {
    let cleaned = name.map(truncate_mic_name).unwrap_or("");
    reg.save_string(REG_MICROPHONE, cleaned)
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

    #[test]
    fn decide_deepgram_key_covers_all_five_zig_cases() {
        assert_eq!(
            decide_deepgram_key(None, None),
            (None, DeepgramKeyAction::None)
        );
        assert_eq!(
            decide_deepgram_key(None, Some("seed-1")),
            (Some("seed-1"), DeepgramKeyAction::Save)
        );
        assert_eq!(
            decide_deepgram_key(Some("stored-1"), None),
            (Some("stored-1"), DeepgramKeyAction::Load)
        );
        assert_eq!(
            decide_deepgram_key(Some("same"), Some("same")),
            (Some("same"), DeepgramKeyAction::Load)
        );
        assert_eq!(
            decide_deepgram_key(Some("old"), Some("new")),
            (Some("new"), DeepgramKeyAction::Update)
        );
    }

    #[test]
    fn load_deepgram_key_detailed_handles_seed_refresh_delete_migration_and_fail_closed() {
        let plat = MemoryPlatform::default();
        // 1. Empty -> None
        let empty = load_deepgram_key_detailed(&plat, &plat, |_| None).unwrap();
        assert_eq!(empty.key, None);
        assert_eq!(empty.action, DeepgramKeyAction::None);
        assert!(!empty.seed_saved);

        // 2. Seed from HKCU\Environment with full ASCII whitespace (\x0b, \x0c) -> Save
        plat.set_user_env(ENV_DEEPGRAM_API_KEY, "\x0b\x0c  dg-v1 \r\n\t");
        let saved =
            load_deepgram_key_detailed(&plat, &plat, |_| Some("proc-ignored".into())).unwrap();
        assert_eq!(saved.key.as_deref(), Some("dg-v1"));
        assert_eq!(saved.action, DeepgramKeyAction::Save);
        assert!(saved.seed_saved);
        assert!(plat.has_encrypted_secret(DEEPGRAM_API_KEY_SECRET_NAME));

        // 3. Same seed -> Load without redundant rewrite
        let same = load_deepgram_key_detailed(&plat, &plat, |_| None).unwrap();
        assert_eq!(same.key.as_deref(), Some("dg-v1"));
        assert_eq!(same.action, DeepgramKeyAction::Load);
        assert_eq!(same.stored_outcome, Some(SecretLoadOutcome::Encrypted));
        assert!(!same.seed_saved);

        // 4. Delete env var -> Load from DPAPI blob
        plat.remove_user_env(ENV_DEEPGRAM_API_KEY);
        let from_dpapi = load_deepgram_key_detailed(&plat, &plat, |_| None).unwrap();
        assert_eq!(from_dpapi.key.as_deref(), Some("dg-v1"));
        assert_eq!(from_dpapi.action, DeepgramKeyAction::Load);
        assert_eq!(
            from_dpapi.stored_outcome,
            Some(SecretLoadOutcome::Encrypted)
        );

        // 5. Changed env var -> Update DPAPI blob
        plat.set_user_env(ENV_DEEPGRAM_API_KEY, "dg-v2");
        let updated = load_deepgram_key_detailed(&plat, &plat, |_| None).unwrap();
        assert_eq!(updated.key.as_deref(), Some("dg-v2"));
        assert_eq!(updated.action, DeepgramKeyAction::Update);
        assert!(updated.seed_saved);

        // 6. Legacy plaintext migration succeeds
        let legacy = MemoryPlatform::default();
        legacy.set_legacy_plaintext_secret(DEEPGRAM_API_KEY_SECRET_NAME, "legacy-dg-key");
        let migrated = load_deepgram_key_detailed(&legacy, &legacy, |_| None).unwrap();
        assert_eq!(migrated.key.as_deref(), Some("legacy-dg-key"));
        assert_eq!(migrated.action, DeepgramKeyAction::Load);
        assert_eq!(migrated.stored_outcome, Some(SecretLoadOutcome::Migrated));
        assert!(legacy.has_encrypted_secret(DEEPGRAM_API_KEY_SECRET_NAME));
        assert!(!legacy.has_legacy_secret(DEEPGRAM_API_KEY_SECRET_NAME));

        // 7. Legacy migration failure fails closed even if env var is set
        let broken = MemoryPlatform::default();
        broken.set_legacy_plaintext_secret(DEEPGRAM_API_KEY_SECRET_NAME, "legacy-dg-key");
        broken.set_fail_migration(true);
        broken.set_user_env(ENV_DEEPGRAM_API_KEY, "env-seed");
        assert!(load_deepgram_key_detailed(&broken, &broken, |_| None).is_err());
    }

    #[test]
    fn os_registry_dictation_language_and_microphone_round_trip() {
        let plat = MemoryPlatform::default();
        // Defaults when missing
        assert_eq!(load_dictation_language(&plat), DictationLanguage::Automatic);
        assert_eq!(load_microphone_choice(&plat), None);

        // DWORD 0/1/2 round-trip + >2 fallback to Automatic
        for (lang, expected_dword) in [
            (DictationLanguage::Automatic, 0u32),
            (DictationLanguage::English, 1u32),
            (DictationLanguage::German, 2u32),
        ] {
            save_dictation_language(&plat, lang).unwrap();
            assert_eq!(
                plat.load_dword(REG_DICTATION_LANGUAGE).unwrap(),
                Some(expected_dword)
            );
            assert_eq!(load_dictation_language(&plat), lang);
        }
        plat.save_dword(REG_DICTATION_LANGUAGE, 99).unwrap();
        assert_eq!(load_dictation_language(&plat), DictationLanguage::Automatic);

        // Microphone save, UTF-8-safe 128-byte truncation, and clear to System default
        let long_mic = format!("{}ü", "M".repeat(127));
        save_microphone_choice(&plat, Some(&long_mic)).unwrap();
        let loaded = load_microphone_choice(&plat).unwrap();
        assert_eq!(loaded.len(), 127);
        assert_eq!(loaded, "M".repeat(127));

        save_microphone_choice(&plat, None).unwrap();
        assert_eq!(load_microphone_choice(&plat), None);
    }
}
