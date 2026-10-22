// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operator configuration for the Tensorlake compute driver.

use std::path::PathBuf;
use std::time::Duration;

/// Default Tensorlake sandbox lifecycle API.
pub const DEFAULT_API_URL: &str = "https://sandbox.tensorlake.ai";

/// Default Tensorlake platform API. The tunnel in each guard VM checks its
/// key there before it connects to the sandbox API.
pub const DEFAULT_PLATFORM_API_URL: &str = "https://api.tensorlake.ai";

/// Default vCPU count when the sandbox template sets no CPU limit.
pub const DEFAULT_CPUS: f64 = 2.0;

/// Default memory in MiB when the sandbox template sets no memory limit.
pub const DEFAULT_MEMORY_MB: i64 = 4096;

/// Default status poll interval. Tensorlake has no watch API.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Default vCPU count for a guard VM.
pub const DEFAULT_GUARD_CPUS: f64 = 1.0;

/// Default memory in MiB for a guard VM. The guard uses about 32 MiB and
/// the tunnel about 15 MiB.
pub const DEFAULT_GUARD_MEMORY_MB: i64 = 1024;

#[derive(Clone)]
pub struct TensorlakeComputeConfig {
    /// Tensorlake sandbox lifecycle API base URL.
    pub api_url: String,
    /// Tensorlake platform API base URL. The guard VM's tunnel checks its
    /// key there, so the host must be reachable from the guard VM.
    pub platform_api_url: String,
    /// Tensorlake API key. Sent as a bearer token.
    pub api_key: String,
    /// Registered Tensorlake image that contains the `OpenShell` guard,
    /// starter, and launcher.
    pub default_image: String,
    /// Gateway gRPC endpoint that the guard in each VM dials.
    pub grpc_endpoint: String,
    /// Gateway CA bundle (PEM). Required for an https endpoint, because the
    /// guard trusts only this CA.
    pub gateway_tls_ca: Option<PathBuf>,
    pub default_cpus: f64,
    pub default_memory_mb: i64,
    pub default_disk_mb: Option<u64>,
    /// Tensorlake idle timeout. `Some(0)` uses the plan maximum. `None` uses
    /// the Tensorlake default (600 seconds).
    pub timeout_secs: Option<i64>,
    /// Tensorlake `allow_out` entries. Empty keeps Tensorlake's default
    /// internet access and relies on the `OpenShell` guard alone.
    pub allow_out: Vec<String>,
    pub poll_interval: Duration,
    /// Default log level for the guard when the sandbox spec sets none.
    pub log_level: String,
    /// Registered Tensorlake image for guard VMs. `None` uses
    /// `default_image`, which contains the guard and the Tensorlake CLI.
    pub guard_image: Option<String>,
    pub guard_cpus: f64,
    pub guard_memory_mb: i64,
    /// Where the driver keeps a copy of each sandbox request, so that a
    /// restart can report recovered sandboxes under their `OpenShell` names.
    pub state_dir: PathBuf,
}

impl std::fmt::Debug for TensorlakeComputeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TensorlakeComputeConfig")
            .field("api_url", &self.api_url)
            .field("platform_api_url", &self.platform_api_url)
            .field("api_key", &"[REDACTED]")
            .field("default_image", &self.default_image)
            .field("grpc_endpoint", &self.grpc_endpoint)
            .field("gateway_tls_ca", &self.gateway_tls_ca)
            .field("default_cpus", &self.default_cpus)
            .field("default_memory_mb", &self.default_memory_mb)
            .field("default_disk_mb", &self.default_disk_mb)
            .field("timeout_secs", &self.timeout_secs)
            .field("allow_out", &self.allow_out)
            .field("poll_interval", &self.poll_interval)
            .field("guard_image", &self.guard_image)
            .field("guard_cpus", &self.guard_cpus)
            .field("guard_memory_mb", &self.guard_memory_mb)
            .field("state_dir", &self.state_dir)
            .finish()
    }
}

