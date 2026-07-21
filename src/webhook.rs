//! The GitHub App webhook server: verifies event signatures, decides which
//! events warrant a re-render, and drives the conversion pipeline.

use crate::config::PdfOptions;
use crate::github::{AppAuth, GitHubClient};
use crate::models::WebhookPayload;
use crate::pipeline;
use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Everything the webhook handlers need, shared across requests.
pub struct AppState {
    pub app_auth: Arc<AppAuth>,
    pub webhook_secret: String,
    pub options: PdfOptions,
    /// Login of this app's bot account (e.g. `gh2pdf[bot]`); events it sends
    /// are ignored so description updates do not trigger endless re-renders.
    pub bot_login: String,
    /// Per-issue locks so concurrent events about the same issue/PR are
    /// serialised; each run fetches fresh state, so later runs converge.
    issue_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl AppState {
    pub fn new(
        app_auth: Arc<AppAuth>,
        webhook_secret: String,
        options: PdfOptions,
        bot_login: String,
    ) -> Self {
        Self {
            app_auth,
            webhook_secret,
            options,
            bot_login,
            issue_locks: Mutex::new(HashMap::new()),
        }
    }

    async fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.issue_locks.lock().await;
        locks.entry(key.to_string()).or_default().clone()
    }
}

/// Runs the webhook server until interrupted.
pub async fn serve(state: Arc<AppState>, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/webhook", post(handle_webhook))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    log::info!("Webhook: Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {}", addr))?;
    axum::serve(listener, app).await.context("serving webhook")
}

/// Event/action pairs that change the content of an issue or PR PDF.
fn is_relevant(event: &str, action: Option<&str>) -> bool {
    let action = action.unwrap_or("");
    match event {
        "issues" => matches!(action, "opened" | "edited" | "reopened" | "closed"),
        "issue_comment" => matches!(action, "created" | "edited" | "deleted"),
        "pull_request" => matches!(
            action,
            "opened" | "edited" | "reopened" | "synchronize" | "closed" | "ready_for_review"
        ),
        "pull_request_review" => matches!(action, "submitted" | "edited" | "dismissed"),
        "pull_request_review_comment" => matches!(action, "created" | "edited" | "deleted"),
        _ => false,
    }
}

async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(&state.webhook_secret, &body, signature) {
        log::warn!("Webhook: Rejected request with invalid signature");
        return StatusCode::UNAUTHORIZED;
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Webhook: Could not parse {} payload: {}", event, e);
            return StatusCode::BAD_REQUEST;
        }
    };

    if !is_relevant(&event, payload.action.as_deref()) {
        log::debug!(
            "Webhook: Ignoring {} / {:?}",
            event,
            payload.action.as_deref()
        );
        return StatusCode::NO_CONTENT;
    }

    // Our own description edits come back as `issues`/`pull_request` `edited`
    // events; processing them would loop forever.
    if let Some(sender) = &payload.sender {
        if sender.login == state.bot_login {
            log::debug!("Webhook: Ignoring event from our own bot {}", sender.login);
            return StatusCode::NO_CONTENT;
        }
    }

    let Some((owner, repo, number, installation_id)) = payload.target() else {
        log::warn!("Webhook: {} event without a usable target", event);
        return StatusCode::NO_CONTENT;
    };

    log::info!(
        "Webhook: {} / {:?} for {}/{}#{}",
        event,
        payload.action.as_deref(),
        owner,
        repo,
        number
    );

    tokio::spawn(async move {
        let key = format!("{}/{}#{}", owner, repo, number);
        let lock = state.lock_for(&key).await;
        let _guard = lock.lock().await;

        let github = Arc::new(GitHubClient::for_installation(
            state.app_auth.clone(),
            installation_id,
        ));
        if let Err(e) =
            pipeline::convert_and_publish(github, &state.options, &owner, &repo, number).await
        {
            log::error!("Webhook: Conversion failed for {}: {:#}", key, e);
        }
    });

    StatusCode::ACCEPTED
}

/// Verifies GitHub's `X-Hub-Signature-256` HMAC over the raw request body.
fn verify_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn test_verify_signature_accepts_valid() {
        let secret = "s3cret";
        let body = b"{\"action\":\"opened\"}";
        assert!(verify_signature(secret, body, &sign(secret, body)));
    }

    #[test]
    fn test_verify_signature_rejects_invalid() {
        let secret = "s3cret";
        let body = b"{}";
        assert!(!verify_signature(secret, body, &sign("wrong", body)));
        assert!(!verify_signature(secret, body, "sha256=nothex"));
        assert!(!verify_signature(secret, body, "sha1=abcd"));
        assert!(!verify_signature(secret, body, ""));
    }

    #[test]
    fn test_is_relevant() {
        assert!(is_relevant("issues", Some("opened")));
        assert!(is_relevant("issues", Some("edited")));
        assert!(is_relevant("issue_comment", Some("created")));
        assert!(is_relevant("pull_request", Some("synchronize")));
        assert!(is_relevant("pull_request_review", Some("submitted")));
        assert!(is_relevant("pull_request_review_comment", Some("deleted")));

        assert!(!is_relevant("issues", Some("labeled")));
        assert!(!is_relevant("pull_request", Some("assigned")));
        assert!(!is_relevant("push", Some("created")));
        assert!(!is_relevant("issues", None));
    }

    #[test]
    fn test_payload_target_extraction() {
        let payload: WebhookPayload = serde_json::from_str(
            r#"{
                "action": "opened",
                "issue": {"number": 42},
                "repository": {"name": "gh2pdf", "owner": {"login": "iesahin", "type": "User"}},
                "installation": {"id": 123},
                "sender": {"login": "someone", "type": "User"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            payload.target(),
            Some(("iesahin".to_string(), "gh2pdf".to_string(), 42, 123))
        );
    }

    #[test]
    fn test_payload_target_from_pull_request() {
        let payload: WebhookPayload = serde_json::from_str(
            r#"{
                "action": "synchronize",
                "pull_request": {"number": 5},
                "repository": {"name": "r", "owner": {"login": "o", "type": "User"}},
                "installation": {"id": 9},
                "sender": {"login": "someone", "type": "User"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            payload.target(),
            Some(("o".to_string(), "r".to_string(), 5, 9))
        );
    }
}
