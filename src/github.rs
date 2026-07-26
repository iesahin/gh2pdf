//! All GitHub access, behind the `GitHubProvider` trait (same approach as
//! inboxbot: production code uses `GitHubClient`, tests use
//! `MockGitHubClient`). The client hides GitHub App authentication (App JWT →
//! cached installation tokens), pagination, and the release/asset workflow
//! behind a handful of simple methods.

use crate::models::{Issue, IssueComment, PullRequest, ReviewComment};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::EncodingKey;
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const API_ROOT: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("gh2pdf/", env!("CARGO_PKG_VERSION"));

#[async_trait]
pub trait GitHubProvider: Send + Sync {
    async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<Issue>;

    async fn get_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>>;

    async fn get_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewComment>>;

    async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<PullRequest>>;

    async fn get_pull_request_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String>;

    /// Replaces the description (body) of an issue or PR.
    async fn update_issue_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()>;

    /// Returns the raw content of a file in the repository's default branch,
    /// or None when the file does not exist.
    async fn get_repo_file(&self, owner: &str, repo: &str, path: &str) -> Result<Option<String>>;

    /// Publishes `content` as asset `filename` on the release tagged `tag`,
    /// creating the release when it does not exist and deleting any existing
    /// assets whose name starts with `stale_prefix` (so a renamed issue does
    /// not leave its old PDF behind). Returns the asset's download URL.
    async fn publish_release_asset(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
        filename: &str,
        stale_prefix: &str,
        content: Vec<u8>,
    ) -> Result<String>;

    /// Returns an API token usable for authenticated downloads (images,
    /// attachments) while rendering the PDF.
    async fn token(&self) -> Result<String>;
}

// ---------------------------------------------------------------------------
// GitHub App authentication
// ---------------------------------------------------------------------------

struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

/// Authenticates as a GitHub App: mints short-lived App JWTs and exchanges
/// them for per-installation access tokens, cached until shortly before
/// expiry.
pub struct AppAuth {
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    cache: Mutex<HashMap<u64, CachedToken>>,
}

#[derive(serde::Serialize)]
struct AppJwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

impl AppAuth {
    pub fn new(app_id: String, private_key_pem: &[u8]) -> Result<Self> {
        let key = EncodingKey::from_rsa_pem(private_key_pem)
            .context("parsing GitHub App private key (expected an RSA PEM)")?;
        Ok(Self {
            app_id,
            key,
            http: http_client(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn app_jwt(&self) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AppJwtClaims {
            // Backdate to absorb clock drift, as GitHub recommends.
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.clone(),
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &self.key).context("signing GitHub App JWT")
    }

    pub async fn installation_token(&self, installation_id: u64) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.get(&installation_id) {
            if cached.expires_at > Utc::now() + Duration::minutes(2) {
                return Ok(cached.token.clone());
            }
        }

        log::debug!(
            "GitHub: Requesting access token for installation {}",
            installation_id
        );
        let jwt = self.app_jwt()?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            API_ROOT, installation_id
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "Failed to create installation token for {}: HTTP {} - {}",
                installation_id,
                status,
                text
            );
        }

        let token_resp: InstallationTokenResponse = resp.json().await?;
        let token = token_resp.token.clone();
        cache.insert(
            installation_id,
            CachedToken {
                token: token_resp.token,
                expires_at: token_resp.expires_at,
            },
        );
        Ok(token)
    }
}

// ---------------------------------------------------------------------------
// REST client
// ---------------------------------------------------------------------------

enum Auth {
    /// A personal access token (CLI usage).
    Token(String),
    /// A GitHub App installation (webhook server usage).
    App {
        auth: Arc<AppAuth>,
        installation_id: u64,
    },
}

pub struct GitHubClient {
    http: reqwest::Client,
    auth: Auth,
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Deserialize)]
struct Release {
    id: u64,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct UploadedAsset {
    browser_download_url: String,
}

impl GitHubClient {
    pub fn with_token(token: String) -> Self {
        Self {
            http: http_client(),
            auth: Auth::Token(token),
        }
    }

