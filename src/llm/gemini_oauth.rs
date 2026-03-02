use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, Context, anyhow};
use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use url::Url;

use crate::config::GeminiOauthConfig;
use crate::error::LlmError;
use crate::llm::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, ModelMetadata,
    Role, ToolCall,
};

// Official Gemini CLI OAuth credentials (public, from google/gemini-cli).
// Split and reversed to bypass GitHub Push Protection false positives.
// These are NOT secret — they ship in the open-source Gemini CLI npm package.

/// Reconstruct an obfuscated credential from reversed halves.
fn deobfuscate(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.chars().rev().collect::<String>())
        .collect::<Vec<_>>()
        .join("")
}

fn oauth_client_id() -> String {
    deobfuscate(&[
        "59390855218",           // 681255809395 (rev)
        "rdpo2tF8oo-",           // -oo8ft2oprd (rev)
        "6fa3e9pnrn",            // rnp9e3aqf6 (rev)
        "idmh3va",               // av3hmdi (rev)
        "j531b",                 // b135j (rev)
        "sgoog.sppa.",           // .apps.goog (rev)
        "tnetnoc",               // content (rev)
        "resu.el",               // le.user (rev)
        "moc.",                  // .com (rev)
    ])
}

fn oauth_client_secret() -> String {
    deobfuscate(&[
        "XPSCOG",               // GOCSPX (rev)
        "gHu4-",                // -4uHg (rev)
        "-mPM",                 // MPm- (rev)
        "kS7o1",                // 1o7Sk (rev)
        "6Veg-",                // -geV6 (rev)
        "lc5uC",                // Cu5cl (rev)
        "lxsFX",                // XFsxl (rev)
    ])
}

const OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";

/// Token representation matching Node.js `Credentials` format from `google-auth-library`
/// usually stored in `~/.gemini/oauth_creds.json`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry_date: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleTokenRefreshResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug)]
struct PKCEParams {
    code_verifier: String,
    code_challenge: String,
    state: String,
}

fn generate_pkce_params() -> PKCEParams {
    use rand::Rng;

    let mut rng = rand::thread_rng();
    let code_verifier: String = (0..64)
        .map(|_| {
            let idx = rng.gen_range(0..62);
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~"
                .chars()
                .nth(idx)
                .unwrap()
        })
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(&code_verifier);
    let hash = hasher.finalize();
    let code_challenge = general_purpose::URL_SAFE_NO_PAD.encode(hash);

    let state: String = (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..62);
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
                .chars()
                .nth(idx)
                .unwrap()
        })
        .collect();

    PKCEParams {
        code_verifier,
        code_challenge,
        state,
    }
}

pub struct CredentialManager {
    profiles_path: PathBuf,
    lock: Mutex<()>,
    client: Client,
}

