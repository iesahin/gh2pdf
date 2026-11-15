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
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Everything the webhook handlers need, shared across requests.
pub struct AppState {
    pub app_auth: Arc<AppAuth>,
    pub webhook_secret: String,
    pub options: PdfOptions,
    /// Login of this app's bot account (e.g. `gh2pdf[bot]`); events it sends
    /// are ignored so description updates do not trigger endless re-renders.
    pub bot_login: String,
    /// How long an issue/PR must stay quiet before it is rendered; every new
    /// event about it within this window restarts the wait.
    pub quiet_period: Duration,
    debouncer: Debouncer,
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
        quiet_period: Duration,
    ) -> Self {
        Self {
            app_auth,
            webhook_secret,
            options,
            bot_login,
            quiet_period,
            debouncer: Debouncer::default(),
            issue_locks: Mutex::new(HashMap::new()),
        }
    }

    async fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.issue_locks.lock().await;
        locks.entry(key.to_string()).or_default().clone()
    }
}

/// Trailing-edge debouncer keyed by issue: `schedule` runs a job only once no
/// newer job has been scheduled under the same key for the whole delay.
/// Scheduling again within the delay supersedes (drops) the earlier job, so a
/// burst of edits produces a single conversion after the burst ends.
#[derive(Default, Clone)]
struct Debouncer {
    generations: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl Debouncer {
    fn bump(&self, key: &str) -> u64 {
        let mut g = self.generations.lock().unwrap_or_else(|e| e.into_inner());
        let gen = g.entry(key.to_string()).or_insert(0);
        *gen += 1;
        *gen
    }

    /// True (and forgets the key) when `gen` is still the latest job for it.
    fn claim(&self, key: &str, gen: u64) -> bool {
        let mut g = self.generations.lock().unwrap_or_else(|e| e.into_inner());
        if g.get(key) == Some(&gen) {
            g.remove(key);
            true
        } else {
            false
        }
    }

    fn schedule<F>(&self, key: String, delay: Duration, job: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let gen = self.bump(&key);
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if this.claim(&key, gen) {
                job.await;
            } else {
                log::debug!("Webhook: {} superseded by a newer event", key);
            }
        });
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

    let key = format!("{}/{}#{}", owner, repo, number);
    let delay = state.quiet_period;
    let job_state = state.clone();
    let job_key = key.clone();
    let job = async move {
        let (state, key) = (job_state, job_key);
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
    };
    // Each event restarts the quiet-period timer; only the last event of a
    // burst renders, and it fetches fresh state, so nothing is lost.
    state.debouncer.schedule(key, delay, job);

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

    async fn run_debounced(schedule_at: &[u64], keys: &[&str]) -> Vec<String> {
        let debouncer = Debouncer::default();
        let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut last = 0;
        for (i, (&at, &key)) in schedule_at.iter().zip(keys).enumerate() {
            tokio::time::sleep(Duration::from_secs(at - last)).await;
            last = at;
            let ran = ran.clone();
            let label = format!("{}:{}", key, i);
            debouncer.schedule(key.to_string(), Duration::from_secs(120), async move {
                ran.lock().unwrap().push(label);
            });
        }
        tokio::time::sleep(Duration::from_secs(1000)).await;
        let out = ran.lock().unwrap().clone();
        assert!(debouncer.generations.lock().unwrap().is_empty());
        out
    }

    #[tokio::test(start_paused = true)]
    async fn test_debouncer_runs_only_last_event_of_a_burst() {
        // Events 100 s apart keep resetting the 120 s timer.
        let ran = run_debounced(&[0, 100, 200], &["a", "a", "a"]).await;
        assert_eq!(ran, vec!["a:2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_debouncer_runs_each_event_after_quiet_period() {
        let ran = run_debounced(&[0, 121], &["a", "a"]).await;
        assert_eq!(ran, vec!["a:0", "a:1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_debouncer_keys_are_independent() {
        let ran = run_debounced(&[0, 10, 20], &["a", "b", "a"]).await;
        assert_eq!(ran, vec!["b:1", "a:2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_debouncer_waits_full_quiet_period() {
        let debouncer = Debouncer::default();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        debouncer.schedule("a".into(), Duration::from_secs(120), async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_secs(119)).await;
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
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
