use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::{
    Availability, ModelCapability, ModelCost, ModelProfile, ModelRegistry, ModelTaskProfile,
    ProviderConfig, ProviderKind, RegistryConfig,
};

#[tokio::test]
async fn ollama_discovery_reports_registered_model_availability() {
    let (registry, _address, server) = local_ollama_registry().await;
    let cards = registry.model_cards(false, false).await;
    let available = cards
        .iter()
        .find(|card| card.model.id == "fallback")
        .unwrap();
    assert!(matches!(available.availability, Availability::Available));
    let unavailable = cards
        .iter()
        .find(|card| card.model.id == "preferred")
        .unwrap();
    assert!(matches!(
        unavailable.availability,
        Availability::Unavailable { .. }
    ));
    server.abort();
}

#[tokio::test]
async fn dynamically_discovered_ollama_models_are_selectable_but_not_implicitly_assigned() {
    let (registry, _address, server) = local_ollama_registry().await;
    let cards = registry.model_cards(false, false).await;
    let card = cards
        .iter()
        .find(|card| card.model.id == "local::catalog:latest")
        .unwrap();
    assert!(card.discovered);
    assert_eq!(card.model.supported_task_profiles, Vec::<String>::new());
    assert!(matches!(card.availability, Availability::Available));

    let call = registry
        .run_registered_model(crate::ExplicitModelRequest {
            model_id: "local::catalog:latest",
            task_profile_id: Some("ask"),
            prompt: "answer",
            context: None,
            allow_hosted: false,
            operator_allows_hosted: false,
            fallback_used: false,
        })
        .await;
    assert_eq!(call.result.unwrap().response, "fallback answer");
    assert_eq!(call.attempt.unwrap().model_id, "local::catalog:latest");
    server.abort();
}

#[tokio::test]
async fn dynamic_model_selection_rejects_names_not_returned_by_provider_discovery() {
    let (registry, _address, server) = local_ollama_registry().await;
    let call = registry
        .run_registered_model(crate::ExplicitModelRequest {
            model_id: "local::invented:latest",
            task_profile_id: None,
            prompt: "answer",
            context: None,
            allow_hosted: false,
            operator_allows_hosted: false,
            fallback_used: false,
        })
        .await;
    assert!(matches!(
        call.result,
        Err(crate::ModelError::UnknownModel(_))
    ));
    assert!(call.attempt.is_none());
    server.abort();
}

#[tokio::test]
async fn failed_preferred_model_routes_to_and_records_the_fallback() {
    let (registry, _address, server) = local_ollama_registry().await;
    let routed = registry
        .run_profile("ask", None, "answer", None, false, false)
        .await;
    let result = routed.result.unwrap();
    assert_eq!(result.model_id, "fallback");
    assert_eq!(result.response, "fallback answer");
    assert_eq!(routed.attempts.len(), 2);
    assert_eq!(
        routed.attempts[0].failure_code.as_deref(),
        Some("model_or_endpoint_missing")
    );
    assert!(routed.attempts[1].fallback_used);
    assert_eq!(routed.attempts[1].input_tokens, Some(5));
    assert_eq!(routed.attempts[1].output_tokens, Some(3));
    server.abort();
}

#[tokio::test]
async fn empty_completion_routes_to_the_configured_fallback() {
    let (registry, _address, server) = local_ollama_registry().await;
    let routed = registry
        .run_profile("empty-ask", None, "answer", None, false, false)
        .await;
    let result = routed.result.unwrap();
    assert_eq!(result.model_id, "fallback");
    assert_eq!(result.response, "fallback answer");
    assert_eq!(
        routed.attempts[0].failure_code.as_deref(),
        Some("empty_response")
    );
    assert!(routed.attempts[1].fallback_used);
    server.abort();
}

#[tokio::test]
async fn explicit_model_selection_does_not_use_an_automatic_fallback() {
    let (registry, _address, server) = local_ollama_registry().await;
    let routed = registry
        .run_profile("ask", Some("preferred"), "answer", None, false, false)
        .await;
    assert!(routed.result.is_err());
    assert_eq!(routed.attempts.len(), 1);
    assert_eq!(
        routed.attempts[0].failure_code.as_deref(),
        Some("model_or_endpoint_missing")
    );
    assert!(!routed.attempts[0].fallback_used);
    server.abort();
}

#[tokio::test]
async fn hosted_model_is_never_contacted_without_both_consent_gates() {
    let registry = hosted_registry();
    let result = registry
        .run_profile(
            "ask",
            None,
            "do not send me",
            Some("private context"),
            true,
            false,
        )
        .await;
    assert!(result.attempts.is_empty());
    assert!(result.result.is_err());

    let cards = registry.model_cards(true, false).await;
    assert!(matches!(
        cards[0].availability,
        Availability::HostedConsentRequired
    ));
}