    pub fn for_installation(auth: Arc<AppAuth>, installation_id: u64) -> Self {
        Self {
            http: http_client(),
            auth: Auth::App {
                auth,
                installation_id,
            },
        }
    }

    async fn current_token(&self) -> Result<String> {
        match &self.auth {
            Auth::Token(t) => Ok(t.clone()),
            Auth::App {
                auth,
                installation_id,
            } => auth.installation_token(*installation_id).await,
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let token = self.current_token().await?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28"))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .request(reqwest::Method::GET, url, "application/vnd.github+json")
            .await?
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("GET {} failed: HTTP {} - {}", url, status, text);
        }
        Ok(resp.json().await?)
    }

    /// Fetches every page of a list endpoint (100 items per page).
    async fn get_all_pages<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<Vec<T>> {
        let mut items = Vec::new();
        for page in 1.. {
            let page_url = format!("{}?per_page=100&page={}", url, page);
            let mut batch: Vec<T> = self.get_json(&page_url).await?;
            let len = batch.len();
            items.append(&mut batch);
            if len < 100 {
                break;
            }
        }
        Ok(items)
    }
}

#[async_trait]
impl GitHubProvider for GitHubClient {
    async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<Issue> {
        let url = format!("{}/repos/{}/{}/issues/{}", API_ROOT, owner, repo, number);
        self.get_json(&url).await
    }