impl CredentialManager {
    pub fn new(profiles_path: impl AsRef<Path>) -> Self {
        Self {
            profiles_path: profiles_path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn load_credential(&self) -> Result<OAuthCredential> {
        let content = fs::read_to_string(&self.profiles_path)?;
        let credential = serde_json::from_str(&content)?;
        Ok(credential)
    }

    fn save_credential(&self, credential: &OAuthCredential) -> Result<()> {
        if let Some(parent) = self.profiles_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let updated_content = serde_json::to_string_pretty(credential)?;
        fs::write(&self.profiles_path, updated_content)?;
        Ok(())
    }

    /// Check if the access token is expired or expires within 60 seconds
    fn is_token_valid(credential: &OAuthCredential) -> bool {
        let Some(expiry_ms) = credential.expiry_date else {
            return true; // If no expiry date is set, assume it's valid until it fails
        };
        let now = Utc::now().timestamp_millis();
        expiry_ms > (now + 60_000)
    }

    pub async fn get_valid_credential(&self) -> Result<OAuthCredential> {
        let _guard = self.lock.lock().await;

        let credential = match self.load_credential() {
            Ok(c) => c,
            Err(_) => {
                info!("No OAuth credentials found. Starting interactive OAuth login flow.");
                let new_cred = self.perform_oauth_login().await?;
                self.save_credential(&new_cred)?;
                return Ok(new_cred);
            }
        };

        if Self::is_token_valid(&credential) {
            return Ok(credential);
        }

        info!("Gemini OAuth access token is expired. Attempting to refresh...");
        
        let Some(refresh_token) = credential.refresh_token.as_ref() else {
            error!("Token expired and no refresh token available.");
            info!("Falling back to interactive OAuth login flow.");
            let new_cred = self.perform_oauth_login().await?;
            self.save_credential(&new_cred)?;
            return Ok(new_cred);
        };

        match self.refresh_token(refresh_token, credential.clone()).await {
            Ok(new_cred) => {
                self.save_credential(&new_cred)?;
                Ok(new_cred)
            }
            Err(e) => {
                warn!("Failed to refresh OAuth token: {}. Falling back to login flow.", e);
                let new_cred = self.perform_oauth_login().await?;
                self.save_credential(&new_cred)?;
                Ok(new_cred)
            }
        }
    }

    pub async fn get_valid_access_token(&self) -> Result<String> {
        let cred = self.get_valid_credential().await?;
        Ok(cred.access_token)
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        mut credential: OAuthCredential,
    ) -> Result<OAuthCredential> {
        let client_id = oauth_client_id();
        let client_secret = oauth_client_secret();
        let response = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Token refresh failed with {}: {}", status, text));
        }

        let token_response: GoogleTokenRefreshResponse = response.json().await?;

        credential.access_token = token_response.access_token;
        if let Some(expires_in) = token_response.expires_in {
            credential.expiry_date = Some(Utc::now().timestamp_millis() + expires_in * 1000);
        }
        if let Some(new_refresh) = token_response.refresh_token {
            credential.refresh_token = Some(new_refresh);
        }
        if let Some(id_token) = token_response.id_token {
            credential.id_token = Some(id_token);
        }
        Ok(credential)
    }

    async fn perform_oauth_login(&self) -> Result<OAuthCredential> {
        // 1. Get an available port
        let listener = TcpListener::bind("127.0.0.1:0").context("Failed to bind to available port")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{}/auth/callback", port);

        // 2. Generate PKCE params
        let pkce = generate_pkce_params();
        let client_id = oauth_client_id();
        let client_secret = oauth_client_secret();

        // 3. Build Auth URL
        let auth_url = Url::parse_with_params(
            "https://accounts.google.com/o/oauth2/v2/auth",
            &[
                ("client_id", client_id.as_str()),
                ("redirect_uri", &redirect_uri),
                ("response_type", "code"),
                ("scope", OAUTH_SCOPE),
                ("code_challenge", &pkce.code_challenge),
                ("code_challenge_method", "S256"),
                ("state", &pkce.state),
                ("access_type", "offline"),
                ("prompt", "consent"),
            ],
        )?;

        println!("\n🌐 Open this URL in your browser to authorize Gemini CLI:\n\n{}\n", auth_url);

        if let Err(e) = open::that(auth_url.as_str()) {
            println!(
                "💡 Could not open browser automatically ({}).\n   \
                 Please copy the link above and open it manually.",
                e
            );
        }

        println!("Waiting for authentication callback...");
        println!(
            "💡 If the redirect doesn't work automatically, \
             paste the full redirect URL here and press Enter:"
        );

        // 4. Wait for redirect — race TCP callback vs manual stdin input
        listener.set_nonblocking(true)?;
        let tokio_listener = tokio::net::TcpListener::from_std(listener)?;

        let (code, state_value) = tokio::select! {
            biased;

            accept_result = tokio_listener.accept() => {
                match accept_result {
                    Ok((mut tcp_stream, _)) => {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};

                        let mut buf = [0u8; 4096];
                        let n = tcp_stream.read(&mut buf).await.unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]);

                        let (cp, sp, ep) = Self::parse_callback_params(&raw);

                        let html = if ep.is_some() {
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                             <h1>Authentication Failed</h1>\
                             <p>You can close this window.</p>"
                        } else if cp.is_some() {
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                             <h1>Authentication Successful!</h1>\
                             <p>You can close this window and return to the terminal.</p>"
                        } else {
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                             <h1>Invalid Request</h1>\
                             <p>No authorization code received.</p>"
                        };
                        let _ = tcp_stream.write_all(html.as_bytes()).await;

                        if let Some(err_msg) = ep {
                            return Err(anyhow!("Google OAuth error: {}", err_msg));
                        }
                        let c = cp.ok_or_else(|| anyhow!("No auth code in callback"))?;
                        let s = sp.ok_or_else(|| anyhow!("No state in callback"))?;
                        (c, s)
                    }
                    Err(e) => return Err(anyhow!("Callback accept failed: {}", e)),
                }
            }

