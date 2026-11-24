use std::{env, time::Instant};

use serde::Serialize;
use thiserror::Error;

use crate::{
    MAX_INFERENCE_INPUT_BYTES,
    config::{ModelCapability, ModelProfile, ModelRegistry, ProviderKind},
    provider::{ProviderClient, discovered_model_id},
};

/// One provider attempt recorded for operational usage and privacy auditing.
#[derive(Clone, Debug, Serialize)]
pub struct InferenceAttempt {
    /// Task profile used for this attempt, if one was selected.
    pub task_profile_id: Option<String>,
    /// Registered model ID.
    pub model_id: String,
    /// Provider ID.
    pub provider_id: String,
    /// Provider protocol.
    pub provider_kind: ProviderKind,
    /// Provider-specific model identifier.
    pub provider_model_id: String,
    /// Whether the provider is configured as hosted.
    pub hosted: bool,
    /// Whether the provider is explicitly trusted on the private LAN.
    pub trusted_lan: bool,
    /// `succeeded` or `failed`.
    pub status: String,
    /// Safe machine-readable failure category, if the call failed.
    pub failure_code: Option<String>,
    /// Whether this was reached as a fallback.
    pub fallback_used: bool,
    /// Elapsed provider request duration in milliseconds.
    pub latency_ms: u64,
    /// Input token count, when reported by the provider.
    pub input_tokens: Option<u64>,
    /// Output token count, when reported by the provider.
    pub output_tokens: Option<u64>,
    /// Estimated cost in USD, only when token counts and configured rates exist.
    pub estimated_cost_usd: Option<f64>,
}

/// Result metadata and generated text from one successful model call.
#[derive(Clone, Debug, Serialize)]
pub struct InferenceResult {
    /// Registered model ID that generated the result.
    pub model_id: String,
    /// Task profile used for the call, if any.
    pub task_profile_id: Option<String>,
    /// Provider-specific model identifier.
    pub provider_model_id: String,
    /// Generated response text.
    pub response: String,
    /// Usage and route metadata.
    pub attempt: InferenceAttempt,
}

/// Result of an explicit registered-model call.
pub struct ModelCall {
    /// Successful response or a safe, actionable error.
    pub result: Result<InferenceResult, ModelError>,
    /// Provider-attempt metadata. Privacy and input validation failures have no attempt.
    pub attempt: Option<InferenceAttempt>,
}

/// Result of profile routing, including failed preferred-model attempts.
pub struct RoutedInference {
    /// Successful response or the final routing error.
    pub result: Result<InferenceResult, ModelError>,
    /// Every provider call made, in order. Prompts and responses are never included.
    pub attempts: Vec<InferenceAttempt>,
}

/// Inputs for one explicitly selected registered model.
pub struct ExplicitModelRequest<'a> {
    /// Configured model ID or provider-qualified ID from dynamic model discovery.
    pub model_id: &'a str,
    /// Registered task profile to apply, or `None` for direct comparison.
    pub task_profile_id: Option<&'a str>,
    /// User prompt.
    pub prompt: &'a str,
    /// Optional user or retrieval context.
    pub context: Option<&'a str>,
    /// Explicit per-request consent for hosted inference.
    pub allow_hosted: bool,
    /// Operator-level hosted inference gate.
    pub operator_allows_hosted: bool,
    /// Whether this model was reached through profile fallback.
    pub fallback_used: bool,
}

/// A model selection, privacy, or provider error safe to return to a caller.
#[derive(Clone, Debug, Error)]
pub enum ModelError {
    /// The requested model is neither configured nor currently discovered.
    #[error("model {0:?} is not configured or currently discovered")]
    UnknownModel(String),
    /// The requested task profile is not in the operator-managed registry.
    #[error("task profile {0:?} is not registered")]
    UnknownTaskProfile(String),
    /// The request would send data to a hosted provider without both consent gates.
    #[error(
        "hosted model access is disabled; enable ITC_MEMORY_ALLOW_HOSTED and explicitly allow hosted inference for this request"
    )]
    HostedConsentRequired,
    /// The prompt and context exceed the bounded input size.
    #[error("combined inference input exceeds the {MAX_INFERENCE_INPUT_BYTES}-byte limit")]
    InputTooLarge,
    /// The provider failed; the category does not include credentials or response bodies.
    #[error("model provider error ({code}): {message}")]
    Provider {
        /// Safe machine-readable error category.
        code: String,
        /// Safe diagnostic.
        message: String,
        /// Whether profile routing may try its next registered fallback.
        retryable: bool,
    },
}

