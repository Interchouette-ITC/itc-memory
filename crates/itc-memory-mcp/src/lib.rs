//! CLI discovery over MCP stdio and Streamable HTTP.

mod cli_help;

pub use cli_help::CliHelpServer;

use std::{env, fmt::Display, future::Future, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    transport::{
        self,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Reads the HTTP transport bind address and token.
///
/// # Errors
/// Returns an error when the address or token is invalid.
pub fn http_settings_from_environment() -> anyhow::Result<(SocketAddr, String)> {
    let bind = env::var("ITC_MEMORY_MCP_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8765".to_owned())
        .parse()
        .map_err(|error| anyhow::anyhow!("ITC_MEMORY_MCP_BIND is invalid: {error}"))?;
    let token = env::var("ITC_MEMORY_MCP_TOKEN")
        .map_err(|_| anyhow::anyhow!("ITC_MEMORY_MCP_TOKEN is required for HTTP"))?;
    if token.len() < 32 {
        anyhow::bail!("ITC_MEMORY_MCP_TOKEN must contain at least 32 characters");
    }
    Ok((bind, token))
}

/// Serves CLI discovery over MCP stdio.
///
/// # Errors
/// Returns an error when the MCP transport fails.
pub async fn run_stdio(server: impl ServerHandler) -> anyhow::Result<()> {
    server.serve(transport::stdio()).await?.waiting().await?;
    Ok(())
}

/// Serves CLI discovery over authenticated Streamable HTTP.
///
/// # Errors
/// Returns an error when binding or serving the HTTP listener fails.
pub async fn run_http(
    server: impl ServerHandler + Clone,
    bind: SocketAddr,
    token: String,
) -> anyhow::Result<()> {
    validate_http_bind(bind)?;
    if token.len() < 32 {
        anyhow::bail!("HTTP bearer token must contain at least 32 characters");
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let auth = HttpAuth {
        token: Arc::from(token),
        bound_address: bind,
    };
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth, authorize_http));
    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn validate_http_bind(bind: SocketAddr) -> anyhow::Result<()> {
    if !bind.ip().is_loopback() {
        anyhow::bail!(
            "the MCP HTTP listener must bind to loopback; use a TLS-terminating reverse proxy for remote clients"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct HttpAuth {
    token: Arc<str>,
    bound_address: SocketAddr,
}

async fn authorize_http(
    State(auth): State<HttpAuth>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let headers = request.headers();
    if !valid_bearer_token(headers, &auth.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !safe_http_origin(headers, auth.bound_address) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn valid_bearer_token(headers: &HeaderMap, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

fn safe_http_origin(headers: &HeaderMap, bound_address: SocketAddr) -> bool {
    if headers.contains_key(header::ORIGIN) || !bound_address.ip().is_loopback() {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let acceptable_hosts = [
        format!("127.0.0.1:{}", bound_address.port()),
        format!("localhost:{}", bound_address.port()),
        format!("[::1]:{}", bound_address.port()),
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "[::1]".to_owned(),
    ];
    acceptable_hosts
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
}

async fn cancellable<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ErrorData> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ErrorData::internal_error("request was cancelled", None)),
        result = future => Ok(result),
    }
}

fn internal_error(error: impl Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_cancellation_drops_the_active_future() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let cancellation = CancellationToken::new();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (dropped_sender, dropped_receiver) = tokio::sync::oneshot::channel();
        let future = async move {
            let _signal = DropSignal(Some(dropped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        };
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { cancellable(&task_cancellation, future).await });

        started_receiver.await.unwrap();
        cancellation.cancel();
        assert!(task.await.unwrap().is_err());
        dropped_receiver.await.unwrap();
    }

    #[test]
    fn loopback_http_rejects_browser_origins_and_untrusted_hosts() {
        let address: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:8765".parse().unwrap());
        assert!(safe_http_origin(&headers, address));
        headers.insert(header::ORIGIN, "http://localhost:8765".parse().unwrap());
        assert!(!safe_http_origin(&headers, address));
        headers.remove(header::ORIGIN);
        headers.insert(
            header::HOST,
            "localhost.attacker.test:8765".parse().unwrap(),
        );
        assert!(!safe_http_origin(&headers, address));
    }

    #[test]
    fn http_listener_rejects_non_loopback_bind_addresses() {
        assert!(validate_http_bind("127.0.0.1:8765".parse().unwrap()).is_ok());
        assert!(validate_http_bind("0.0.0.0:8765".parse().unwrap()).is_err());
    }

    #[test]
    fn http_requires_the_exact_configured_bearer_token() {
        let mut headers = HeaderMap::new();
        assert!(!valid_bearer_token(&headers, "configured-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer wrong-secret".parse().unwrap(),
        );
        assert!(!valid_bearer_token(&headers, "configured-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer configured-secret".parse().unwrap(),
        );
        assert!(valid_bearer_token(&headers, "configured-secret"));
    }
}