    async fn get_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            API_ROOT, owner, repo, number
        );
        self.get_all_pages(&url).await
    }

    async fn get_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/comments",
            API_ROOT, owner, repo, number
        );
        self.get_all_pages(&url).await
    }

    async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<PullRequest>> {
        let url = format!("{}/repos/{}/{}/pulls/{}", API_ROOT, owner, repo, number);
        let resp = self
            .request(reqwest::Method::GET, &url, "application/vnd.github+json")
            .await?
            .send()
            .await?;
        match resp.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(resp.json().await?)),
            status => {
                let text = resp.text().await.unwrap_or_default();
                bail!("GET {} failed: HTTP {} - {}", url, status, text)
            }
        }
    }

    async fn get_pull_request_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        let url = format!("{}/repos/{}/{}/pulls/{}", API_ROOT, owner, repo, number);
        let resp = self
            .request(reqwest::Method::GET, &url, "application/vnd.github.v3.diff")
            .await?
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("Failed to get PR diff: HTTP {} - {}", status, text);
        }
        let diff = resp.text().await?;
        log::debug!("GitHub: Fetched diff ({} bytes)", diff.len());
        Ok(diff)
    }

    async fn update_issue_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let url = format!("{}/repos/{}/{}/issues/{}", API_ROOT, owner, repo, number);
        let resp = self
            .request(reqwest::Method::PATCH, &url, "application/vnd.github+json")
            .await?
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("PATCH {} failed: HTTP {} - {}", url, status, text);
        }
        Ok(())
    }

    async fn get_repo_file(&self, owner: &str, repo: &str, path: &str) -> Result<Option<String>> {
        let url = format!("{}/repos/{}/{}/contents/{}", API_ROOT, owner, repo, path);
        let resp = self
            .request(
                reqwest::Method::GET,
                &url,
                "application/vnd.github.raw+json",
            )
            .await?
            .send()
            .await?;
        match resp.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(resp.text().await?)),
            status => {
                let text = resp.text().await.unwrap_or_default();
                bail!("GET {} failed: HTTP {} - {}", url, status, text)
            }
        }
    }

    async fn publish_release_asset(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
        filename: &str,
        stale_prefix: &str,
        content: Vec<u8>,
    ) -> Result<String> {
        log::debug!(
            "GitHub: Publishing {} to release {} in {}/{}",
            filename,
            tag,
            owner,
            repo
        );

        // 1. Find or create the dedicated release.
        let by_tag_url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            API_ROOT, owner, repo, tag
        );
        let resp = self
            .request(
                reqwest::Method::GET,
                &by_tag_url,
                "application/vnd.github+json",
            )
            .await?
            .send()
            .await?;
        let release: Release = match resp.status() {
            StatusCode::NOT_FOUND => {
                log::info!("GitHub: Release {} not found, creating...", tag);
                let create_url = format!("{}/repos/{}/{}/releases", API_ROOT, owner, repo);
                let resp = self
                    .request(
                        reqwest::Method::POST,
                        &create_url,
                        "application/vnd.github+json",
                    )
                    .await?
                    .json(&serde_json::json!({
                        "tag_name": tag,
                        "name": format!("gh2pdf PDFs ({})", tag),
                        "body": "PDF renderings of issues and pull requests, generated by gh2pdf.",
                        "prerelease": false,
                    }))
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    bail!(
                        "Creating release {} failed: HTTP {} - {}",
                        tag,
                        status,
                        text
                    );
                }
                resp.json().await?
            }
            status if status.is_success() => resp.json().await?,
            status => {
                let text = resp.text().await.unwrap_or_default();
                bail!("GET {} failed: HTTP {} - {}", by_tag_url, status, text)
            }
        };

        // 2. Delete assets superseded by this upload (same name, or the same
        //    issue under an older title).
        for asset in &release.assets {
            if asset.name == filename || asset.name.starts_with(stale_prefix) {
                log::debug!("GitHub: Deleting stale asset {}", asset.name);
                let delete_url = format!(
                    "{}/repos/{}/{}/releases/assets/{}",
                    API_ROOT, owner, repo, asset.id
                );
                let resp = self
                    .request(
                        reqwest::Method::DELETE,
                        &delete_url,
                        "application/vnd.github+json",
                    )
                    .await?
                    .send()
                    .await?;
                if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
                    log::warn!(
                        "GitHub: Could not delete stale asset {}: HTTP {}",
                        asset.name,
                        resp.status()
                    );
                }
            }
        }

        // 3. Upload the new asset.
        let upload_url = format!(
            "https://uploads.github.com/repos/{}/{}/releases/{}/assets?name={}",
            owner,
            repo,
            release.id,
            urlencode(filename)
        );
        let token = self.current_token().await?;
        let resp = self
            .http
            .post(&upload_url)
            .bearer_auth(token)
            .header("Content-Type", "application/pdf")
            .header("Accept", "application/vnd.github+json")
            .body(content)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "Uploading asset {} failed: HTTP {} - {}",
                filename,
                status,
                text
            );
        }
        let asset: UploadedAsset = resp.json().await?;
        Ok(asset.browser_download_url)
    }

    async fn token(&self) -> Result<String> {
        self.current_token().await
    }
}

