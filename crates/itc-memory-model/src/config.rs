use std::{
    collections::{HashMap, HashSet},
    env, fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A provider protocol implemented by the model service.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Ollama native `/api/chat` and `/api/tags` APIs.
    Ollama,
    /// OpenAI-compatible `/chat/completions` and `/models` APIs, including llama.cpp.
    OpenAiCompatible,
}

impl ProviderKind {
    /// Stable identifier stored in inference usage records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "open_ai_compatible",
        }
    }
}

/// A model capability declared by the operator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    /// General conversational responses.
    Chat,
    /// Source-code generation or analysis.
    Code,
    /// Multi-step reasoning.
    Reasoning,
    /// Structured JSON output.
    StructuredOutput,
    /// Tool-call generation.
    ToolCalling,
    /// Embedding generation.
    Embeddings,
}

/// Optional per-million-token price used for estimates, not billing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCost {
    /// Estimated input-token price in USD per million tokens.
    pub input_per_million_tokens_usd: f64,
    /// Estimated output-token price in USD per million tokens.
    pub output_per_million_tokens_usd: f64,
}

/// A provider endpoint and its non-secret authentication reference.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Stable provider identifier.
    pub id: String,
    /// Provider protocol.
    pub kind: ProviderKind,
    /// Base URL. Credentials and query parameters are prohibited.
    pub base_url: String,
    /// Whether prompts are sent to a third-party hosted provider.
    #[serde(default)]
    pub hosted: bool,
    /// Whether this endpoint is explicitly trusted on the private LAN.
    #[serde(default)]
    pub trusted_lan: bool,
    /// Name of the environment variable containing an optional bearer token.
    pub api_key_env: Option<String>,
    /// Request timeout in seconds.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Default Ollama thinking setting for dynamically discovered models.
    #[serde(default)]
    pub default_think: Option<bool>,
}

/// A registered model and its explicit operator-provided metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    /// Stable internal identifier selected by agents and users.
    pub id: String,
    /// Human-readable model card name.
    pub display_name: String,
    /// Registered provider identifier.
    pub provider: String,
    /// Provider-specific model name.
    pub model_identifier: String,
    /// Context window in tokens, when known.
    pub context_window: Option<u32>,
    /// Optional Ollama thinking toggle; ignored by OpenAI-compatible servers.
    pub think: Option<bool>,
    /// Operator-declared capabilities.
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
    /// Hardware requirements or practical runtime notes, when known.
    pub hardware_requirements: Option<String>,
    /// Task profile IDs for which this model is registered.
    #[serde(default)]
    pub supported_task_profiles: Vec<String>,
    /// Optional estimated input/output price.
    pub cost: Option<ModelCost>,
    /// Optional operator-recorded evaluation notes.
    #[serde(default)]
    pub evaluation_results: Vec<String>,
    /// Known limitations shown to model selectors.
    #[serde(default)]
    pub known_limitations: Vec<String>,
    /// Maximum generated tokens requested from the provider.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
}

/// A task-focused instruction profile and its preferred/fallback models.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTaskProfile {
    /// Stable task-role identifier.
    pub id: String,
    /// Human-readable task-role name.
    pub display_name: String,
    /// The work this profile is intended to perform.
    pub responsibility: String,
    /// System instruction applied to requests routed through this profile.
    pub instructions: String,
    /// Preferred registered model ID.
    pub preferred_model_id: String,
    /// Ordered fallback model IDs. Hosted models are still consent-gated.
    #[serde(default)]
    pub fallback_model_ids: Vec<String>,
}

/// Versioned on-disk model and task-profile registry.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// Registry schema version; currently only version 1 is supported.
    pub version: u32,
    /// Configured inference providers.
    pub providers: Vec<ProviderConfig>,
    /// Models the agent may select.
    pub models: Vec<ModelProfile>,
    /// Task roles and their routing assignments.
    pub task_profiles: Vec<ModelTaskProfile>,
}

/// Validated in-memory model registry.
#[derive(Clone, Debug)]
pub struct ModelRegistry {
    pub(crate) config: RegistryConfig,
    pub(crate) providers: HashMap<String, ProviderConfig>,
    pub(crate) models: HashMap<String, ModelProfile>,
    pub(crate) task_profiles: HashMap<String, ModelTaskProfile>,
}