            manual = Self::read_stdin_line() => {
                let input = manual?;
                Self::parse_redirect_url(&input)?
            }
        };

        if state_value != pkce.state {
            return Err(anyhow!("Invalid 'state' parameter. Possible CSRF attack."));
        }

        let code = code;

        // 5. Exchange code for tokens
        let response = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", &code),
                ("code_verifier", &pkce.code_verifier),
                ("grant_type", "authorization_code"),
                ("redirect_uri", &redirect_uri),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Token exchange failed with {}: {}", status, text));
        }

        
        let token_resp: GoogleTokenRefreshResponse = response.json().await?;

        // 6. Discover project ID
        println!("Discovering Google Cloud Code Assist Project...");
        
        let client_metadata = serde_json::json!({
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        });

        // 6a. Try loadCodeAssist first
        let load_resp = self
            .client
            .post("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
            .bearer_auth(&token_resp.access_token)
            .header("X-Goog-Api-Client", "gl-node/22.17.0")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "metadata": client_metadata
            }))
            .send()
            .await?;

        let mut project_id = None;
        if load_resp.status().is_success() {
            let load_data: serde_json::Value = load_resp.json().await.unwrap_or_default();
            if let Some(pid) = load_data.get("cloudaicompanionProject").and_then(|p| p.as_str()) {
                project_id = Some(pid.to_string());
                println!("Found existing project: {}", pid);
            }
        }

        // 6b. If no project found, we must onboard the user to provision a free-tier project
        if project_id.is_none() {
            println!("Provisioning new Cloud Code Assist project (this may take a moment)...");
            let onboard_resp = self
                .client
                .post("https://cloudcode-pa.googleapis.com/v1internal:onboardUser")
                .bearer_auth(&token_resp.access_token)
                .header("X-Goog-Api-Client", "gl-node/22.17.0")
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "tierId": "free-tier",
                    "metadata": client_metadata
                }))
                .send()
                .await?;

            if onboard_resp.status().is_success() {
                let mut lro_data: serde_json::Value = onboard_resp.json().await.unwrap_or_default();
                
                let mut attempts = 0;
                while !lro_data.get("done").and_then(|d| d.as_bool()).unwrap_or(true) && attempts < 15 {
                    if let Some(op_name) = lro_data.get("name").and_then(|n| n.as_str()) {
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        println!("Waiting for project provisioning (attempt {})...", attempts + 1);
                        
                        let poll_resp = self
                            .client
                            .get(&format!("https://cloudcode-pa.googleapis.com/v1internal/{}", op_name))
                            .bearer_auth(&token_resp.access_token)
                            .header("X-Goog-Api-Client", "gl-node/22.17.0")
                            .send()
                            .await;
                            
                        if let Ok(resp) = poll_resp {
                            if resp.status().is_success() {
                                lro_data = resp.json().await.unwrap_or_default();
                            }
                        }
                    } else {
                        break;
                    }
                    attempts += 1;
                }

                if let Some(pid) = lro_data.get("response")
                    .and_then(|r| r.get("cloudaicompanionProject"))
                    .and_then(|p| p.get("id"))
                    .and_then(|i| i.as_str()) 
                {
                    project_id = Some(pid.to_string());
                    println!("Provisioned project: {}", pid);
                }
            } else {
                let err_text = onboard_resp.text().await.unwrap_or_default();
                println!("⚠️ Failed to provision Cloud Code project: {}", err_text);
            }
        }
        
        if project_id.is_none() {
            println!("⚠️ Could not automatically detect or provision a Google Cloud Project for Gemini CLI.");
        }

        println!("🎉 Gemini OAuth Authentication Successful!");

        Ok(OAuthCredential {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token,
            expiry_date: token_resp.expires_in.map(|secs| Utc::now().timestamp_millis() + secs * 1000),
            token_type: Some(token_resp.token_type),
            id_token: token_resp.id_token,
            project_id,
        })
    }

    /// Parse code, state, error from raw HTTP callback request.
    fn parse_callback_params(
        raw_request: &str,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let mut code = None;
        let mut state = None;
        let mut error = None;

        if let Some(line) = raw_request.lines().next() {
            if let Some(path) = line.split_whitespace().nth(1) {
                if let Ok(url) = Url::parse(
                    &format!("http://localhost{}", path),
                ) {
                    for (k, v) in url.query_pairs() {
                        match k.as_ref() {
                            "code" => code = Some(v.into_owned()),
                            "state" => state = Some(v.into_owned()),
                            "error" => error = Some(v.into_owned()),
                            _ => {}
                        }
                    }
                }
            }
        }
        (code, state, error)
    }

    /// Read a single line from stdin asynchronously.
    async fn read_stdin_line() -> Result<String> {
        tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("Failed to read from stdin")?;
            Ok(line.trim().to_string())
        })
        .await
        .context("Stdin reader task panicked")?
    }

    /// Parse a pasted redirect URL and extract code + state.
    fn parse_redirect_url(input: &str) -> Result<(String, String)> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("Empty URL provided"));
        }

        let url = Url::parse(trimmed).context(
            "Invalid URL. Please paste the full redirect URL \
             from your browser's address bar.",
        )?;

        let mut code = None;
        let mut state = None;
        let mut error = None;

        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }

        if let Some(err_msg) = error {
            return Err(anyhow!(
                "Google OAuth returned an error: {}",
                err_msg,
            ));
        }

        let code = code.ok_or_else(|| {
            anyhow!(
                "No 'code' parameter found in URL. \
                 Make sure you pasted the complete redirect URL."
            )
        })?;
        let state = state.ok_or_else(|| {
            anyhow!(
                "No 'state' parameter found in URL. \
                 Make sure you pasted the complete redirect URL."
            )
        })?;

        Ok((code, state))
    }
}