impl TensorlakeComputeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.api_key.trim().is_empty() {
            return Err("Tensorlake API key is required".into());
        }
        if self.default_image.trim().is_empty() {
            return Err("Tensorlake sandbox image is required".into());
        }
        if self.grpc_endpoint.trim().is_empty() {
            return Err(
                "gateway gRPC endpoint is required; Tensorlake VMs dial it over the internet"
                    .into(),
            );
        }
        let endpoint = url::Url::parse(&self.grpc_endpoint)
            .map_err(|error| format!("invalid gateway gRPC endpoint: {error}"))?;
        match endpoint.scheme() {
            // The guard trusts only OPENSHELL_TLS_CA and has no public roots.
            "https" if self.gateway_tls_ca.is_none() => {
                return Err(
                    "an https gateway gRPC endpoint requires a gateway CA bundle; the guard trusts only that CA"
                        .into(),
                );
            }
            "http" | "https" => {}
            other => {
                return Err(format!(
                    "gateway gRPC endpoint must use http or https, got '{other}'"
                ));
            }
        }
        // The guard's tunnel receives these URLs as TENSORLAKE_SANDBOX_API_URL
        // and TENSORLAKE_API_URL. The tl CLI silently ignores values of any
        // other shape and falls back to the public service, so reject them
        // here.
        validate_tunnel_url("Tensorlake API URL", &self.api_url)?;
        validate_tunnel_url("Tensorlake platform API URL", &self.platform_api_url)?;
        if self.default_cpus.is_nan() || self.default_cpus <= 0.0 || self.default_memory_mb <= 0 {
            return Err("default CPU and memory must be positive".into());
        }
        if self.guard_cpus.is_nan() || self.guard_cpus <= 0.0 || self.guard_memory_mb <= 0 {
            return Err("guard CPU and memory must be positive".into());
        }
        if self
            .guard_image
            .as_deref()
            .is_some_and(|image| image.trim().is_empty())
        {
            return Err("guard image must not be empty".into());
        }
        Ok(())
    }

    /// Image for guard VMs.
    pub fn guard_image(&self) -> &str {
        self.guard_image.as_deref().unwrap_or(&self.default_image)
    }

    /// Tensorlake `allow_out` for the guard VM, with the hosts the guard
    /// itself needs added: the gateway, the sandbox API that the tunnel
    /// relays through, and the platform API where the tunnel checks its
    /// key. Verified on 2026-10-20: the tunnel calls
    /// `api.tensorlake.ai/platform/v1/keys/introspect` first, then
    /// `sandbox.tensorlake.ai`; the guard reaches the gateway through
    /// `*.sandbox.tensorlake.ai`.
    pub fn effective_allow_out(&self) -> Vec<String> {
        let mut entries = self.allow_out.clone();
        if entries.is_empty() {
            return entries;
        }
        let mut push = |host: String| {
            if !entries.contains(&host) {
                entries.push(host);
            }
        };
        if let Some(host) = url_host(&self.grpc_endpoint) {
            push(host);
        }
        if let Some(host) = url_host(&self.api_url) {
            push(format!("*.{host}"));
            push(host);
        }
        if let Some(host) = url_host(&self.platform_api_url) {
            push(host);
        }
        entries
    }
}

fn url_host(value: &str) -> Option<String> {
    url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn validate_tunnel_url(label: &str, value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|error| format!("invalid {label}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(format!(
            "{label} must be an http or https URL with a host and no query, fragment, or credentials"
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn test_config() -> TensorlakeComputeConfig {
        TensorlakeComputeConfig {
            api_url: DEFAULT_API_URL.into(),
            platform_api_url: DEFAULT_PLATFORM_API_URL.into(),
            api_key: "key".into(),
            default_image: "openshell-tensorlake".into(),
            grpc_endpoint: "https://gateway.example.com:8080".into(),
            gateway_tls_ca: None,
            default_cpus: DEFAULT_CPUS,
            default_memory_mb: DEFAULT_MEMORY_MB,
            default_disk_mb: None,
            timeout_secs: None,
            allow_out: Vec::new(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            log_level: "info".into(),
            guard_image: None,
            guard_cpus: DEFAULT_GUARD_CPUS,
            guard_memory_mb: DEFAULT_GUARD_MEMORY_MB,
            state_dir: std::env::temp_dir().join(format!(
                "openshell-tensorlake-driver-test-{}",
                std::process::id()
            )),
        }
    }

    #[test]
    fn empty_allow_out_stays_empty() {
        assert!(test_config().effective_allow_out().is_empty());
    }

    #[test]
    fn allow_out_includes_gateway_host() {
        let mut config = test_config();
        config.allow_out = vec!["api.anthropic.com".into()];
        assert_eq!(
            config.effective_allow_out(),
            vec![
                "api.anthropic.com",
                "gateway.example.com",
                "*.sandbox.tensorlake.ai",
                "sandbox.tensorlake.ai",
                "api.tensorlake.ai"
            ]
        );
    }

    #[test]
    fn allow_out_does_not_repeat_hosts() {
        let mut config = test_config();
        config.allow_out = vec!["sandbox.tensorlake.ai".into(), "api.tensorlake.ai".into()];
        assert_eq!(
            config.effective_allow_out(),
            vec![
                "sandbox.tensorlake.ai",
                "api.tensorlake.ai",
                "gateway.example.com",
                "*.sandbox.tensorlake.ai"
            ]
        );
    }

    #[test]
    fn debug_hides_api_key() {
        let mut config = test_config();
        config.api_key = "tl_secret_value".into();
        assert!(!format!("{config:?}").contains("tl_secret_value"));
    }

    #[test]
    fn validate_rejects_api_urls_the_tunnel_ignores() {
        for api_url in [
            "ftp://sandbox.example.test",
            "https://sandbox.example.test/?scope=x",
            "https://sandbox.example.test/#frag",
            "https://user:pw@sandbox.example.test",
        ] {
            let mut config = test_config();
            config.gateway_tls_ca = Some("ca".into());
            config.api_url = api_url.into();
            assert!(config.validate().is_err(), "{api_url}");
        }
        let mut config = test_config();
        config.gateway_tls_ca = Some("ca".into());
        config.api_url = "https://byoc.example.test:8443/base".into();
        assert_eq!(config.validate(), Ok(()));
        config.platform_api_url = "https://user:pw@api.example.test".into();
        assert!(config.validate().unwrap_err().contains("platform API URL"));
    }

    #[test]
    fn validate_rejects_missing_endpoint() {
        let mut config = test_config();
        config.grpc_endpoint.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_requires_ca_for_https_endpoint() {
        let mut config = test_config();
        assert!(config.validate().unwrap_err().contains("CA bundle"));
        config.gateway_tls_ca = Some(PathBuf::from("/etc/openshell/gateway-ca.pem"));
        assert!(config.validate().is_ok());
        config.gateway_tls_ca = None;
        config.grpc_endpoint = "http://gateway.example.com:8080".into();
        assert!(config.validate().is_ok());
        config.grpc_endpoint = "ftp://gateway.example.com".into();
        assert!(config.validate().is_err());
    }
}
