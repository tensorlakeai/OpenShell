// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tensorlake compute driver.
//!
//! Each `OpenShell` sandbox maps to one Tensorlake sandbox (a Firecracker VM).
//! Create returns after Tensorlake accepts the request. A background task then
//! waits for the VM to run, uploads the launch bundle, and starts the in-image
//! launcher as root. A poll loop keeps status current because Tensorlake has no
//! watch API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt as _};
use openshell_core::proto::compute::v1::{
    CpuResourceCapabilities, DriverCondition, DriverSandbox, DriverSandboxStatus,
    GetCapabilitiesResponse, GpuResourceCapabilities, MemoryResourceCapabilities,
    ResolvedWorkloadIdentity as ProtoIdentity, ResourceCapabilities, WatchSandboxesDeletedEvent,
    WatchSandboxesEvent, WatchSandboxesSandboxEvent, watch_sandboxes_event,
};
use openshell_core::resource_admission::DriverAdmissionConfig;
use openshell_isolation_interface::contract::ResolvedWorkloadIdentity;
use tokio::sync::{Mutex, broadcast};
use tonic::Status;
use tracing::{info, warn};

use crate::client::{
    CreateSandboxRequest, NetworkPolicy, Resources, SandboxInfo, StartProcessRequest,
    TensorlakeClient, TensorlakeError,
};
use crate::config::TensorlakeComputeConfig;
use crate::launch::{self, LaunchBundleInput};

/// Prefix for Tensorlake sandbox names created by this driver.
pub const NAME_PREFIX: &str = "openshell-";

/// How long a Tensorlake sandbox may stay pending before create fails.
const PROVISION_TIMEOUT: Duration = Duration::from_mins(10);

const WATCH_BUFFER: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Tensorlake accepted the sandbox; the VM is not running yet.
    Pending,
    /// The VM runs; the driver is uploading the bundle and starting the launcher.
    Launching,
    /// The launcher started. The gateway waits for the guard session.
    Running,
    Suspended,
    Failed {
        reason: String,
        message: String,
    },
    Deleting,
}

#[derive(Debug, Clone)]
struct Record {
    sandbox: DriverSandbox,
    tensorlake_id: String,
    tensorlake_name: String,
    identity: Option<ResolvedWorkloadIdentity>,
    phase: Phase,
}

impl Record {
    fn snapshot(&self) -> DriverSandbox {
        let mut sandbox = self.sandbox.clone();
        sandbox.status = Some(DriverSandboxStatus {
            name: self.tensorlake_name.clone(),
            instance_id: self.tensorlake_id.clone(),
            conditions: vec![ready_condition(&self.phase)],
            deleting: self.phase == Phase::Deleting,
            resolved_identity: self.identity.as_ref().map(|identity| ProtoIdentity {
                uid: identity.uid,
                gid: identity.gid,
                supplementary_gids: identity.supplementary_gids.clone(),
                source: identity.source.clone(),
                resource_digest: identity.resource_digest.clone(),
            }),
            ..DriverSandboxStatus::default()
        });
        sandbox
    }
}

fn ready_condition(phase: &Phase) -> DriverCondition {
    let (status, reason, message) = match phase {
        Phase::Pending => (
            "False",
            "Starting",
            "Tensorlake sandbox is pending".to_string(),
        ),
        Phase::Launching => (
            "False",
            "Starting",
            "starting the OpenShell guard in the Tensorlake sandbox".to_string(),
        ),
        Phase::Running => (
            "True",
            "TensorlakeRunning",
            "Tensorlake sandbox is running".to_string(),
        ),
        Phase::Suspended => (
            "False",
            "Suspended",
            "Tensorlake sandbox is suspended; the Tensorlake driver cannot resume it yet"
                .to_string(),
        ),
        Phase::Failed { reason, message } => ("False", reason.as_str(), message.clone()),
        Phase::Deleting => (
            "False",
            "Deleting",
            "Tensorlake sandbox is being deleted".to_string(),
        ),
    };
    DriverCondition {
        r#type: "Ready".to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message,
        transition_time: None,
    }
}