/// Percent-encodes a filename for use as a query parameter value.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Parses a GitHub issue/PR URL into (owner, repo, number).
pub fn parse_issue_url(url: &str) -> Result<(String, String, u64)> {
    let stripped = url
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")
        .ok_or_else(|| anyhow!("Not a GitHub URL: {}", url))?;
    let parts: Vec<&str> = stripped.split('/').collect();
    match parts.as_slice() {
        [owner, repo, kind, number] if *kind == "issues" || *kind == "pull" => {
            let number: u64 = number
                .parse()
                .with_context(|| format!("Invalid issue number in URL: {}", url))?;
            Ok((owner.to_string(), repo.to_string(), number))
        }
        _ => bail!(
            "Expected https://github.com/<owner>/<repo>/issues|pull/<number>: {}",
            url
        ),
    }
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Recorded arguments of a `publish_release_asset` call:
    /// (tag, filename, stale_prefix, content length).
    pub type PublishCall = (String, String, String, usize);

    #[derive(Default)]
    pub struct MockGitHubClient {
        pub issue: StdMutex<Option<Issue>>,
        pub issue_comments: StdMutex<Vec<IssueComment>>,
        pub review_comments: StdMutex<Vec<ReviewComment>>,
        pub pull_request: StdMutex<Option<PullRequest>>,
        pub diff: StdMutex<String>,
        pub repo_files: StdMutex<HashMap<String, String>>,
        pub publish_calls: StdMutex<Vec<PublishCall>>,
        pub body_updates: StdMutex<Vec<(u64, String)>>,
    }

    #[async_trait]
    impl GitHubProvider for MockGitHubClient {
        async fn get_issue(&self, _owner: &str, _repo: &str, _number: u64) -> Result<Issue> {
            self.issue
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no issue configured"))
        }

        async fn get_issue_comments(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<Vec<IssueComment>> {
            Ok(self.issue_comments.lock().unwrap().clone())
        }

        async fn get_review_comments(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<Vec<ReviewComment>> {
            Ok(self.review_comments.lock().unwrap().clone())
        }

        async fn get_pull_request(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<Option<PullRequest>> {
            Ok(self.pull_request.lock().unwrap().clone())
        }

        async fn get_pull_request_diff(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<String> {
            Ok(self.diff.lock().unwrap().clone())
        }

        async fn update_issue_body(
            &self,
            _owner: &str,
            _repo: &str,
            number: u64,
            body: &str,
        ) -> Result<()> {
            self.body_updates
                .lock()
                .unwrap()
                .push((number, body.to_string()));
            Ok(())
        }

        async fn get_repo_file(
            &self,
            _owner: &str,
            _repo: &str,
            path: &str,
        ) -> Result<Option<String>> {
            Ok(self.repo_files.lock().unwrap().get(path).cloned())
        }

        async fn publish_release_asset(
            &self,
            owner: &str,
            repo: &str,
            tag: &str,
            filename: &str,
            stale_prefix: &str,
            content: Vec<u8>,
        ) -> Result<String> {
            self.publish_calls.lock().unwrap().push((
                tag.to_string(),
                filename.to_string(),
                stale_prefix.to_string(),
                content.len(),
            ));
            Ok(format!(
                "https://github.com/{}/{}/releases/download/{}/{}",
                owner, repo, tag, filename
            ))
        }

        async fn token(&self) -> Result<String> {
            Ok("mock-token".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway 2048-bit RSA key, generated with `openssl genrsa -traditional
    // 2048`, used only to exercise EncodingKey/jsonwebtoken signing in tests.
    const TEST_RSA_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAns6Be90E/5eDzcwHmHEVduH5BikbIxeQ3ndqGQzibear26Do
BRB1EehF/xgB155+1THE6+g6RFFTtpSRNNXcKS/7SnFgItHO2XF2YlLKp69yR3Gh
wrno6/ZlaPN4b4yMBUPCDk6dfSWZYHXCl5YpXEarrJTAtHbsBrtXmNrEd3mnjFM1
+QbYBw/NBoI3nrR+JIFeUEULWe0PGGcI3NtqfwUTq4+FVOPLqY+zYnpwzV0Vtloy
g1pIUgIgn1aNBiYKlLATvvwRCrqjdfpNK3afZHtm8Rz78z79NZz5MDCm3Xt38YcZ
SK/kAOR4jWxNu4uv2ZPl56LmT4kCKX0lS5xCKwIDAQABAoIBAAe3cBpFMJd81MY8
ukfBgvH+Y/vVJoDrhboRomGqOxxs/3/SD0QjuxSOLUyKbZh9FpukafWumJo2O3Od
P3sKQ0LrFnJVFP9MI5l0RnTbogZI46wuDNap8vP4SpAxeHIvKaSd2MGaN1Pb7lp+
DmEQRl05/+CIb961Ap4HH2gJhU9q0cwlmcVaVVBhZT5pLK2/SHG0yFOu7Pu9HJ0w
BWg69/tJqwxvBKDRQS4OXxS/8YsXd0g7ksKrAWO/TnmATrvfmHF51o545majC6mG
5Wb6DKZz5I+9tOOrVBomL5L0zRKfWNZVS9lEJ8aR0tqJWKxGLMLRoTgr0osW3Ygx
GhaTRnECgYEAznpwy/2FmBOpXTBJOrNj8xraUYg0dCO4bKO3E4jz3rEsAGWe7VrL
UuqQzRrN+aBz3EKk6mMOsks5kLiZjlPrSt9kVVybmMT/u8lIlw+ScmIQrseEqb+O
Q20oGXdZ3UrHwNMDeIvndX+93SEK2ytObCrK785gFJXpLZUyTbh7VxsCgYEAxOUT
VmlO8+zpdim2q2kgiQdiJ1AQNyxj9KFAnIR4Ot8eoBEoqQhfxvrr/zsr0aaerGSQ
/IptlgvfVay28mHQSbDaEgtwb/L5ORAhlDHhtrdeszUwkXAqdCCe3AM/2p8rN6W3
6BP3A5GK0ZOsy7SgtAcFaTt2nOqWvJQUoxs9IjECgYEAlhYeY8lXEKJKG/j7YfYA
EzhTtaxCJKHKbv3aGBMW4ar7hxZXHcU/wnfK5aw0SN2/Gj4/TjjO9/8CSxZEWFbb
08LqVbpJSBT6p2+6mkOxef+ajNFut00MhiqUWV6OLfMrnBhGj5tyldBTHKfmEkY6
bRn2BbaH1K7bnkyzEhelYD0CgYAyT35zdBEyjvTQtrPwdLpViUdxWCnsjzEzTwjd
dZPrJxwCNqA3IOaoR3GKFCqMNZER59iMTyrVTk9Q6wMMSCYazk/KkJW4ZVN9WzvZ
TC2qrIxMKmkwoIKYjcVJ3qKwUD+Qxo2JhaB2jvfzuVJL8umlVq3xR7p1OhQuN4BW
dR1X4QKBgBJWMbY2syN4Bo8QjYsj+exK7OyFI2HaLI9NceWmULTIrgXBCo/sqyjD
C7vsrPf86/vPs8qYcYEx6wptUMlYUYQ0ooE4msg5VTr5iXMbHkkItwylWs5aiSVn
M52l/Si9+gn03soMPRNvRzIQaDU5cFqTPoX1db2RydXmCZBCthMM
-----END RSA PRIVATE KEY-----";

    // Regression test: jsonwebtoken 10 dropped its bundled crypto backend and
    // panics at signing time ("Could not automatically determine the
    // process-level CryptoProvider") unless a backend feature is enabled in
    // Cargo.toml. Cargo won't catch a missing feature; only exercising the
    // signing path does.
    #[test]
    fn test_app_jwt_signs_without_crypto_provider_panic() {
        let auth = AppAuth::new("123456".to_string(), TEST_RSA_PEM.as_bytes()).unwrap();
        let jwt = auth.app_jwt().unwrap();
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn test_parse_issue_url() {
        assert_eq!(
            parse_issue_url("https://github.com/iesahin/gh2pdf/issues/12").unwrap(),
            ("iesahin".to_string(), "gh2pdf".to_string(), 12)
        );
        assert_eq!(
            parse_issue_url("https://github.com/iesahin/gh2pdf/pull/3/").unwrap(),
            ("iesahin".to_string(), "gh2pdf".to_string(), 3)
        );
        assert!(parse_issue_url("https://github.com/iesahin/gh2pdf").is_err());
        assert!(parse_issue_url("https://example.com/a/b/issues/1").is_err());
        assert!(parse_issue_url("https://github.com/a/b/commit/abc").is_err());
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("simple-file_1.pdf"), "simple-file_1.pdf");
        assert_eq!(urlencode("a b#c.pdf"), "a%20b%23c.pdf");
    }
}