/// A model registry configuration error.
#[derive(Debug, Error)]
pub enum ModelRegistryError {
    /// The registry file could not be read.
    #[error("could not read model registry {path}: {source}")]
    Read {
        /// Registry path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The registry file was not valid JSON.
    #[error("model registry is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A semantic validation rule failed.
    #[error("invalid model registry: {0}")]
    Invalid(String),
}

impl RegistryConfig {
    /// Loads and validates a registry file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, malformed, or invalid.
    pub fn from_file(path: impl AsRef<Path>) -> Result<ModelRegistry, ModelRegistryError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ModelRegistryError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = serde_json::from_str(&contents)?;
        ModelRegistry::new(config)
    }
}

impl ModelRegistry {
    /// Loads the configured registry. The path defaults to `config/models.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be loaded or validated.
    pub fn from_environment() -> Result<Self, ModelRegistryError> {
        Self::from_environment_in(None)
    }

    /// Loads the registry, resolving relative paths against the environment file directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be loaded or validated.
    pub fn from_environment_in(config_dir: Option<&Path>) -> Result<Self, ModelRegistryError> {
        let path =
            env::var_os("ITC_MEMORY_MODELS_FILE").unwrap_or_else(|| "config/models.json".into());
        RegistryConfig::from_file(itc_memory_core::resolve_config_path(path, config_dir))
    }

    /// Validates and constructs a registry from an in-memory definition.
    ///
    /// # Errors
    ///
    /// Returns an error if identifiers, providers, model assignments, or endpoints are invalid.
    pub fn new(config: RegistryConfig) -> Result<Self, ModelRegistryError> {
        validate_config(&config)?;
        let providers = config
            .providers
            .iter()
            .cloned()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        let models = config
            .models
            .iter()
            .cloned()
            .map(|model| (model.id.clone(), model))
            .collect();
        let task_profiles = config
            .task_profiles
            .iter()
            .cloned()
            .map(|profile| (profile.id.clone(), profile))
            .collect();
        Ok(Self {
            config,
            providers,
            models,
            task_profiles,
        })
    }

    /// Returns the configured providers in file order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderConfig] {
        &self.config.providers
    }

    /// Returns the registered models in file order.
    #[must_use]
    pub fn models(&self) -> &[ModelProfile] {
        &self.config.models
    }

    /// Returns registered task profiles in file order.
    #[must_use]
    pub fn task_profiles(&self) -> &[ModelTaskProfile] {
        &self.config.task_profiles
    }

    /// Looks up a registered provider.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    /// Looks up a registered model.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelProfile> {
        self.models.get(id)
    }

    /// Looks up a registered task profile.
    #[must_use]
    pub fn task_profile(&self, id: &str) -> Option<&ModelTaskProfile> {
        self.task_profiles.get(id)
    }
}

fn validate_config(config: &RegistryConfig) -> Result<(), ModelRegistryError> {
    if config.version != 1 {
        return invalid(format!(
            "unsupported version {}; only version 1 is supported",
            config.version
        ));
    }
    if config.providers.is_empty() || config.models.is_empty() || config.task_profiles.is_empty() {
        return invalid("providers, models, and task_profiles must not be empty".to_owned());
    }

    let provider_ids = validate_providers(&config.providers)?;
    let model_ids = validate_models(&config.models, &provider_ids)?;
    let profile_ids = validate_task_profiles(&config.task_profiles)?;
    validate_assignments(config, &model_ids, &profile_ids)
}

fn validate_providers(providers: &[ProviderConfig]) -> Result<HashSet<&str>, ModelRegistryError> {
    let mut provider_ids = HashSet::new();
    for provider in providers {
        validate_id(&provider.id, "provider")?;
        if !provider_ids.insert(provider.id.as_str()) {
            return invalid(format!("duplicate provider ID {}", provider.id));
        }
        if !(1..=600).contains(&provider.timeout_seconds) {
            return invalid(format!(
                "provider {} timeout_seconds must be from 1 through 600",
                provider.id
            ));
        }
        validate_provider_endpoint(provider)?;
        if provider.hosted && provider.trusted_lan {
            return invalid(format!(
                "provider {} cannot be both hosted and trusted_lan",
                provider.id
            ));
        }
        if provider.default_think.is_some() && provider.kind != ProviderKind::Ollama {
            return invalid(format!(
                "provider {} default_think is only supported by Ollama",
                provider.id
            ));
        }
        if let Some(name) = provider.api_key_env.as_deref()
            && !valid_environment_name(name)
        {
            return invalid(format!(
                "provider {} api_key_env must be an environment variable name",
                provider.id
            ));
        }
        if provider.hosted {
            if provider.kind != ProviderKind::OpenAiCompatible {
                return invalid(format!(
                    "hosted provider {} must use the OpenAI-compatible protocol",
                    provider.id
                ));
            }
            if provider.api_key_env.is_none() {
                return invalid(format!(
                    "hosted provider {} must reference an API-key environment variable",
                    provider.id
                ));
            }
        }
    }
    Ok(provider_ids)
}

