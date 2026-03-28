use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};

use crate::api::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let mut event_rx = state.events.subscribe();

    // Send initial snapshot (same data as GET /graph, map format for UI)
    let snapshot = super::state::build_ws_snapshot(&state).await;
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let _ = sender.send(Message::Text(json.into())).await;
    }

    // Forward events
    let state_ref = state.clone();
    let send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let ws_event = event.to_ws_event();
                    if let Ok(json) = serde_json::to_string(&ws_event)
                        && sender.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ws: skipped {n} events, resending snapshot");
                    let snapshot = super::state::build_ws_snapshot(&state_ref).await;
                    if let Ok(json) = serde_json::to_string(&snapshot)
                        && sender.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let recv_task = tokio::spawn(async move { while let Some(Ok(_)) = receiver.next().await {} });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}