/// Tensorlake sandbox name for an `OpenShell` sandbox ID.
pub fn tensorlake_name(sandbox_id: &str) -> String {
    let sanitized: String = sandbox_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{NAME_PREFIX}{sanitized}")
}

/// Parse a Kubernetes-style CPU quantity (`2`, `1.5`, `500m`) into vCPUs.
fn parse_cpus(quantity: &str) -> Result<f64, String> {
    let quantity = quantity.trim();
    let value = quantity.strip_suffix('m').map_or_else(
        || quantity.parse::<f64>(),
        |millis| millis.parse::<f64>().map(|value| value / 1000.0),
    );
    match value {
        Ok(cpus) if cpus > 0.0 && cpus.is_finite() => Ok(cpus),
        _ => Err(format!("invalid CPU quantity '{quantity}'")),
    }
}

/// Parse a Kubernetes-style memory quantity into MiB.
fn parse_memory_mb(quantity: &str) -> Result<i64, String> {
    let quantity = quantity.trim();
    let units: [(&str, f64); 8] = [
        ("Ki", 1.0 / 1024.0),
        ("Mi", 1.0),
        ("Gi", 1024.0),
        ("Ti", 1024.0 * 1024.0),
        ("K", 1000.0 / 1_048_576.0),
        ("M", 1_000_000.0 / 1_048_576.0),
        ("G", 1_000_000_000.0 / 1_048_576.0),
        ("T", 1_000_000_000_000.0 / 1_048_576.0),
    ];
    let (number, scale) = units
        .iter()
        .find_map(|(suffix, scale)| quantity.strip_suffix(suffix).map(|n| (n, *scale)))
        .unwrap_or((quantity, 1.0 / 1_048_576.0));
    match number.parse::<f64>() {
        #[allow(clippy::cast_possible_truncation)]
        Ok(value) if value > 0.0 && value.is_finite() => Ok(((value * scale).ceil() as i64).max(1)),
        _ => Err(format!("invalid memory quantity '{quantity}'")),
    }
}

fn first_non_empty(values: [&str; 2]) -> Option<&str> {
    values.into_iter().find(|value| !value.trim().is_empty())
}

#[derive(Clone)]
pub struct TensorlakeComputeDriver {
    client: TensorlakeClient,
    config: Arc<TensorlakeComputeConfig>,
    admission: Arc<DriverAdmissionConfig>,
    gateway_ca_pem: Option<Arc<Vec<u8>>>,
    records: Arc<Mutex<HashMap<String, Record>>>,
    events: broadcast::Sender<WatchSandboxesEvent>,
}

impl std::fmt::Debug for TensorlakeComputeDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TensorlakeComputeDriver")
            .field("config", &self.config)
            .finish()
    }
}

impl TensorlakeComputeDriver {
    /// Build the driver, recover sandboxes it created before a restart, and
    /// start the status poll loop.
    pub async fn new(
        config: TensorlakeComputeConfig,
        admission: DriverAdmissionConfig,
    ) -> Result<Self, String> {
        config.validate()?;
        let gateway_ca_pem = match &config.gateway_tls_ca {
            Some(path) => {
                Some(Arc::new(std::fs::read(path).map_err(|error| {
                    format!("read gateway CA {}: {error}", path.display())
                })?))
            }
            None => None,
        };
        let client =
            TensorlakeClient::new(&config.api_url, &config.api_key).map_err(|e| e.to_string())?;
        let (events, _) = broadcast::channel(WATCH_BUFFER);
        let driver = Self {
            client,
            config: Arc::new(config),
            admission: Arc::new(admission),
            gateway_ca_pem,
            records: Arc::new(Mutex::new(HashMap::new())),
            events,
        };
        driver.recover().await?;
        let poller = driver.clone();
        tokio::spawn(async move { poller.poll_loop().await });
        Ok(driver)
    }