fn validate_models<'a>(
    models: &'a [ModelProfile],
    provider_ids: &HashSet<&str>,
) -> Result<HashSet<&'a str>, ModelRegistryError> {
    let mut model_ids = HashSet::new();
    for model in models {
        validate_id(&model.id, "model")?;
        if !model_ids.insert(model.id.as_str()) {
            return invalid(format!("duplicate model ID {}", model.id));
        }
        if !provider_ids.contains(model.provider.as_str()) {
            return invalid(format!(
                "model {} references unknown provider {}",
                model.id, model.provider
            ));
        }
        if model.model_identifier.trim().is_empty() {
            return invalid(format!(
                "model {} has an empty provider identifier",
                model.id
            ));
        }
        if model.max_output_tokens == 0 || model.max_output_tokens > 32_768 {
            return invalid(format!(
                "model {} max_output_tokens must be from 1 through 32768",
                model.id
            ));
        }
        if model.context_window == Some(0) {
            return invalid(format!(
                "model {} context_window must be positive",
                model.id
            ));
        }
        if let Some(cost) = &model.cost
            && (!cost.input_per_million_tokens_usd.is_finite()
                || !cost.output_per_million_tokens_usd.is_finite()
                || cost.input_per_million_tokens_usd < 0.0
                || cost.output_per_million_tokens_usd < 0.0)
        {
            return invalid(format!("model {} has invalid token cost rates", model.id));
        }
    }
    Ok(model_ids)
}

fn validate_task_profiles(
    task_profiles: &[ModelTaskProfile],
) -> Result<HashSet<&str>, ModelRegistryError> {
    let mut profile_ids = HashSet::new();
    for profile in task_profiles {
        validate_id(&profile.id, "task profile")?;
        if !profile_ids.insert(profile.id.as_str()) {
            return invalid(format!("duplicate task profile ID {}", profile.id));
        }
        if profile.display_name.trim().is_empty()
            || profile.responsibility.trim().is_empty()
            || profile.instructions.trim().is_empty()
        {
            return invalid(format!(
                "task profile {} requires a name, responsibility, and instructions",
                profile.id
            ));
        }
        if profile
            .fallback_model_ids
            .contains(&profile.preferred_model_id)
        {
            return invalid(format!(
                "task profile {} repeats its preferred model as a fallback",
                profile.id
            ));
        }
        let mut assigned = HashSet::from([profile.preferred_model_id.as_str()]);
        for fallback in &profile.fallback_model_ids {
            if !assigned.insert(fallback.as_str()) {
                return invalid(format!(
                    "task profile {} repeats fallback model {}",
                    profile.id, fallback
                ));
            }
        }
    }
    Ok(profile_ids)
}

fn validate_assignments(
    config: &RegistryConfig,
    model_ids: &HashSet<&str>,
    profile_ids: &HashSet<&str>,
) -> Result<(), ModelRegistryError> {
    for profile in &config.task_profiles {
        let assigned_models = std::iter::once(profile.preferred_model_id.as_str())
            .chain(profile.fallback_model_ids.iter().map(String::as_str));
        for model_id in assigned_models {
            if !model_ids.contains(model_id) {
                return invalid(format!(
                    "task profile {} references unknown model {}",
                    profile.id, model_id
                ));
            }
            let Some(model) = config.models.iter().find(|model| model.id == model_id) else {
                return invalid(format!(
                    "task profile {} references unknown model {}",
                    profile.id, model_id
                ));
            };
            if !model
                .supported_task_profiles
                .iter()
                .any(|supported| supported == &profile.id)
            {
                return invalid(format!(
                    "model {} is assigned to profile {} but does not list that supported task profile",
                    model.id, profile.id
                ));
            }
        }
    }
    for model in &config.models {
        for profile in &model.supported_task_profiles {
            if !profile_ids.contains(profile.as_str()) {
                return invalid(format!(
                    "model {} references unknown task profile {}",
                    model.id, profile
                ));
            }
        }
    }
    Ok(())
}