#[tokio::test]
async fn openai_compatible_backend_returns_usage_and_cost_estimates() {
    let app = Router::new()
        .route("/v1/models", get(openai_models))
        .route("/v1/chat/completions", post(openai_chat));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut profile = model("small", "small:latest");
    profile.cost = Some(ModelCost {
        input_per_million_tokens_usd: 1.0,
        output_per_million_tokens_usd: 2.0,
    });
    let registry = ModelRegistry::new(RegistryConfig {
        version: 1,
        providers: vec![ProviderConfig {
            id: "local".to_owned(),
            kind: ProviderKind::OpenAiCompatible,
            base_url: format!("http://{address}/v1"),
            hosted: false,
            trusted_lan: false,
            api_key_env: None,
            timeout_seconds: 5,
            default_think: None,
        }],
        models: vec![profile],
        task_profiles: vec![ModelTaskProfile {
            id: "ask".to_owned(),
            display_name: "Ask".to_owned(),
            responsibility: "Answer a question".to_owned(),
            instructions: "Answer accurately.".to_owned(),
            preferred_model_id: "small".to_owned(),
            fallback_model_ids: Vec::new(),
        }],
    })
    .unwrap();
    let routed = registry
        .run_profile("ask", None, "answer", None, false, false)
        .await;
    let result = routed.result.unwrap();
    assert_eq!(result.response, "local compatible response");
    assert_eq!(result.attempt.input_tokens, Some(5));
    assert_eq!(result.attempt.output_tokens, Some(3));
    assert_eq!(result.attempt.estimated_cost_usd, Some(0.000_011));
    server.abort();
}

async fn local_ollama_registry() -> (
    ModelRegistry,
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    let app = Router::new()
        .route("/api/tags", get(model_tags))
        .route("/api/chat", post(chat));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let endpoint = format!("http://{address}");
    let mut empty_model = model("empty", "empty:latest");
    empty_model
        .supported_task_profiles
        .push("empty-ask".to_owned());
    let mut fallback_model = model("fallback", "fallback:latest");
    fallback_model
        .supported_task_profiles
        .push("empty-ask".to_owned());
    let registry = ModelRegistry::new(RegistryConfig {
        version: 1,
        providers: vec![ProviderConfig {
            id: "local".to_owned(),
            kind: ProviderKind::Ollama,
            base_url: endpoint,
            hosted: false,
            trusted_lan: false,
            api_key_env: None,
            timeout_seconds: 5,
            default_think: Some(false),
        }],
        models: vec![
            model("preferred", "preferred:latest"),
            empty_model,
            fallback_model,
        ],
        task_profiles: vec![
            ModelTaskProfile {
                id: "ask".to_owned(),
                display_name: "Ask".to_owned(),
                responsibility: "Answer a question".to_owned(),
                instructions: "Answer accurately.".to_owned(),
                preferred_model_id: "preferred".to_owned(),
                fallback_model_ids: vec!["fallback".to_owned()],
            },
            ModelTaskProfile {
                id: "empty-ask".to_owned(),
                display_name: "Ask with empty preferred".to_owned(),
                responsibility: "Answer a question".to_owned(),
                instructions: "Answer accurately.".to_owned(),
                preferred_model_id: "empty".to_owned(),
                fallback_model_ids: vec!["fallback".to_owned()],
            },
        ],
    })
    .unwrap();
    (registry, address, server)
}

fn hosted_registry() -> ModelRegistry {
    let provider = ProviderConfig {
        id: "hosted".to_owned(),
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://models.example.test/v1".to_owned(),
        hosted: true,
        trusted_lan: false,
        api_key_env: Some("ITC_MEMORY_TEST_HOSTED_KEY".to_owned()),
        timeout_seconds: 5,
        default_think: None,
    };
    let mut remote_model = model("remote", "remote-model");
    remote_model.provider = "hosted".to_owned();
    ModelRegistry::new(RegistryConfig {
        version: 1,
        providers: vec![provider],
        models: vec![remote_model],
        task_profiles: vec![ModelTaskProfile {
            id: "ask".to_owned(),
            display_name: "Ask".to_owned(),
            responsibility: "Answer a question".to_owned(),
            instructions: "Answer accurately.".to_owned(),
            preferred_model_id: "remote".to_owned(),
            fallback_model_ids: Vec::new(),
        }],
    })
    .unwrap()
}

fn model(id: &str, model_identifier: &str) -> ModelProfile {
    ModelProfile {
        id: id.to_owned(),
        display_name: id.to_owned(),
        provider: "local".to_owned(),
        model_identifier: model_identifier.to_owned(),
        context_window: Some(4096),
        think: Some(false),
        capabilities: vec![ModelCapability::Chat],
        hardware_requirements: None,
        supported_task_profiles: vec!["ask".to_owned()],
        cost: Some(ModelCost {
            input_per_million_tokens_usd: 1.0,
            output_per_million_tokens_usd: 2.0,
        }),
        evaluation_results: Vec::new(),
        known_limitations: Vec::new(),
        max_output_tokens: 32,
    }
}

async fn model_tags() -> Json<Value> {
    Json(json!({"models":[{"name":"fallback:latest"},{"name":"catalog:latest"}]}))
}

async fn openai_models() -> Json<Value> {
    Json(json!({"data":[{"id":"small:latest"}]}))
}

async fn openai_chat(Json(_request): Json<Value>) -> Json<Value> {
    Json(json!({
        "choices":[{"message":{"content":"local compatible response"}}],
        "usage":{"prompt_tokens":5,"completion_tokens":3}
    }))
}

async fn chat(Json(request): Json<Value>) -> (StatusCode, Json<Value>) {
    if request["model"] == "preferred:latest" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"model missing"})),
        );
    }
    if request["think"] != false {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"expected thinking to be disabled"})),
        );
    }
    if request["model"] == "catalog:latest" {
        assert!(request["options"].get("num_ctx").is_none());
    } else {
        assert_eq!(request["options"]["num_ctx"], 4096);
    }
    if request["model"] == "empty:latest" {
        return (
            StatusCode::OK,
            Json(json!({
                "message":{"content":"   "},
                "prompt_eval_count":5,
                "eval_count":3
            })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "message":{"content":"fallback answer"},
            "prompt_eval_count":5,
            "eval_count":3
        })),
    )
}
