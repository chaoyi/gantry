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

    // Send initial snapshot
    let snapshot = build_snapshot(&state).await;
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let _ = sender.send(Message::Text(json.into())).await;
    }

    // Forward events as WsEvent
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
                    tracing::warn!("ws: skipped {n} events (slow consumer), resending snapshot");
                    let snapshot = build_snapshot(&state_ref).await;
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

    // Consume incoming messages (ignore them, just detect disconnect)
    let recv_task = tokio::spawn(async move { while let Some(Ok(_)) = receiver.next().await {} });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}

async fn build_snapshot(state: &AppState) -> serde_json::Value {
    use crate::model::{ProbeDisplayState, SvcDisplayState};

    let services = state.services.read().await;
    let targets = state.targets.read().await;

    // Same map format the UI expects for efficient state merge by name
    let mut svc_states = serde_json::Map::new();
    for (name, svc) in services.iter() {
        let mut probes = serde_json::Map::new();
        for (probe_name, probe_rt) in &svc.probes {
            let display = ProbeDisplayState::from_probe(probe_rt, svc.state);
            probes.insert(
                probe_name.clone(),
                serde_json::json!({ "state": display.as_str() }),
            );
        }
        let svc_display = SvcDisplayState::from_service(svc);
        svc_states.insert(
            name.clone(),
            serde_json::json!({
                "state": svc_display.as_str(),
                "runtime": svc.state.as_str(),
                "probes": probes,
            }),
        );
    }

    let mut tgt_states = serde_json::Map::new();
    for (name, tgt) in targets.iter() {
        let s = tgt.state(&services);
        tgt_states.insert(name.clone(), serde_json::json!({ "state": s.as_str() }));
    }

    serde_json::json!({
        "type": "snapshot",
        "services": svc_states,
        "targets": tgt_states,
    })
}