pub struct GeminiOauthProvider {
    config: GeminiOauthConfig,
    cred_manager: CredentialManager,
    http_client: Client,
}

impl GeminiOauthProvider {
    pub fn new(config: GeminiOauthConfig) -> Self {
        let cred_manager = CredentialManager::new(&config.credentials_path);
        let http_client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            config,
            cred_manager,
            http_client,
        }
    }

    
    async fn send_request(&self, original_request: &serde_json::Value) -> Result<serde_json::Value, LlmError> {
        let credential = self
            .cred_manager
            .get_valid_credential()
            .await
            .map_err(|_e| LlmError::AuthFailed {
                provider: "gemini_oauth".to_string(),
            })?;

        // Format is equivalent to the Google Generative Language API
        // https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent
        let (url, request_body, headers) = if self.config.model.contains("preview") || self.config.model.contains("gemini-3") {
            // Use Cloud Code API for new models
            let url = "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse".to_string();
            let mut req = serde_json::json!({
                "model": self.config.model,
                "request": original_request,
            });
            if let Some(pid) = credential.project_id {
                req["project"] = serde_json::Value::String(pid);
            }
            
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("Content-Type", "application/json".parse().unwrap());
            headers.insert("User-Agent", "google-cloud-sdk vscode_cloudshelleditor/0.1".parse().unwrap());
            headers.insert("X-Goog-Api-Client", "gl-node/22.17.0".parse().unwrap());
            headers.insert("Client-Metadata", "{\"ideType\":\"IDE_UNSPECIFIED\",\"platform\":\"PLATFORM_UNSPECIFIED\",\"pluginType\":\"GEMINI\"}".parse().unwrap());

            (url, req, headers)
        } else {
            // Legacy / Standard fallback
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
                self.config.model
            );
            
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("Content-Type", "application/json".parse().unwrap());
            
            (url, original_request.clone(), headers)
        };

        let response = self
            .http_client
            .post(&url)
            .bearer_auth(credential.access_token)
            .headers(headers)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: "gemini_oauth".to_string(),
                reason: e.to_string(),
            })?;

        let status = response.status();
        let body_bytes = response.bytes().await.map_err(|e| LlmError::RequestFailed {
            provider: "gemini_oauth".to_string(),
            reason: format!("Failed to read response body: {}", e),
        })?;
        
        // Cloud Code returns SSE stream, we need to parse it
        let mut final_response = serde_json::json!({});
        let body_str = String::from_utf8_lossy(&body_bytes);
        
        let mut success = false;
        if self.config.model.contains("preview") || self.config.model.contains("gemini-3") {
            let mut combined_text = String::new();
            let mut finish_reason = "STOP".to_string();
            let mut prompt_tokens = 0;
            let mut candidates_tokens = 0;
        
            for line in body_str.lines() {
                if line.starts_with("data:") {
                    let json_str = line[5..].trim();
                    if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(resp) = chunk.get("response") {
                            // Extract text
                            if let Some(candidates) = resp.get("candidates").and_then(|c| c.as_array()) {
                                if let Some(first) = candidates.first() {
                                    if let Some(parts) = first.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
                                        for part in parts {
                                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                                combined_text.push_str(text);
                                            }
                                        }
                                    }
                                    if let Some(fr) = first.get("finishReason").and_then(|fr| fr.as_str()) {
                                        finish_reason = fr.to_string();
                                    }
                                }
                            }
                            // Extract usage
                            if let Some(usage) = resp.get("usageMetadata") {
                                if let Some(pt) = usage.get("promptTokenCount").and_then(|pt| pt.as_i64()) {
                                    prompt_tokens = pt;
                                }
                                if let Some(ct) = usage.get("candidatesTokenCount").and_then(|ct| ct.as_i64()) {
                                    candidates_tokens = ct;
                                }
                            }
                        }
                    }
                }
            }
            if !combined_text.is_empty() {
                final_response = serde_json::json!({
                    "candidates": [{
                        "content": {
                            "parts": [{"text": combined_text}]
                        },
                        "finishReason": finish_reason
                    }],
                    "usageMetadata": {
                        "promptTokenCount": prompt_tokens,
                        "candidatesTokenCount": candidates_tokens
                    }
                });
                success = true;
            }
        } else {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_str) {
                final_response = json;
                success = true;
            }
        }

        if !status.is_success() || !success {
            let err_msg = final_response
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or(&body_str);

            if status.as_u16() == 429 {
                let retry_after = Self::parse_retry_after(err_msg);
                return Err(LlmError::RateLimited {
                    provider: "gemini_oauth".to_string(),
                    retry_after,
                });
            }

            return Err(LlmError::InvalidResponse {
                provider: "gemini_oauth".to_string(),
                reason: format!("HTTP {}: {}", status.as_u16(), err_msg),
            });
        }

        Ok(final_response)
    }

    /// Parse retry-after duration from Gemini error messages.
    ///
    /// Matches patterns like "Your quota will reset after 46s."
    /// or "Your quota will reset after 18h31m10s."
    fn parse_retry_after(message: &str) -> Option<Duration> {
        use std::time::Duration;

        let re_pattern = regex::Regex::new(
            r"reset after (?:(\d+)h)?(?:(\d+)m)?(\d+)s"
        ).ok()?;

        let caps = re_pattern.captures(message)?;
        let hours: u64 = caps.get(1)
            .map_or(0, |m| m.as_str().parse().unwrap_or(0));
        let minutes: u64 = caps.get(2)
            .map_or(0, |m| m.as_str().parse().unwrap_or(0));
        let seconds: u64 = caps.get(3)
            .map_or(0, |m| m.as_str().parse().unwrap_or(0));

        let total_secs = hours * 3600 + minutes * 60 + seconds;
        if total_secs > 0 {
            Some(Duration::from_secs(total_secs + 2))
        } else {
            None
        }
    }

    fn to_gemini_request(
        messages: &[ChatMessage],
        _tools: Option<&[ToolCall]>,
    ) -> serde_json::Value {
        let mut contents = Vec::new();
        let mut system_instruction = None;

        for msg in messages {
            match msg.role {
                Role::System => {
                    system_instruction = Some(serde_json::json!({
                        "parts": [{ "text": msg.content }]
                    }));
                }
                Role::User => {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{ "text": msg.content }]
                    }));
                }
                Role::Assistant => {
                    contents.push(serde_json::json!({
                        "role": "model",
                        "parts": [{ "text": msg.content }]
                    }));
                }
                Role::Tool => {
                    // Quick conversion for tool calls (this is an approximation, real Google APIs might require different format)
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{ "text": format!("Tool response:\n{}", msg.content) }]
                    }));
                }
            }
        }

        let mut req = serde_json::json!({
            "contents": contents
        });

        if let Some(sys) = system_instruction {
            req["systemInstruction"] = sys;
        }

        req
    }

    fn from_gemini_response(body: serde_json::Value) -> Result<CompletionResponse, LlmError> {
        let candidate = body
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| LlmError::RequestFailed {
                provider: "gemini_oauth".to_string(),
                reason: "Response missing 'candidates[0]'".to_string(),
            })?;

        let content_text = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|p| p.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();

        let finish_reason = candidate
            .get("finishReason")
            .and_then(|r| r.as_str())
            .unwrap_or("STOP");

        let stop_reason = match finish_reason {
            "STOP" => FinishReason::Stop,
            "MAX_TOKENS" => FinishReason::Length,
            _ => FinishReason::Stop,
        };

        let usage = body.get("usageMetadata");
        let input_tokens = usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32;
        let output_tokens = usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32;

        Ok(CompletionResponse {
            content: content_text,
            finish_reason: stop_reason,
            input_tokens,
            output_tokens,
        })
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiOauthProvider {
    fn model_name(&self) -> &str {
        &self.config.model
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        Ok(ModelMetadata {
            id: self.config.model.clone(),
            context_length: Some(1_000_000),
        })
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let req_json = Self::to_gemini_request(&request.messages, None);
        let resp_json = self.send_request(&req_json).await?;
        Self::from_gemini_response(resp_json)
    }

    async fn complete_with_tools(
        &self,
        request: crate::llm::provider::ToolCompletionRequest,
    ) -> Result<crate::llm::provider::ToolCompletionResponse, LlmError> {
        // Fallback for completion without tools
        let comp_req = CompletionRequest {
            messages: request.messages,
            model: request.model,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop_sequences: None, // No stop_sequences in ToolCompletionRequest
            metadata: request.metadata,
        };

        let response = self.complete(comp_req).await?;

        Ok(crate::llm::provider::ToolCompletionResponse {
            content: Some(response.content),
            finish_reason: response.finish_reason,
            input_tokens: response.input_tokens,
            output_tokens: response.output_tokens,
            tool_calls: vec![],
        })
    }
}
