//! Registered model profiles, local inference providers, and privacy-aware routing.

mod config;
mod provider;
mod service;

pub use config::{
    ModelCapability, ModelCost, ModelProfile, ModelRegistry, ModelRegistryError, ModelTaskProfile,
    ProviderConfig, ProviderKind, RegistryConfig,
};
pub use provider::{Availability, ModelCard, ProviderSummary};
pub use service::{
    ExplicitModelRequest, InferenceAttempt, InferenceResult, ModelCall, ModelError,
    RoutedInference, operator_allows_hosted_from_environment,
};

/// Maximum combined UTF-8 bytes accepted for one inference request.
pub const MAX_INFERENCE_INPUT_BYTES: usize = 100_000;

#[cfg(test)]
mod tests;
