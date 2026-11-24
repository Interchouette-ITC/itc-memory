use std::{
    collections::{HashMap, HashSet},
    env,
    time::Duration,
};

use reqwest::{Client, Method, StatusCode, Url, header::HeaderValue};
use serde::{Deserialize, Serialize};

use crate::config::{ModelProfile, ModelRegistry, ProviderConfig, ProviderKind};

/// Availability observed by a provider discovery request.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability {
    /// The provider reports the configured or dynamically discovered model as available.
    Available,
    /// The model is not installed or the provider could not be reached.
    Unavailable {
        /// Safe diagnostic without response bodies, credentials, or prompts.
        reason: String,
    },
    /// A hosted probe was intentionally skipped by the privacy policy.
    HostedConsentRequired,
}

/// Public, non-secret provider details included in model cards.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderSummary {
    /// Stable provider identifier.
    pub id: String,
    /// Provider protocol.
    pub kind: ProviderKind,
    /// Configured endpoint without credentials or query parameters.
    pub base_url: String,
    /// Whether prompts are sent to a third-party hosted provider.
    pub hosted: bool,
    /// Whether this endpoint is explicitly trusted on the private LAN.
    pub trusted_lan: bool,
}

/// A configured or dynamically discovered model card.
#[derive(Clone, Debug, Serialize)]
pub struct ModelCard {
    /// Model profile, with unknown metadata left unspecified for discovered models.
    pub model: ModelProfile,
    /// Non-secret provider information.
    pub provider: ProviderSummary,
    /// Availability observed during this listing request.
    pub availability: Availability,
    /// Whether this model was found at the provider rather than statically configured.
    pub discovered: bool,
}

#[derive(Debug)]
pub struct ProviderError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

pub struct ProviderResponse {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Default)]
pub struct ProviderClient;

impl ProviderClient {
    pub async fn discover(
        &self,
        provider: &ProviderConfig,
    ) -> Result<HashSet<String>, ProviderError> {
        let client = Self::client(provider)?;
        let endpoint = endpoint(
            provider,
            match provider.kind {
                ProviderKind::Ollama => "api/tags",
                ProviderKind::OpenAiCompatible => "models",
            },
        )?;
        let request = Self::authorize(client.request(Method::GET, endpoint), provider)?;
        let response = request
            .send()
            .await
            .map_err(|error| map_request_error(&error))?;
        let response = successful_response(response)?;
        match provider.kind {
            ProviderKind::Ollama => {
                let payload: OllamaModelList = response.json().await.map_err(|_| {
                    provider_error(
                        "invalid_response",
                        "provider returned an invalid model list",
                        false,
                    )
                })?;
                Ok(payload.models.into_iter().map(|model| model.name).collect())
            }
            ProviderKind::OpenAiCompatible => {
                let payload: OpenAiModelList = response.json().await.map_err(|_| {
                    provider_error(
                        "invalid_response",
                        "provider returned an invalid model list",
                        false,
                    )
                })?;
                Ok(payload.data.into_iter().map(|model| model.id).collect())
            }
        }
    }

    pub async fn generate(
        &self,
        provider: &ProviderConfig,
        model: &ModelProfile,
        system_prompt: &str,
        user_prompt: &str,
        context: Option<&str>,
    ) -> Result<ProviderResponse, ProviderError> {
        let client = Self::client(provider)?;
        let endpoint = endpoint(
            provider,
            match provider.kind {
                ProviderKind::Ollama => "api/chat",
                ProviderKind::OpenAiCompatible => "chat/completions",
            },
        )?;
        let user_content = match context {
            Some(context) if !context.is_empty() => {
                format!("Context:\n{context}\n\nRequest:\n{user_prompt}")
            }
            _ => user_prompt.to_owned(),
        };
        let request = match provider.kind {
            ProviderKind::Ollama => {
                Self::authorize(client.post(endpoint), provider)?.json(&OllamaChatRequest {
                    model: &model.model_identifier,
                    messages: [
                        ChatMessage {
                            role: "system",
                            content: system_prompt,
                        },
                        ChatMessage {
                            role: "user",
                            content: &user_content,
                        },
                    ],
                    stream: false,
                    options: OllamaOptions {
                        num_predict: model.max_output_tokens,
                        num_ctx: model.context_window,
                    },
                    think: model.think.or(provider.default_think),
                })
            }
            ProviderKind::OpenAiCompatible => Self::authorize(client.post(endpoint), provider)?
                .json(&OpenAiChatRequest {
                    model: &model.model_identifier,
                    messages: [
                        ChatMessage {
                            role: "system",
                            content: system_prompt,
                        },
                        ChatMessage {
                            role: "user",
                            content: &user_content,
                        },
                    ],
                    max_tokens: model.max_output_tokens,
                }),
        };
        let response = request
            .send()
            .await
            .map_err(|error| map_request_error(&error))?;
        let response = successful_response(response)?;
        match provider.kind {
            ProviderKind::Ollama => ollama_chat_response(response).await,
            ProviderKind::OpenAiCompatible => openai_chat_response(response).await,
        }
    }

