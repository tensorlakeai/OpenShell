// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal Tensorlake sandbox HTTP client.
//!
//! Lifecycle calls go to the sandbox API (`POST/GET/DELETE /sandboxes`).
//! File and process calls go to the per-sandbox proxy (`sandbox_url`) with the
//! `X-Tensorlake-Sandbox-Id` routing header.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const SANDBOX_ID_HEADER: &str = "X-Tensorlake-Sandbox-Id";
const ROUTE_HINT_HEADER: &str = "X-Tensorlake-Route-Hint";
/// The Tensorlake SDK sends the client deadline in milliseconds on every
/// request so the server can stop work at the same point.
const REQUEST_TIMEOUT_HEADER: &str = "X-Tensorlake-Request-Timeout-Ms";

/// Upper bound on opening a TCP connection to the API or a sandbox proxy.
/// The Tensorlake SDK sets none; this matches the `OpenShell` SDK transport.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on one whole request, from send to the last response byte.
///
/// This is the Tensorlake SDK default (`DEFAULT_HTTP_TIMEOUT_SEC`), sized for
/// a blocking create that waits for the server's ready timeout. The driver
/// creates with `wait: false` and uploads only a small configuration tar, so
/// every call should finish well inside it. The bound exists because the
/// status poll awaits launcher checks one sandbox at a time: one stalled
/// connection must fail instead of freezing reconciliation for every sandbox.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Debug, thiserror::Error)]
pub enum TensorlakeError {
    #[error("Tensorlake request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Tensorlake API returned {status}: {body}")]
    Api { status: StatusCode, body: String },
    #[error("Tensorlake sandbox not found")]
    NotFound,
    #[error("Tensorlake sandbox {0} has no sandbox_url for file and process calls")]
    MissingProxyUrl(String),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSandboxRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `false` returns immediately with state `pending`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resources {
    pub cpus: f64,
    pub memory_mb: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub allow_internet_access: bool,
    #[serde(default)]
    pub allow_out: Vec<String>,
    #[serde(default)]
    pub deny_out: Vec<String>,
}

impl NetworkPolicy {
    /// No outbound path at all. With an empty allow list,
    /// `allow_internet_access: false` turns off every outbound connection,
    /// DNS included. Inbound tunnel traffic is not affected.
    pub fn no_egress() -> Self {
        Self {
            allow_internet_access: false,
            allow_out: Vec::new(),
            deny_out: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSandboxResponse {
    pub sandbox_id: String,
}

/// Subset of Tensorlake `SandboxInfo` used by the driver.
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxInfo {
    #[serde(alias = "id")]
    pub sandbox_id: String,
    #[serde(alias = "state")]
    pub status: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sandbox_url: Option<String>,
    #[serde(default)]
    pub routing_hint: Option<String>,
    #[serde(default)]
    pub termination_reason: Option<String>,
    #[serde(default)]
    pub pending_reason: Option<String>,
    #[serde(default)]
    pub error_details: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ListSandboxesResponse {
    sandboxes: Vec<SandboxInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartProcessRequest {
    pub command: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub user: String,
    pub stdin_mode: &'static str,
}

/// Process state from the Tensorlake process API.
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessInfo {
    pub pid: i64,
    /// `running`, `exited`, `signaled`, or `oom_killed`.
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub signal: Option<i64>,
}

impl ProcessInfo {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    /// Short summary of how the process ended, for status messages.
    pub fn outcome(&self) -> String {
        match (self.status.as_str(), self.exit_code, self.signal) {
            ("exited", Some(code), _) => format!("exited with status {code}"),
            ("signaled", _, Some(signal)) => format!("was killed by signal {signal}"),
            ("oom_killed", _, _) => "was killed because it ran out of memory".into(),
            (status, _, _) => format!("stopped ({status})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TensorlakeClient {
    http: reqwest::Client,
    api_url: String,
    api_key: String,
}

impl TensorlakeClient {
    pub fn new(api_url: &str, api_key: &str) -> Result<Self, TensorlakeError> {
        // Reqwest uses Rustls's process-default provider. Ensure one exists
        // through the crypto facade instead of hard-coding a backend here.
        // Classification: like the other Reqwest clients listed in
        // `openshell-crypto/README.md`, this transport does not necessarily
        // honor an explicitly selected `CryptoContext`.
        openshell_crypto::tls::ensure_default_provider();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            REQUEST_TIMEOUT_HEADER,
            reqwest::header::HeaderValue::from_str(&REQUEST_TIMEOUT.as_millis().to_string())
                .expect("a decimal millisecond count is a valid header value"),
        );
        let http = reqwest::Client::builder()
            .user_agent(format!(
                "openshell-driver-tensorlake/{}",
                openshell_core::VERSION
            ))
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        })
    }

    pub async fn create_sandbox(
        &self,
        request: &CreateSandboxRequest,
    ) -> Result<CreateSandboxResponse, TensorlakeError> {
        let response = self
            .http
            .post(format!("{}/sandboxes", self.api_url))
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await?;
        Ok(check(response).await?.json().await?)
    }

    pub async fn get_sandbox(&self, id: &str) -> Result<SandboxInfo, TensorlakeError> {
        let response = self
            .http
            .get(format!("{}/sandboxes/{id}", self.api_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        Ok(check(response).await?.json().await?)
    }

    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>, TensorlakeError> {
        let response = self
            .http
            .get(format!("{}/sandboxes", self.api_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let body: ListSandboxesResponse = check(response).await?.json().await?;
        Ok(body.sandboxes)
    }

    /// Terminate a sandbox. Returns `false` when it no longer exists.
    pub async fn delete_sandbox(&self, id: &str) -> Result<bool, TensorlakeError> {
        let response = self
            .http
            .delete(format!("{}/sandboxes/{id}", self.api_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        match check(response).await {
            Ok(_) => Ok(true),
            Err(TensorlakeError::NotFound) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Write raw bytes to an absolute path inside a running sandbox.
    pub async fn write_file(
        &self,
        sandbox: &SandboxInfo,
        path: &str,
        contents: Vec<u8>,
    ) -> Result<(), TensorlakeError> {
        let request = self
            .proxy_request(sandbox, reqwest::Method::PUT, "/api/v1/files")?
            .query(&[("path", path)])
            .body(contents);
        check(request.send().await?).await?;
        Ok(())
    }

    /// Start a background process inside a running sandbox.
    pub async fn start_process(
        &self,
        sandbox: &SandboxInfo,
        request: &StartProcessRequest,
    ) -> Result<ProcessInfo, TensorlakeError> {
        let request = self
            .proxy_request(sandbox, reqwest::Method::POST, "/api/v1/processes")?
            .json(request);
        Ok(check(request.send().await?).await?.json().await?)
    }

    /// Read one process by PID. Returns `None` when the process is unknown.
    pub async fn get_process(
        &self,
        sandbox: &SandboxInfo,
        pid: i64,
    ) -> Result<Option<ProcessInfo>, TensorlakeError> {
        let request = self.proxy_request(
            sandbox,
            reqwest::Method::GET,
            &format!("/api/v1/processes/{pid}"),
        )?;
        match check(request.send().await?).await {
            Ok(response) => Ok(Some(response.json().await?)),
            Err(TensorlakeError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn proxy_request(
        &self,
        sandbox: &SandboxInfo,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, TensorlakeError> {
        let base = proxy_base(sandbox)?;
        let mut request = self
            .http
            .request(method, format!("{base}{path}"))
            .bearer_auth(&self.api_key)
            .header(SANDBOX_ID_HEADER, &sandbox.sandbox_id);
        if let Some(hint) = sandbox.routing_hint.as_deref().filter(|h| !h.is_empty()) {
            request = request.header(ROUTE_HINT_HEADER, hint);
        }
        Ok(request)
    }
}

/// Reduce `sandbox_url` to `scheme://host[:port]`, as the Tensorlake SDK does.
fn proxy_base(sandbox: &SandboxInfo) -> Result<String, TensorlakeError> {
    let missing = || TensorlakeError::MissingProxyUrl(sandbox.sandbox_id.clone());
    let raw = sandbox
        .sandbox_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .ok_or_else(missing)?;
    let url = url::Url::parse(raw).map_err(|_| missing())?;
    let host = url.host_str().ok_or_else(missing)?;
    Ok(url.port().map_or_else(
        || format!("{}://{host}", url.scheme()),
        |port| format!("{}://{host}:{port}", url.scheme()),
    ))
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response, TensorlakeError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == StatusCode::NOT_FOUND {
        return Err(TensorlakeError::NotFound);
    }
    let body = response.text().await.unwrap_or_default();
    Err(TensorlakeError::Api {
        status,
        body: body.chars().take(512).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(url: Option<&str>) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: "sbx1".into(),
            status: "running".into(),
            name: None,
            sandbox_url: url.map(str::to_string),
            routing_hint: None,
            termination_reason: None,
            pending_reason: None,
            error_details: None,
        }
    }

    #[test]
    fn proxy_base_keeps_scheme_host_and_port() {
        assert_eq!(
            proxy_base(&info(Some("https://sandbox.tensorlake.ai/some/path"))).unwrap(),
            "https://sandbox.tensorlake.ai"
        );
        assert_eq!(
            proxy_base(&info(Some("http://localhost:9443"))).unwrap(),
            "http://localhost:9443"
        );
    }

    #[test]
    fn proxy_base_requires_sandbox_url() {
        assert!(matches!(
            proxy_base(&info(None)),
            Err(TensorlakeError::MissingProxyUrl(_))
        ));
    }

    #[test]
    fn create_request_omits_unset_fields() {
        let request = CreateSandboxRequest {
            image: Some("img".into()),
            wait: Some(false),
            ..CreateSandboxRequest::default()
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"image": "img", "wait": false})
        );
    }

    #[test]
    fn start_process_request_has_no_managed_process_name() {
        let request = StartProcessRequest {
            command: "/bin/true".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            user: "root".into(),
            stdin_mode: "closed",
        };
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("name").is_none());
    }

    #[test]
    fn process_outcome_describes_exit() {
        let parsed: ProcessInfo = serde_json::from_value(serde_json::json!({
            "handle": 2, "pid": 721, "status": "exited", "exit_code": 1, "signal": null
        }))
        .unwrap();
        assert!(!parsed.is_running());
        assert_eq!(parsed.outcome(), "exited with status 1");
    }

    #[test]
    fn sandbox_info_accepts_id_and_state_aliases() {
        let parsed: SandboxInfo =
            serde_json::from_value(serde_json::json!({"id": "a", "state": "pending"})).unwrap();
        assert_eq!(parsed.sandbox_id, "a");
        assert_eq!(parsed.status, "pending");
    }
}