    #[cfg(test)]
    fn for_tests(config: TensorlakeComputeConfig) -> Self {
        let (events, _) = broadcast::channel(WATCH_BUFFER);
        Self {
            client: TensorlakeClient::new(&config.api_url, &config.api_key).unwrap(),
            config: Arc::new(config),
            admission: Arc::new(DriverAdmissionConfig::default()),
            gateway_ca_pem: None,
            records: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    pub fn capabilities(&self) -> GetCapabilitiesResponse {
        let mut extension = openshell_core::extension_protocol::extension_metadata(
            openshell_core::extension_protocol::ExtensionFamily::Compute,
            "openshell/tensorlake",
            openshell_core::VERSION,
            [],
        );
        // The launch bundle needs gateway-signed launch credentials.
        extension
            .required_capabilities
            .push(openshell_core::extension_protocol::COMPUTE_LAUNCH_AUTHENTICATION.to_string());
        GetCapabilitiesResponse {
            resource_admission_policy: self.admission.acknowledgement(),
            driver_name: "tensorlake".to_string(),
            driver_version: openshell_core::VERSION.to_string(),
            default_image: self.config.default_image.clone(),
            gateway_manages_lifecycle: false,
            supports_sandbox_authentication: false,
            driver_reports_runtime_readiness: false,
            resource_capabilities: Some(ResourceCapabilities {
                cpu: Some(CpuResourceCapabilities {
                    limit_supported: true,
                }),
                memory: Some(MemoryResourceCapabilities {
                    limit_supported: true,
                }),
                gpu: Some(GpuResourceCapabilities {
                    default_selection_supported: false,
                    count_selection_supported: false,
                }),
            }),
            rootfs_tar_staging_dir: String::new(),
            rootfs_tar_max_bytes: 0,
            extension: Some(extension),
        }
    }

    fn image_for(&self, sandbox: &DriverSandbox) -> String {
        sandbox
            .spec
            .as_ref()
            .and_then(|spec| spec.template.as_ref())
            .map(|template| template.image.trim())
            .filter(|image| !image.is_empty())
            .unwrap_or(&self.config.default_image)
            .to_string()
    }

    fn resources_for(&self, sandbox: &DriverSandbox) -> Result<Resources, String> {
        let requirements = sandbox
            .spec
            .as_ref()
            .and_then(|spec| spec.template.as_ref())
            .and_then(|template| template.resources.as_ref());
        let cpus = requirements
            .and_then(|r| first_non_empty([&r.cpu_limit, &r.cpu_request]))
            .map_or(Ok(self.config.default_cpus), parse_cpus)?;
        let memory_mb = requirements
            .and_then(|r| first_non_empty([&r.memory_limit, &r.memory_request]))
            .map_or(Ok(self.config.default_memory_mb), parse_memory_mb)?;
        Ok(Resources {
            cpus,
            memory_mb,
            disk_mb: self.config.default_disk_mb,
        })
    }

    /// Validate everything that can be checked before Tensorlake is called.
    pub fn validate_sandbox_create(
        &self,
        sandbox: &DriverSandbox,
    ) -> Result<(Resources, ResolvedWorkloadIdentity), Status> {
        openshell_core::resource_admission::check_sandbox_driver_config(
            self.admission.allow_driver_config,
            sandbox,
        )?;
        if sandbox.id.trim().is_empty() {
            return Err(Status::invalid_argument("sandbox.id is required"));
        }
        let spec = sandbox
            .spec
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("sandbox.spec is required"))?;
        if let Some(template) = spec.template.as_ref() {
            if !template.agent_socket_path.trim().is_empty() {
                return Err(Status::failed_precondition(
                    "the Tensorlake driver does not support template.agent_socket_path",
                ));
            }
            if template
                .platform_config
                .as_ref()
                .is_some_and(|config| !config.fields.is_empty())
            {
                return Err(Status::failed_precondition(
                    "the Tensorlake driver does not support template.platform_config",
                ));
            }
        }
        if spec
            .resource_requirements
            .as_ref()
            .and_then(|requirements| requirements.gpu.as_ref())
            .is_some()
        {
            return Err(Status::failed_precondition(
                "the Tensorlake driver does not support GPU sandboxes yet",
            ));
        }
        if spec.launch_authentication.is_empty() {
            return Err(Status::failed_precondition(
                "Tensorlake sandboxes require launch-scoped gateway authentication",
            ));
        }
        let resources = self
            .resources_for(sandbox)
            .map_err(Status::invalid_argument)?;
        let identity = launch::resolve_identity(sandbox, &self.image_for(sandbox))
            .map_err(Status::failed_precondition)?;
        Ok((resources, identity))
    }

    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), Status> {
        let (resources, identity) = self.validate_sandbox_create(sandbox)?;
        if self.records.lock().await.contains_key(&sandbox.id) {
            return Err(Status::already_exists("sandbox already exists"));
        }
        let allow_out = self.config.effective_allow_out();
        let network = (!allow_out.is_empty()).then(|| NetworkPolicy {
            allow_internet_access: false,
            allow_out,
            deny_out: Vec::new(),
        });
        let name = tensorlake_name(&sandbox.id);
        let request = CreateSandboxRequest {
            image: Some(self.image_for(sandbox)),
            resources: Some(resources),
            timeout_secs: self.config.timeout_secs,
            network,
            name: Some(name.clone()),
            wait: Some(false),
        };
        let created = self
            .client
            .create_sandbox(&request)
            .await
            .map_err(|error| match error {
                TensorlakeError::Api { status, .. } if status.as_u16() == 409 => {
                    Status::already_exists("Tensorlake sandbox already exists")
                }
                other => Status::unavailable(other.to_string()),
            })?;
        info!(
            sandbox_id = %sandbox.id,
            tensorlake_sandbox_id = %created.sandbox_id,
            "Created Tensorlake sandbox"
        );

        // Keep secrets out of the stored record; only the provisioning task
        // sees the launch credentials.
        let mut stored = sandbox.clone();
        if let Some(spec) = stored.spec.as_mut() {
            spec.sandbox_token.clear();
            spec.launch_authentication.clear();
        }
        let record = Record {
            sandbox: stored,
            tensorlake_id: created.sandbox_id.clone(),
            tensorlake_name: name,
            identity: Some(identity.clone()),
            phase: Phase::Pending,
        };
        self.records
            .lock()
            .await
            .insert(sandbox.id.clone(), record.clone());
        self.emit_sandbox(&record);

        let driver = self.clone();
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            if let Err(message) = driver
                .provision(&sandbox, &created.sandbox_id, &identity)
                .await
            {
                warn!(sandbox_id = %sandbox.id, %message, "Tensorlake sandbox launch failed");
                driver
                    .set_phase(
                        &sandbox.id,
                        Phase::Failed {
                            reason: "LaunchFailed".into(),
                            message,
                        },
                    )
                    .await;
            }
        });
        Ok(())
    }

    /// Wait for the VM, upload the launch bundle, and start the launcher.
    async fn provision(
        &self,
        sandbox: &DriverSandbox,
        tensorlake_id: &str,
        identity: &ResolvedWorkloadIdentity,
    ) -> Result<(), String> {
        let started = Instant::now();
        let info = loop {
            if !self.is_active(&sandbox.id).await {
                return Ok(());
            }
            let info = self
                .client
                .get_sandbox(tensorlake_id)
                .await
                .map_err(|error| error.to_string())?;
            match info.status.as_str() {
                "running" => break info,
                "pending" => {}
                other => {
                    return Err(format!(
                        "Tensorlake sandbox entered state '{other}' before launch{}",
                        failure_detail(&info)
                    ));
                }
            }
            if started.elapsed() > PROVISION_TIMEOUT {
                return Err("Tensorlake sandbox did not start within 10 minutes".into());
            }
            tokio::time::sleep(self.config.poll_interval).await;
        };
        self.set_phase(&sandbox.id, Phase::Launching).await;

        let bundle = launch::build_launch_bundle(LaunchBundleInput {
            sandbox,
            config: &self.config,
            tensorlake_sandbox_id: tensorlake_id,
            identity,
            gateway_ca_pem: self.gateway_ca_pem.as_deref().map(Vec::as_slice),
        })?;
        self.client
            .write_file(&info, launch::BUNDLE_UPLOAD_PATH, bundle)
            .await
            .map_err(|error| format!("upload launch bundle: {error}"))?;
        self.client
            .start_process(
                &info,
                &StartProcessRequest {
                    command: launch::LAUNCHER_PATH.to_string(),
                    args: vec![launch::BUNDLE_UPLOAD_PATH.to_string()],
                    env: std::collections::BTreeMap::new(),
                    user: "root".to_string(),
                    name: launch::LAUNCHER_PROCESS_NAME.to_string(),
                    stdin_mode: "closed",
                },
            )
            .await
            .map_err(|error| format!("start OpenShell launcher: {error}"))?;
        info!(sandbox_id = %sandbox.id, "Started OpenShell launcher in Tensorlake sandbox");
        self.set_phase(&sandbox.id, Phase::Running).await;
        Ok(())
    }

    pub async fn get_sandbox(&self, sandbox_id: &str) -> Option<DriverSandbox> {
        self.records
            .lock()
            .await
            .get(sandbox_id)
            .map(Record::snapshot)
    }

    pub async fn list_sandboxes(&self) -> Vec<DriverSandbox> {
        let mut sandboxes: Vec<_> = self
            .records
            .lock()
            .await
            .values()
            .map(Record::snapshot)
            .collect();
        sandboxes.sort_by(|a, b| a.id.cmp(&b.id));
        sandboxes
    }

    pub async fn delete_sandbox(&self, sandbox_id: &str) -> Result<bool, Status> {
        let tensorlake_id = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(sandbox_id) else {
                return Ok(false);
            };
            record.phase = Phase::Deleting;
            let record = record.clone();
            drop(records);
            self.emit_sandbox(&record);
            record.tensorlake_id
        };
        let deleted = self
            .client
            .delete_sandbox(&tensorlake_id)
            .await
            .map_err(|error| Status::unavailable(error.to_string()))?;
        if !deleted {
            self.forget(sandbox_id).await;
        }
        Ok(true)
    }

    pub async fn watch_sandboxes(
        &self,
    ) -> impl Stream<Item = Result<WatchSandboxesEvent, Status>> + Send + 'static {
        let receiver = self.events.subscribe();
        let initial = self.list_sandboxes().await;
        let live = tokio_stream::wrappers::BroadcastStream::new(receiver).filter_map(|item| {
            std::future::ready(match item {
                Ok(event) => Some(Ok(event)),
                Err(error) => Some(Err(Status::data_loss(format!(
                    "Tensorlake watch fell behind: {error}"
                )))),
            })
        });
        futures::stream::iter(
            initial
                .into_iter()
                .map(|sandbox| Ok(sandbox_event(sandbox))),
        )
        .chain(live)
    }

    async fn is_active(&self, sandbox_id: &str) -> bool {
        self.records
            .lock()
            .await
            .get(sandbox_id)
            .is_some_and(|record| record.phase != Phase::Deleting)
    }

    async fn set_phase(&self, sandbox_id: &str, phase: Phase) {
        let record = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(sandbox_id) else {
                return;
            };
            if record.phase == Phase::Deleting || record.phase == phase {
                return;
            }
            record.phase = phase;
            record.clone()
        };
        self.emit_sandbox(&record);
    }

    async fn forget(&self, sandbox_id: &str) {
        if self.records.lock().await.remove(sandbox_id).is_some() {
            let _ = self.events.send(WatchSandboxesEvent {
                payload: Some(watch_sandboxes_event::Payload::Deleted(
                    WatchSandboxesDeletedEvent {
                        sandbox_id: sandbox_id.to_string(),
                    },
                )),
            });
        }
    }

    fn emit_sandbox(&self, record: &Record) {
        let _ = self.events.send(sandbox_event(record.snapshot()));
    }

    /// Rebuild records for sandboxes this driver created before a restart.
    /// Launch credentials are not kept, so pending sandboxes cannot finish
    /// launching and are reported as failed.
    async fn recover(&self) -> Result<(), String> {
        let sandboxes = self
            .client
            .list_sandboxes()
            .await
            .map_err(|error| format!("list Tensorlake sandboxes: {error}"))?;
        let mut records = self.records.lock().await;
        for info in sandboxes {
            let Some(sandbox_id) = info
                .name
                .as_deref()
                .and_then(|name| name.strip_prefix(NAME_PREFIX))
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let phase = match observed_phase(&info) {
                Some(Phase::Pending) => Phase::Failed {
                    reason: "DriverRestarted".into(),
                    message: "the Tensorlake driver restarted before this sandbox launched; delete and recreate it".into(),
                },
                Some(phase) => phase,
                None => continue,
            };
            records.insert(
                sandbox_id.to_string(),
                Record {
                    sandbox: DriverSandbox {
                        id: sandbox_id.to_string(),
                        ..DriverSandbox::default()
                    },
                    tensorlake_id: info.sandbox_id.clone(),
                    tensorlake_name: info.name.clone().unwrap_or_default(),
                    identity: None,
                    phase,
                },
            );
        }
        info!(recovered = records.len(), "Recovered Tensorlake sandboxes");
        Ok(())
    }

    async fn poll_loop(&self) {
        loop {
            tokio::time::sleep(self.config.poll_interval).await;
            if let Err(error) = self.poll_once().await {
                warn!(%error, "Tensorlake status poll failed");
            }
        }
    }

    async fn poll_once(&self) -> Result<(), TensorlakeError> {
        // Read the records before the Tensorlake list. A record created after
        // this point is not in the list yet and must not look terminated.
        let records: Vec<(String, Record)> = self
            .records
            .lock()
            .await
            .iter()
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect();
        let observed: HashMap<String, SandboxInfo> = self
            .client
            .list_sandboxes()
            .await?
            .into_iter()
            .map(|info| (info.sandbox_id.clone(), info))
            .collect();
        for (sandbox_id, record) in records {
            match reconcile(&record.phase, observed.get(&record.tensorlake_id)) {
                Reconcile::Keep => {}
                Reconcile::Forget => self.forget(&sandbox_id).await,
                Reconcile::Set(next) => self.set_phase(&sandbox_id, next).await,
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reconcile {
    Keep,
    Forget,
    Set(Phase),
}

/// Decide how one poll observation changes a record.
fn reconcile(current: &Phase, info: Option<&SandboxInfo>) -> Reconcile {
    match (current, info.and_then(observed_phase)) {
        // Gone from Tensorlake while we delete it: finish the delete.
        (Phase::Deleting, None) => Reconcile::Forget,
        // The provisioning task owns launch progress and reports its own
        // failures.
        (Phase::Pending | Phase::Launching, _)
        // Keep the current phase while a delete runs.
        | (Phase::Deleting, Some(_))
        // A failed launch stays failed while the VM still exists. Nothing
        // will launch it again.
        | (Phase::Failed { .. }, Some(Phase::Pending | Phase::Running)) => Reconcile::Keep,
        (_, None) => Reconcile::Set(Phase::Failed {
            reason: "TensorlakeTerminated".into(),
            message: format!(
                "Tensorlake sandbox is no longer running{}",
                info.map(failure_detail).unwrap_or_default()
            ),
        }),
        (_, Some(next)) => Reconcile::Set(next),
    }
}

/// Map Tensorlake status to a phase. `None` means the VM is gone.
fn observed_phase(info: &SandboxInfo) -> Option<Phase> {
    match info.status.as_str() {
        "pending" => Some(Phase::Pending),
        "running" | "snapshotting" => Some(Phase::Running),
        "suspending" | "suspended" => Some(Phase::Suspended),
        _ => None,
    }
}

fn failure_detail(info: &SandboxInfo) -> String {
    let mut parts = Vec::new();
    if let Some(reason) = info.termination_reason.as_deref().filter(|r| !r.is_empty()) {
        parts.push(format!("termination reason: {reason}"));
    }
    if let Some(details) = info.error_details.as_ref().filter(|d| !d.is_null()) {
        parts.push(format!("details: {details}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn sandbox_event(sandbox: DriverSandbox) -> WatchSandboxesEvent {
    WatchSandboxesEvent {
        payload: Some(watch_sandboxes_event::Payload::Sandbox(
            WatchSandboxesSandboxEvent {
                sandbox: Some(sandbox),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use openshell_core::proto::compute::v1::{
        DriverResourceRequirements, DriverSandboxSpec, DriverSandboxTemplate,
    };

    fn driver() -> TensorlakeComputeDriver {
        TensorlakeComputeDriver::for_tests(crate::config::tests::test_config())
    }

    fn sandbox(template: DriverSandboxTemplate) -> DriverSandbox {
        DriverSandbox {
            id: "sb-1".into(),
            name: "demo".into(),
            spec: Some(DriverSandboxSpec {
                template: Some(template),
                launch_authentication: b"{}".to_vec(),
                ..DriverSandboxSpec::default()
            }),
            ..DriverSandbox::default()
        }
    }

    #[test]
    fn capabilities_require_launch_authentication() {
        let capabilities = driver().capabilities();
        let extension = capabilities.extension.unwrap();
        assert!(extension.required_capabilities.iter().any(|capability| {
            capability == openshell_core::extension_protocol::COMPUTE_LAUNCH_AUTHENTICATION
        }));
        assert!(!capabilities.gateway_manages_lifecycle);
        assert!(!capabilities.driver_reports_runtime_readiness);
    }

    #[test]
    fn validate_uses_template_resources_and_defaults() {
        let driver = driver();
        let (resources, identity) = driver
            .validate_sandbox_create(&sandbox(DriverSandboxTemplate {
                resources: Some(DriverResourceRequirements {
                    cpu_limit: "4".into(),
                    memory_limit: "8Gi".into(),
                    ..DriverResourceRequirements::default()
                }),
                ..DriverSandboxTemplate::default()
            }))
            .unwrap();
        assert!((resources.cpus - 4.0).abs() < f64::EPSILON);
        assert_eq!(resources.memory_mb, 8192);
        assert_eq!(identity.resource_digest, "openshell-tensorlake");

        let (resources, _) = driver
            .validate_sandbox_create(&sandbox(DriverSandboxTemplate::default()))
            .unwrap();
        assert!((resources.cpus - crate::config::DEFAULT_CPUS).abs() < f64::EPSILON);
        assert_eq!(resources.memory_mb, crate::config::DEFAULT_MEMORY_MB);
    }

    #[test]
    fn validate_rejects_unsupported_requests() {
        let driver = driver();
        let mut missing_auth = sandbox(DriverSandboxTemplate::default());
        missing_auth
            .spec
            .as_mut()
            .unwrap()
            .launch_authentication
            .clear();
        assert!(driver.validate_sandbox_create(&missing_auth).is_err());

        let agent_socket = sandbox(DriverSandboxTemplate {
            agent_socket_path: "/run/agent.sock".into(),
            ..DriverSandboxTemplate::default()
        });
        assert!(driver.validate_sandbox_create(&agent_socket).is_err());
    }

    #[test]
    fn names_use_prefix_and_lowercase() {
        assert_eq!(
            tensorlake_name("3F2A-b9_x"),
            "openshell-3f2a-b9-x".to_string()
        );
    }

    #[test]
    fn cpu_quantities() {
        assert!((parse_cpus("2").unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((parse_cpus("500m").unwrap() - 0.5).abs() < f64::EPSILON);
        assert!(parse_cpus("0").is_err());
        assert!(parse_cpus("lots").is_err());
    }

    #[test]
    fn memory_quantities() {
        assert_eq!(parse_memory_mb("2Gi").unwrap(), 2048);
        assert_eq!(parse_memory_mb("512Mi").unwrap(), 512);
        assert_eq!(parse_memory_mb("1G").unwrap(), 954);
        assert_eq!(parse_memory_mb("1048576").unwrap(), 1);
        assert!(parse_memory_mb("-1Gi").is_err());
    }

    #[test]
    fn ready_condition_maps_phases() {
        assert_eq!(ready_condition(&Phase::Running).status, "True");
        let pending = ready_condition(&Phase::Pending);
        assert_eq!(
            (pending.status.as_str(), pending.reason.as_str()),
            ("False", "Starting")
        );
        let failed = ready_condition(&Phase::Failed {
            reason: "LaunchFailed".into(),
            message: "boom".into(),
        });
        assert_eq!(failed.reason, "LaunchFailed");
    }

    fn info(status: &str) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: "x".into(),
            status: status.into(),
            name: None,
            sandbox_url: None,
            routing_hint: None,
            termination_reason: None,
            pending_reason: None,
            error_details: None,
        }
    }

    #[test]
    fn reconcile_leaves_launch_to_the_provisioning_task() {
        // A new record can be missing from a list that started before create.
        assert_eq!(reconcile(&Phase::Pending, None), Reconcile::Keep);
        assert_eq!(reconcile(&Phase::Launching, None), Reconcile::Keep);
        assert_eq!(
            reconcile(&Phase::Pending, Some(&info("running"))),
            Reconcile::Keep
        );
    }

    #[test]
    fn reconcile_keeps_failed_launch_failed() {
        let failed = Phase::Failed {
            reason: "DriverRestarted".into(),
            message: "m".into(),
        };
        for status in ["pending", "running"] {
            assert_eq!(reconcile(&failed, Some(&info(status))), Reconcile::Keep);
        }
    }

    #[test]
    fn reconcile_tracks_running_sandboxes() {
        assert_eq!(reconcile(&Phase::Deleting, None), Reconcile::Forget);
        assert_eq!(
            reconcile(&Phase::Deleting, Some(&info("running"))),
            Reconcile::Keep
        );
        assert_eq!(
            reconcile(&Phase::Running, Some(&info("suspended"))),
            Reconcile::Set(Phase::Suspended)
        );
        assert_eq!(
            reconcile(&Phase::Suspended, Some(&info("running"))),
            Reconcile::Set(Phase::Running)
        );
        assert!(matches!(
            reconcile(&Phase::Running, Some(&info("terminated"))),
            Reconcile::Set(Phase::Failed { reason, .. }) if reason == "TensorlakeTerminated"
        ));
    }

    #[test]
    fn observed_phase_treats_terminal_states_as_gone() {
        assert_eq!(observed_phase(&info("running")), Some(Phase::Running));
        assert_eq!(observed_phase(&info("suspended")), Some(Phase::Suspended));
        for status in ["terminated", "failed", "timeout"] {
            assert_eq!(observed_phase(&info(status)), None);
        }
    }
}