    fn client(provider: &ProviderConfig) -> Result<Client, ProviderError> {
        Client::builder()
            .timeout(Duration::from_secs(provider.timeout_seconds))
            .build()
            .map_err(|_| {
                provider_error(
                    "client_configuration",
                    "could not construct the provider HTTP client",
                    false,
                )
            })
    }

    fn authorize(
        request: reqwest::RequestBuilder,
        provider: &ProviderConfig,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let Some(name) = provider.api_key_env.as_deref() else {
            return Ok(request);
        };
        let token = env::var(name).map_err(|_| {
            provider_error(
                "missing_api_key",
                "the configured provider API-key environment variable is not set",
                false,
            )
        })?;
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            provider_error(
                "invalid_api_key",
                "the configured provider API key is not a valid HTTP header value",
                false,
            )
        })?;
        value.set_sensitive(true);
        Ok(request.header(reqwest::header::AUTHORIZATION, value))
    }
}

impl ModelRegistry {
    /// Lists configured and dynamically discovered model cards.
    ///
    /// Hosted endpoints are contacted only when both operator and caller consent are enabled.
    pub async fn model_cards(
        &self,
        allow_hosted: bool,
        operator_allows_hosted: bool,
    ) -> Vec<ModelCard> {
        let provider_client = ProviderClient;
        let mut available_models_by_provider = HashMap::new();
        for provider in &self.config.providers {
            let available_models = if provider.hosted && !(allow_hosted && operator_allows_hosted) {
                Err("hosted model discovery requires operator and caller consent".to_owned())
            } else {
                match provider_client.discover(provider).await {
                    Ok(available) => Ok(available),
                    Err(error) => Err(error.message),
                }
            };
            available_models_by_provider.insert(provider.id.as_str(), available_models);
        }

        let configured_models = self
            .config
            .models
            .iter()
            .map(|model| (model.provider.as_str(), model.model_identifier.as_str()))
            .collect::<HashSet<_>>();
        let mut cards = Vec::with_capacity(self.config.models.len());
        for model in &self.config.models {
            let Some(provider) = self.providers.get(&model.provider) else {
                continue;
            };
            let availability = if provider.hosted && !(allow_hosted && operator_allows_hosted) {
                Availability::HostedConsentRequired
            } else {
                match available_models_by_provider.get(provider.id.as_str()) {
                    Some(Ok(available)) if available.contains(&model.model_identifier) => {
                        Availability::Available
                    }
                    Some(Ok(_)) => Availability::Unavailable {
                        reason: "configured model is not currently available at the provider"
                            .to_owned(),
                    },
                    Some(Err(reason)) => Availability::Unavailable {
                        reason: reason.clone(),
                    },
                    None => Availability::Unavailable {
                        reason: "provider availability was not checked".to_owned(),
                    },
                }
            };
            cards.push(ModelCard {
                model: model.clone(),
                provider: ProviderSummary {
                    id: provider.id.clone(),
                    kind: provider.kind,
                    base_url: provider.base_url.clone(),
                    hosted: provider.hosted,
                    trusted_lan: provider.trusted_lan,
                },
                availability,
                discovered: false,
            });
        }

        for provider in &self.config.providers {
            let Some(Ok(available_models)) = available_models_by_provider.get(provider.id.as_str())
            else {
                continue;
            };
            let mut discovered_models = available_models
                .iter()
                .filter(|model_identifier| {
                    !configured_models.contains(&(provider.id.as_str(), model_identifier.as_str()))
                })
                .cloned()
                .collect::<Vec<_>>();
            discovered_models.sort_unstable();
            for model_identifier in discovered_models {
                cards.push(ModelCard {
                    model: discovered_model_profile(provider, &model_identifier),
                    provider: ProviderSummary {
                        id: provider.id.clone(),
                        kind: provider.kind,
                        base_url: provider.base_url.clone(),
                        hosted: provider.hosted,
                        trusted_lan: provider.trusted_lan,
                    },
                    availability: Availability::Available,
                    discovered: true,
                });
            }
        }
        cards
    }
}