impl ModelError {
    /// Whether this failure is eligible for a configured task-profile fallback.
    #[must_use]
    pub fn allows_fallback(&self) -> bool {
        match self {
            Self::Provider {
                code, retryable, ..
            } => {
                *retryable
                    || matches!(
                        code.as_str(),
                        "authentication_rejected" | "missing_api_key" | "invalid_api_key"
                    )
            }
            Self::UnknownModel(_)
            | Self::UnknownTaskProfile(_)
            | Self::HostedConsentRequired
            | Self::InputTooLarge => false,
        }
    }

    /// Returns a stable failure code suitable for persistence.
    #[must_use]
    pub fn failure_code(&self) -> &'static str {
        match self {
            Self::UnknownModel(_) => "unknown_model",
            Self::UnknownTaskProfile(_) => "unknown_task_profile",
            Self::HostedConsentRequired => "hosted_consent_required",
            Self::InputTooLarge => "input_too_large",
            Self::Provider { code, .. } => match code.as_str() {
                "timeout" => "timeout",
                "connection_failed" => "connection_failed",
                "model_or_endpoint_missing" => "model_or_endpoint_missing",
                "rate_limited" => "rate_limited",
                "provider_unavailable" => "provider_unavailable",
                "authentication_rejected" => "authentication_rejected",
                "missing_api_key" => "missing_api_key",
                "invalid_api_key" => "invalid_api_key",
                "invalid_response" => "invalid_response",
                "empty_response" => "empty_response",
                _ => "provider_request_failed",
            },
        }
    }
}

impl ModelRegistry {
    /// Routes a task to its preferred registered model and ordered fallbacks.
    ///
    /// An explicit model override disables fallback. A hosted model is eligible only when
    /// both operator configuration and this request permit it.
    pub async fn run_profile(
        &self,
        profile_id: &str,
        model_override: Option<&str>,
        prompt: &str,
        context: Option<&str>,
        allow_hosted: bool,
        operator_allows_hosted: bool,
    ) -> RoutedInference {
        let Some(profile) = self.task_profile(profile_id) else {
            return RoutedInference {
                result: Err(ModelError::UnknownTaskProfile(profile_id.to_owned())),
                attempts: Vec::new(),
            };
        };
        let candidates: Vec<&str> = model_override.map_or_else(
            || {
                std::iter::once(profile.preferred_model_id.as_str())
                    .chain(profile.fallback_model_ids.iter().map(String::as_str))
                    .collect()
            },
            |model_id| vec![model_id],
        );
        let explicit = model_override.is_some();
        let mut attempts = Vec::new();
        let mut last_error = None;
        let mut hosted_blocked = false;
        for (index, model_id) in candidates.iter().enumerate() {
            let call = self
                .run_registered_model(ExplicitModelRequest {
                    model_id,
                    task_profile_id: Some(&profile.id),
                    prompt,
                    context,
                    allow_hosted,
                    operator_allows_hosted,
                    fallback_used: index > 0,
                })
                .await;
            if let Some(attempt) = call.attempt {
                attempts.push(attempt);
            }
            match call.result {
                Ok(result) => {
                    return RoutedInference {
                        result: Ok(result),
                        attempts,
                    };
                }
                Err(error) if !explicit && error.allows_fallback() => {
                    last_error = Some(error);
                }
                Err(ModelError::HostedConsentRequired) if !explicit => {
                    hosted_blocked = true;
                }
                Err(error) => {
                    return RoutedInference {
                        result: Err(error),
                        attempts,
                    };
                }
            }
        }
        let error = if hosted_blocked {
            ModelError::HostedConsentRequired
        } else {
            last_error.unwrap_or(ModelError::HostedConsentRequired)
        };
        RoutedInference {
            result: Err(error),
            attempts,
        }
    }