fn validate_provider_endpoint(provider: &ProviderConfig) -> Result<(), ModelRegistryError> {
    let url = Url::parse(&provider.base_url).map_err(|_| {
        ModelRegistryError::Invalid(format!("provider {} has an invalid base_url", provider.id))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return invalid(format!(
            "provider {} base_url must be HTTP(S) without credentials, query, or fragment",
            provider.id
        ));
    }
    let local_endpoint = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host)
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    let trusted_lan_endpoint = url.host_str().is_some_and(is_trusted_lan_host);
    if provider.trusted_lan && (!trusted_lan_endpoint || local_endpoint) {
        return invalid(format!(
            "trusted_lan provider {} must use a private LAN hostname or address",
            provider.id
        ));
    }
    if provider.hosted && url.scheme() != "https" {
        return invalid(format!("hosted provider {} must use HTTPS", provider.id));
    }
    if !provider.hosted && !local_endpoint && !provider.trusted_lan {
        return invalid(format!(
            "non-loopback provider {} must be hosted or explicitly marked trusted_lan",
            provider.id
        ));
    }
    Ok(())
}

fn is_trusted_lan_host(host: &str) -> bool {
    if let Ok(address) = host.parse::<IpAddr>() {
        return match address {
            IpAddr::V4(address) => address.is_private() || address.octets()[..2] == [169, 254],
            IpAddr::V6(address) => {
                let octets = address.octets();
                octets[0] & 0xfe == 0xfc || octets[..2] == [0xfe, 0x80]
            }
        };
    }
    host.trim_end_matches('.')
        .to_ascii_lowercase()
        .ends_with(".local")
}

fn validate_id(value: &str, kind: &str) -> Result<(), ModelRegistryError> {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        || !chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        })
    {
        return invalid(format!(
            "{kind} ID {value:?} must use lowercase letters, digits, hyphens, or underscores"
        ));
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_uppercase())
        && chars.all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
}

const fn default_timeout_seconds() -> u64 {
    120
}

const fn default_max_output_tokens() -> u32 {
    1024
}

const fn invalid<T>(message: String) -> Result<T, ModelRegistryError> {
    Err(ModelRegistryError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{
      "version": 1,
      "providers": [{"id":"local","kind":"ollama","base_url":"http://127.0.0.1:11434","hosted":false}],
      "models": [{
        "id":"small","display_name":"Small","provider":"local","model_identifier":"small:latest",
        "supported_task_profiles":["ask"],"capabilities":["chat"],"known_limitations":[]
      }],
      "task_profiles": [{
        "id":"ask","display_name":"Ask","responsibility":"Answer questions","instructions":"Be precise.",
        "preferred_model_id":"small","fallback_model_ids":[]
      }]
    }"#;

    #[test]
    fn validates_registered_models_and_profiles() {
        let parsed: RegistryConfig = serde_json::from_str(CONFIG).unwrap();
        let registry = ModelRegistry::new(parsed).unwrap();
        assert_eq!(
            registry.model("small").unwrap().model_identifier,
            "small:latest"
        );
        assert_eq!(
            registry.task_profile("ask").unwrap().preferred_model_id,
            "small"
        );
    }

    #[test]
    fn rejects_remote_http_and_missing_task_assignments() {
        let remote_http = CONFIG.replace("http://127.0.0.1:11434", "http://models.example.test");
        let parsed: RegistryConfig = serde_json::from_str(&remote_http).unwrap();
        assert!(ModelRegistry::new(parsed).is_err());

        let unsupported = CONFIG.replace(
            "\"supported_task_profiles\":[\"ask\"]",
            "\"supported_task_profiles\":[]",
        );
        let parsed: RegistryConfig = serde_json::from_str(&unsupported).unwrap();
        assert!(ModelRegistry::new(parsed).is_err());
    }

    #[test]
    fn permits_only_explicitly_trusted_private_lan_endpoints() {
        let trusted_lan = CONFIG
            .replace("http://127.0.0.1:11434", "http://model-host.local:11434")
            .replace("\"hosted\":false", "\"hosted\":false,\"trusted_lan\":true");
        let parsed: RegistryConfig = serde_json::from_str(&trusted_lan).unwrap();
        assert!(ModelRegistry::new(parsed).is_ok());

        let public_host = trusted_lan.replace("model-host.local", "models.example.test");
        let parsed: RegistryConfig = serde_json::from_str(&public_host).unwrap();
        assert!(ModelRegistry::new(parsed).is_err());
    }

    #[test]
    fn hosted_providers_require_https_and_a_named_secret() {
        let hosted = CONFIG
            .replace("\"kind\":\"ollama\"", "\"kind\":\"open_ai_compatible\"")
            .replace("\"hosted\":false", "\"hosted\":true")
            .replace("http://127.0.0.1:11434", "https://models.example.test/v1");
        let parsed: RegistryConfig = serde_json::from_str(&hosted).unwrap();
        assert!(ModelRegistry::new(parsed).is_err());
    }
}