pub fn discovered_model_id(provider_id: &str, model_identifier: &str) -> String {
    format!("{provider_id}::{model_identifier}")
}

fn discovered_model_profile(provider: &ProviderConfig, model_identifier: &str) -> ModelProfile {
    ModelProfile {
        id: discovered_model_id(&provider.id, model_identifier),
        display_name: model_identifier.to_owned(),
        provider: provider.id.clone(),
        model_identifier: model_identifier.to_owned(),
        context_window: None,
        think: None,
        capabilities: Vec::new(),
        hardware_requirements: None,
        supported_task_profiles: Vec::new(),
        cost: None,
        evaluation_results: Vec::new(),
        known_limitations: vec![
            "Discovered from the provider; capabilities and quality are unverified.".to_owned(),
        ],
        max_output_tokens: 2048,
    }
}

async fn ollama_chat_response(
    response: reqwest::Response,
) -> Result<ProviderResponse, ProviderError> {
    let payload: OllamaChatResponse = response.json().await.map_err(|_| {
        provider_error(
            "invalid_response",
            "provider returned an invalid chat response",
            false,
        )
    })?;
    if payload.message.content.trim().is_empty() {
        return Err(provider_error(
            "empty_response",
            "provider returned an empty completion",
            true,
        ));
    }
    Ok(ProviderResponse {
        text: payload.message.content,
        input_tokens: payload.prompt_eval_count,
        output_tokens: payload.eval_count,
    })
}

async fn openai_chat_response(
    response: reqwest::Response,
) -> Result<ProviderResponse, ProviderError> {
    let payload: OpenAiChatResponse = response.json().await.map_err(|_| {
        provider_error(
            "invalid_response",
            "provider returned an invalid chat response",
            false,
        )
    })?;
    let choice = payload.choices.into_iter().next().ok_or_else(|| {
        provider_error(
            "empty_response",
            "provider returned no completion choices",
            true,
        )
    })?;
    if choice.message.content.trim().is_empty() {
        return Err(provider_error(
            "empty_response",
            "provider returned an empty completion",
            true,
        ));
    }
    Ok(ProviderResponse {
        text: choice.message.content,
        input_tokens: payload.usage.as_ref().and_then(|usage| usage.prompt_tokens),
        output_tokens: payload.usage.and_then(|usage| usage.completion_tokens),
    })
}

fn endpoint(provider: &ProviderConfig, route: &str) -> Result<Url, ProviderError> {
    let mut base = Url::parse(&provider.base_url)
        .map_err(|_| provider_error("invalid_endpoint", "provider base URL is invalid", false))?;
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(route).map_err(|_| {
        provider_error(
            "invalid_endpoint",
            "provider request URL could not be constructed",
            false,
        )
    })
}

fn successful_response(response: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retryable = status == StatusCode::NOT_FOUND
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error();
    let (code, message) = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => (
            "authentication_rejected",
            "provider rejected authentication",
        ),
        StatusCode::NOT_FOUND => (
            "model_or_endpoint_missing",
            "provider model or endpoint was not found",
        ),
        StatusCode::TOO_MANY_REQUESTS => ("rate_limited", "provider rate-limited the request"),
        _ if status.is_server_error() => {
            ("provider_unavailable", "provider returned a server error")
        }
        _ => ("request_rejected", "provider rejected the request"),
    };
    Err(provider_error(
        code,
        &format!("{message} (HTTP {})", status.as_u16()),
        retryable,
    ))
}

fn map_request_error(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        provider_error("timeout", "provider request timed out", true)
    } else if error.is_connect() {
        provider_error("connection_failed", "could not connect to provider", true)
    } else {
        provider_error("request_failed", "provider request failed", false)
    }
}

fn provider_error(code: &str, message: &str, retryable: bool) -> ProviderError {
    ProviderError {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable,
    }
}

#[derive(Deserialize)]
struct OllamaModelList {
    models: Vec<OllamaListedModel>,
}

#[derive(Deserialize)]
struct OllamaListedModel {
    name: String,
}

#[derive(Deserialize)]
struct OpenAiModelList {
    data: Vec<OpenAiListedModel>,
}

#[derive(Deserialize)]
struct OpenAiListedModel {
    id: String,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_predict: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    stream: bool,
    options: OllamaOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
}

#[derive(Deserialize)]
struct OllamaMessage {
    content: String,
}

#[derive(Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    max_tokens: u32,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}