    /// Calls one configured or dynamically discovered model, optionally using a task profile.
    ///
    /// # Errors
    ///
    /// The error is returned in `ModelCall::result`; validation/privacy failures have no
    /// provider attempt and are therefore not counted as provider usage.
    pub async fn run_registered_model(&self, request: ExplicitModelRequest<'_>) -> ModelCall {
        let resolved = match self.resolve_request(&request).await {
            Ok(resolved) => resolved,
            Err(error) => {
                return ModelCall {
                    result: Err(error),
                    attempt: None,
                };
            }
        };
        let start = Instant::now();
        let call_result = ProviderClient
            .generate(
                resolved.provider,
                &resolved.model,
                &resolved.system_prompt,
                request.prompt,
                request.context,
            )
            .await;
        let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let profile_id = resolved.task_profile_id.clone();
        match call_result {
            Ok(response) => {
                let estimated_cost_usd = estimate_cost(&resolved.model, &response);
                let attempt = inference_attempt(
                    &resolved.model,
                    resolved.provider,
                    AttemptMetadata {
                        task_profile_id: profile_id.clone(),
                        fallback_used: request.fallback_used,
                        latency_ms,
                        status: "succeeded",
                        failure_code: None,
                        input_tokens: response.input_tokens,
                        output_tokens: response.output_tokens,
                        estimated_cost_usd,
                    },
                );
                ModelCall {
                    result: Ok(InferenceResult {
                        model_id: resolved.model.id.clone(),
                        task_profile_id: profile_id,
                        provider_model_id: resolved.model.model_identifier.clone(),
                        response: response.text,
                        attempt: attempt.clone(),
                    }),
                    attempt: Some(attempt),
                }
            }
            Err(error) => {
                let attempt = inference_attempt(
                    &resolved.model,
                    resolved.provider,
                    AttemptMetadata {
                        task_profile_id: profile_id,
                        fallback_used: request.fallback_used,
                        latency_ms,
                        status: "failed",
                        failure_code: Some(error.code.clone()),
                        input_tokens: None,
                        output_tokens: None,
                        estimated_cost_usd: None,
                    },
                );
                ModelCall {
                    result: Err(ModelError::Provider {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    }),
                    attempt: Some(attempt),
                }
            }
        }
    }

    async fn resolve_request<'a>(
        &'a self,
        request: &ExplicitModelRequest<'_>,
    ) -> Result<ResolvedModelRequest<'a>, ModelError> {
        let (model, provider) = if let Some(model) = self.model(request.model_id) {
            let provider = self
                .provider(&model.provider)
                .ok_or_else(|| ModelError::UnknownModel(request.model_id.to_owned()))?;
            (model.clone(), provider)
        } else {
            let Some((provider_id, model_identifier)) = request.model_id.split_once("::") else {
                return Err(ModelError::UnknownModel(request.model_id.to_owned()));
            };
            if provider_id.is_empty() || model_identifier.is_empty() {
                return Err(ModelError::UnknownModel(request.model_id.to_owned()));
            }
            let provider = self
                .provider(provider_id)
                .ok_or_else(|| ModelError::UnknownModel(request.model_id.to_owned()))?;
            ensure_provider_consent(provider, request)?;
            let available_models =
                ProviderClient
                    .discover(provider)
                    .await
                    .map_err(|error| ModelError::Provider {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    })?;
            if !available_models.contains(model_identifier) {
                return Err(ModelError::UnknownModel(request.model_id.to_owned()));
            }
            (
                ModelProfile {
                    id: discovered_model_id(provider_id, model_identifier),
                    display_name: model_identifier.to_owned(),
                    provider: provider.id.clone(),
                    model_identifier: model_identifier.to_owned(),
                    context_window: None,
                    think: None,
                    capabilities: Vec::<ModelCapability>::new(),
                    hardware_requirements: None,
                    supported_task_profiles: Vec::new(),
                    cost: None,
                    evaluation_results: Vec::new(),
                    known_limitations: vec![
                        "Discovered from the provider; capabilities and quality are unverified."
                            .to_owned(),
                    ],
                    max_output_tokens: 2048,
                },
                provider,
            )
        };
        if provider.hosted && !(request.allow_hosted && request.operator_allows_hosted) {
            return Err(ModelError::HostedConsentRequired);
        }
        let task_profile = request
            .task_profile_id
            .map(|id| {
                self.task_profile(id)
                    .ok_or_else(|| ModelError::UnknownTaskProfile(id.to_owned()))
            })
            .transpose()?;
        let system_prompt = task_profile.map_or_else(
            || {
                "You are a focused assistant. Follow the request, use only supplied context, and state uncertainty."
                    .to_owned()
            },
            |profile| profile.instructions.clone(),
        );
        if !input_within_limit(&system_prompt, request.prompt, request.context) {
            return Err(ModelError::InputTooLarge);
        }
        Ok(ResolvedModelRequest {
            model,
            provider,
            task_profile_id: task_profile.map(|profile| profile.id.clone()),
            system_prompt,
        })
    }
}

