use axum::http;
use engine_types::HttpContext;
use hasura_authn::{AuthError, ResolvedAuthConfig, authenticate};
use hasura_authn_core::{Session, SessionError, authorize_identity};
use std::collections::HashMap;

use super::types::{ConnectionInitState, InitPayload, ServerMessage};
use crate::metrics::WebSocketMetrics;
use crate::websocket::types as ws;

/// Handles the connection initialization message from the client.
/// This function authenticates, authorizes, and initializes the WebSocket connection.
pub async fn handle_connection_init<M: WebSocketMetrics>(
    connection: ws::Connection<M>,
    payload: Option<InitPayload>,
) {
    let tracer = tracing_util::global_tracer();
    tracer
        .in_span_async(
            "handle_connection_init",
            "Handling connection_init protocol message",
            tracing_util::SpanVisibility::User,
            || {
                Box::pin(async move {
                    let context = &connection.context;
                    let mut state = connection.protocol_init_state.write().await;

                    match initialize(
                        &state,
                        &context.http_context,
                        &context.handshake_headers,
                        &context.auth_config,
                        &context.auth_mode_header,
                        payload,
                    )
                    .await
                    {
                        Ok((session, headers)) => {
                            // Update state to Initialized and send a connection acknowledgment
                            *state = ConnectionInitState::Initialized { session, headers };
                            connection
                                .send(ws::Message::Protocol(Box::new(
                                    ServerMessage::ConnectionAck,
                                )))
                                .await;
                        }
                        Err(ConnectionInitError::AlreadyInitialized) => {
                            // If already initialized, send an error for too many initialization requests
                            connection.send(ws::Message::too_many_init_requests()).await;
                        }
                        Err(_e) => {
                            // Initialization failed, send a forbidden message
                            connection.send(ws::Message::forbidden()).await;
                        }
                    }
                    tracing_util::Successful::new(())
                })
            },
        )
        .await
        .into_inner();
}

/// Performs the initialization process by validating the payload, authenticating, and authorizing.
/// It returns a session and the headers if the initialization is successful.
async fn initialize(
    init_state: &ConnectionInitState,
    http_context: &HttpContext,
    client_headers: &http::HeaderMap,
    auth_config: &ResolvedAuthConfig,
    auth_mode_header: &str,
    payload: Option<InitPayload>,
) -> Result<(Session, http::HeaderMap), ConnectionInitError> {
    let tracer = tracing_util::global_tracer();
    tracer
        .in_span_async(
            "initialize",
            "Intialiizing graphql-ws protocol",
            tracing_util::SpanVisibility::User,
            || {
                Box::pin(async {
                    match init_state {
                        ConnectionInitState::NotInitialized => {
                            // Parse the headers from the payload
                            let mut headers = match payload {
                                Some(payload) => parse_headers(payload.headers)?,
                                None => http::HeaderMap::new(),
                            };
                            // Extend the headers with the client headers received from the handshake request.
                            // NOTE: In case of conflicts, headers provided during the handshake will take precedence over those in the `connection_init` payload.
                            // The `connection_init` message is sent while the WebSocket session is still unauthenticated, meaning clients could manipulate sensitive
                            // headers injected by an upstream gateway or proxy during the initial handshake. By prioritizing handshake headers, we ensure that any
                            // dynamically injected authentication or routing information remains intact.
                            headers.extend(client_headers.clone());
                            // Authenticate the client based on headers and context
                            let identity = authenticate(
                                &headers,
                                &http_context.client,
                                auth_config,
                                auth_mode_header,
                            )
                            .await?;
                            // Authorize the authenticated identity
                            let session = authorize_identity(&identity, &headers)?;
                            Ok((session, headers))
                        }
                        ConnectionInitState::Initialized { .. } => {
                            Err(ConnectionInitError::AlreadyInitialized)
                        }
                    }
                })
            },
        )
        .await
}

/// Error types that may occur during connection initialization.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionInitError {
    #[error("Connection already initialized")]
    AlreadyInitialized,
    #[error("Invalid header name: {0}")]
    InvalidHeaderName(#[from] http::header::InvalidHeaderName),
    #[error("Invalid header value: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("AuthError: {0}")]
    Authn(#[from] AuthError),
    #[error("SessionError: {0}")]
    Session(#[from] SessionError),
}

impl tracing_util::TraceableError for ConnectionInitError {
    fn visibility(&self) -> tracing_util::ErrorVisibility {
        tracing_util::ErrorVisibility::User
    }
}

/// Parses headers from a given map of strings into an `http::HeaderMap`.
/// Returns a parsed header map or an error if the headers are invalid.
fn parse_headers(map: HashMap<String, String>) -> Result<http::HeaderMap, ConnectionInitError> {
    let mut headers = http::HeaderMap::new();
    for (key, value) in map {
        let header_name = http::HeaderName::from_bytes(key.as_bytes())?;
        let header_value = http::HeaderValue::from_str(&value)?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}