const fn ensure_provider_consent(
    provider: &crate::ProviderConfig,
    request: &ExplicitModelRequest<'_>,
) -> Result<(), ModelError> {
    if provider.hosted && !(request.allow_hosted && request.operator_allows_hosted) {
        return Err(ModelError::HostedConsentRequired);
    }
    Ok(())
}

/// Reads the global hosted-inference privacy gate.
///
/// # Errors
///
/// Returns an error when the environment value is neither `true` nor `false`.
pub fn operator_allows_hosted_from_environment() -> Result<bool, String> {
    match env::var("ITC_MEMORY_ALLOW_HOSTED") {
        Ok(value) if value == "true" => Ok(true),
        Ok(value) if value == "false" => Ok(false),
        Ok(_) => Err("ITC_MEMORY_ALLOW_HOSTED must be true or false".to_owned()),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(env::VarError::NotUnicode(_)) => {
            Err("ITC_MEMORY_ALLOW_HOSTED is not valid Unicode".to_owned())
        }
    }
}

fn input_within_limit(system: &str, prompt: &str, context: Option<&str>) -> bool {
    system
        .len()
        .saturating_add(prompt.len())
        .saturating_add(context.map_or(0, str::len))
        <= MAX_INFERENCE_INPUT_BYTES
}

struct AttemptMetadata {
    task_profile_id: Option<String>,
    fallback_used: bool,
    latency_ms: u64,
    status: &'static str,
    failure_code: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    estimated_cost_usd: Option<f64>,
}

struct ResolvedModelRequest<'a> {
    model: ModelProfile,
    provider: &'a crate::ProviderConfig,
    task_profile_id: Option<String>,
    system_prompt: String,
}

fn inference_attempt(
    model: &ModelProfile,
    provider: &crate::ProviderConfig,
    metadata: AttemptMetadata,
) -> InferenceAttempt {
    InferenceAttempt {
        task_profile_id: metadata.task_profile_id,
        model_id: model.id.clone(),
        provider_id: provider.id.clone(),
        provider_kind: provider.kind,
        provider_model_id: model.model_identifier.clone(),
        hosted: provider.hosted,
        trusted_lan: provider.trusted_lan,
        status: metadata.status.to_owned(),
        failure_code: metadata.failure_code,
        fallback_used: metadata.fallback_used,
        latency_ms: metadata.latency_ms,
        input_tokens: metadata.input_tokens,
        output_tokens: metadata.output_tokens,
        estimated_cost_usd: metadata.estimated_cost_usd,
    }
}

fn estimate_cost(
    model: &ModelProfile,
    response: &super::provider::ProviderResponse,
) -> Option<f64> {
    let rates = model.cost.as_ref()?;
    let input_tokens = response.input_tokens?;
    let output_tokens = response.output_tokens?;
    let input_tokens = f64::from(u32::try_from(input_tokens).ok()?);
    let output_tokens = f64::from(u32::try_from(output_tokens).ok()?);
    Some(
        output_tokens.mul_add(
            rates.output_per_million_tokens_usd,
            input_tokens * rates.input_per_million_tokens_usd,
        ) / 1_000_000.0,
    )
}
