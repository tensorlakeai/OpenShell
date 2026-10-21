// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tensorlake compute driver.
//!
//! Each `OpenShell` sandbox maps to one Tensorlake sandbox (a small VM) for
//! the workload and a second one for the
//! guard. Create returns after Tensorlake accepts the requests. A background
//! task then waits for the VMs to run, uploads a launch bundle to each, and
//! starts the in-image launcher as root. A poll loop keeps status current
//! because Tensorlake has no watch API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt as _};
use openshell_core::progress::{
    PROGRESS_STEP_PULLING_IMAGE, PROGRESS_STEP_REQUESTING_SANDBOX, PROGRESS_STEP_STARTING_SANDBOX,
    mark_progress_active, mark_progress_complete, mark_progress_detail,
};
use openshell_core::proto::compute::v1::{
    CpuResourceCapabilities, DriverCondition, DriverPlatformEvent, DriverSandbox,
    DriverSandboxStatus, GetCapabilitiesResponse, GpuResourceCapabilities,
    MemoryResourceCapabilities, ResolvedWorkloadIdentity as ProtoIdentity, ResourceCapabilities,
    WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesPlatformEvent,
    WatchSandboxesSandboxEvent, watch_sandboxes_event,
};
use openshell_core::resource_admission::DriverAdmissionConfig;
use openshell_isolation_interface::contract::ResolvedWorkloadIdentity;
use tokio::sync::{Mutex, broadcast};
use tonic::Status;
use tracing::{info, warn};

use crate::client::{
    CreateSandboxRequest, NetworkPolicy, ProcessInfo, Resources, SandboxInfo, StartProcessRequest,
    TensorlakeClient, TensorlakeError,
};
use crate::config::TensorlakeComputeConfig;
use crate::launch::{self, LaunchBundleInput};
use crate::state::StateStore;

/// Prefix for Tensorlake sandbox names created by this driver.
pub const NAME_PREFIX: &str = "openshell-";

/// Suffix of the guard VM's Tensorlake name.
pub const GUARD_NAME_SUFFIX: &str = "-guard";

/// How long a Tensorlake sandbox may stay pending before create fails.
const PROVISION_TIMEOUT: Duration = Duration::from_mins(10);

const WATCH_BUFFER: usize = 256;

/// `source` of the platform events this driver publishes.
const EVENT_SOURCE: &str = "tensorlake";

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
    /// The workload VM.
    tensorlake_id: String,
    tensorlake_name: String,
    /// The guard VM. Empty for a recovered record whose guard VM is gone.
    guard_tensorlake_id: String,
    identity: Option<ResolvedWorkloadIdentity>,
    phase: Phase,
    /// PID of the launcher in the workload VM. `None` for recovered records.
    launcher_pid: Option<i64>,
    /// PID of the launcher in the guard VM. `None` for recovered records.
    guard_launcher_pid: Option<i64>,
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

/// Tensorlake name of the guard VM for an `OpenShell` sandbox ID.
pub fn tensorlake_guard_name(sandbox_id: &str) -> String {
    format!("{}{GUARD_NAME_SUFFIX}", tensorlake_name(sandbox_id))
}

/// Which `OpenShell` sandbox a Tensorlake name belongs to, and whether it is
/// the guard VM. `None` for names this driver did not create, such as the
/// control sandbox `openshell-control`. `OpenShell` sandbox IDs are UUIDs,
/// and `tensorlake_name` keeps them intact, so the ID part must look like
/// one.
fn parse_tensorlake_name(name: &str) -> Option<(&str, bool)> {
    let rest = name.strip_prefix(NAME_PREFIX)?;
    let (id, guard) = rest
        .strip_suffix(GUARD_NAME_SUFFIX)
        .map_or((rest, false), |id| (id, true));
    let uuid_shaped = id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        });
    uuid_shaped.then_some((id, guard))
}

fn create_error(error: TensorlakeError) -> Status {
    match error {
        TensorlakeError::Api { status, .. } if status.as_u16() == 409 => {
            Status::already_exists("Tensorlake sandbox already exists")
        }
        other => Status::unavailable(other.to_string()),
    }
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
    /// Saved sandbox requests, read back after a restart.
    state: StateStore,
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
        let state = StateStore::new(&config.state_dir);
        let driver = Self {
            client,
            config: Arc::new(config),
            admission: Arc::new(admission),
            gateway_ca_pem,
            records: Arc::new(Mutex::new(HashMap::new())),
            events,
            state,
        };
        driver.recover().await?;
        let poller = driver.clone();
        tokio::spawn(async move { poller.poll_loop().await });
        Ok(driver)
    }

    #[cfg(test)]
    fn for_tests(config: TensorlakeComputeConfig) -> Self {
        let (events, _) = broadcast::channel(WATCH_BUFFER);
        let state = StateStore::new(&config.state_dir);
        Self {
            client: TensorlakeClient::new(&config.api_url, &config.api_key).unwrap(),
            config: Arc::new(config),
            admission: Arc::new(DriverAdmissionConfig::default()),
            gateway_ca_pem: None,
            records: Arc::new(Mutex::new(HashMap::new())),
            events,
            state,
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
        let resources = self
            .resources_for(sandbox)
            .map_err(Status::invalid_argument)?;
        let identity = launch::resolve_identity(sandbox, &self.image_for(sandbox))
            .map_err(Status::failed_precondition)?;
        Ok((resources, identity))
    }

    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), Status> {
        let (resources, identity) = self.validate_sandbox_create(sandbox)?;
        // The gateway mints launch authentication after validation, so only
        // create can require it.
        if sandbox
            .spec
            .as_ref()
            .is_none_or(|spec| spec.launch_authentication.is_empty())
        {
            return Err(Status::failed_precondition(
                "Tensorlake sandboxes require launch-scoped gateway authentication",
            ));
        }
        if self.records.lock().await.contains_key(&sandbox.id) {
            return Err(Status::already_exists("sandbox already exists"));
        }
        // The guard VM needs the gateway and the Tensorlake hosts. Empty
        // keeps Tensorlake's default internet. With an allow list, internet
        // access stays on: Tensorlake then connects only to the listed hosts
        // and its resolver answers only for them. Turning internet off as
        // well blocks DNS, and the tunnel cannot resolve its own API host
        // (verified on 2026-10-20).
        let allow_out = self.config.effective_allow_out();
        let guard_network = (!allow_out.is_empty()).then(|| NetworkPolicy {
            allow_internet_access: true,
            allow_out,
            deny_out: Vec::new(),
        });
        // The workload VM gets no outbound path at all. Its connections
        // travel over the boundary channel to the guard.
        let workload_network = Some(NetworkPolicy::no_egress());
        let name = tensorlake_name(&sandbox.id);
        // Keep secrets out of the stored record; only the provisioning task
        // sees the launch credentials. Save it before any VM exists, so a
        // restart always finds the record for a VM it finds.
        let mut stored = sandbox.clone();
        if let Some(spec) = stored.spec.as_mut() {
            spec.sandbox_token.clear();
            spec.launch_authentication.clear();
        }
        self.state
            .save(&stored)
            .await
            .map_err(|error| Status::internal(format!("save sandbox record: {error}")))?;
        let created = match self
            .client
            .create_sandbox(&CreateSandboxRequest {
                image: Some(self.image_for(sandbox)),
                resources: Some(resources),
                timeout_secs: self.config.timeout_secs,
                network: workload_network,
                name: Some(name.clone()),
                wait: Some(false),
            })
            .await
        {
            Ok(created) => created,
            Err(error) => {
                self.discard_state(&sandbox.id).await;
                return Err(create_error(error));
            }
        };
        info!(
            sandbox_id = %sandbox.id,
            tensorlake_sandbox_id = %created.sandbox_id,
            "Created Tensorlake sandbox"
        );
        let guard = self
            .client
            .create_sandbox(&CreateSandboxRequest {
                image: Some(self.config.guard_image().to_string()),
                resources: Some(Resources {
                    cpus: self.config.guard_cpus,
                    memory_mb: self.config.guard_memory_mb,
                    disk_mb: None,
                }),
                timeout_secs: self.config.timeout_secs,
                network: guard_network,
                name: Some(tensorlake_guard_name(&sandbox.id)),
                wait: Some(false),
            })
            .await;
        let guard_tensorlake_id = match guard {
            Ok(guard) => {
                info!(
                    sandbox_id = %sandbox.id,
                    tensorlake_sandbox_id = %guard.sandbox_id,
                    "Created Tensorlake guard sandbox"
                );
                guard.sandbox_id
            }
            Err(error) => {
                // Never leave a workload VM without its guard.
                if let Err(cleanup) = self.client.delete_sandbox(&created.sandbox_id).await {
                    warn!(
                        sandbox_id = %sandbox.id,
                        tensorlake_sandbox_id = %created.sandbox_id,
                        %cleanup,
                        "Failed to delete the workload VM after the guard VM create failed"
                    );
                }
                self.discard_state(&sandbox.id).await;
                return Err(create_error(error));
            }
        };

        let record = Record {
            sandbox: stored,
            tensorlake_id: created.sandbox_id.clone(),
            tensorlake_name: name,
            guard_tensorlake_id: guard_tensorlake_id.clone(),
            identity: Some(identity.clone()),
            phase: Phase::Pending,
            launcher_pid: None,
            guard_launcher_pid: None,
        };
        self.records
            .lock()
            .await
            .insert(sandbox.id.clone(), record.clone());
        self.emit_sandbox(&record);
        self.publish_progress(&sandbox.id, LaunchProgress::Accepted);

        let driver = self.clone();
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            if let Err(message) = driver
                .provision(
                    &sandbox,
                    &created.sandbox_id,
                    &guard_tensorlake_id,
                    &identity,
                )
                .await
            {
                warn!(sandbox_id = %sandbox.id, %message, "Tensorlake sandbox launch failed");
                driver.publish_progress(&sandbox.id, LaunchProgress::Failed(&message));
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

    /// Wait for the VMs, upload the launch bundles, and start the launchers.
    async fn provision(
        &self,
        sandbox: &DriverSandbox,
        tensorlake_id: &str,
        guard_tensorlake_id: &str,
        identity: &ResolvedWorkloadIdentity,
    ) -> Result<(), String> {
        let started = Instant::now();
        let Some(workload_info) = self
            .wait_running(sandbox, tensorlake_id, "workload", started)
            .await?
        else {
            return Ok(());
        };
        let Some(guard_info) = self
            .wait_running(sandbox, guard_tensorlake_id, "guard", started)
            .await?
        else {
            return Ok(());
        };
        self.set_phase(&sandbox.id, Phase::Launching).await;
        self.publish_progress(&sandbox.id, LaunchProgress::VmRunning);
        self.publish_progress(&sandbox.id, LaunchProgress::UploadingBundle);

        let bundles = launch::build_launch_bundles(LaunchBundleInput {
            sandbox,
            config: &self.config,
            tensorlake_sandbox_id: tensorlake_id,
            guard_tensorlake_id,
            identity,
            gateway_ca_pem: self.gateway_ca_pem.as_deref().map(Vec::as_slice),
        })?;
        self.client
            .write_file(&workload_info, launch::BUNDLE_UPLOAD_PATH, bundles.workload)
            .await
            .map_err(|error| format!("upload launch bundle: {error}"))?;
        self.client
            .write_file(&guard_info, launch::BUNDLE_UPLOAD_PATH, bundles.guard)
            .await
            .map_err(|error| format!("upload guard launch bundle: {error}"))?;
        self.publish_progress(&sandbox.id, LaunchProgress::StartingLauncher);
        let guard_launcher_pid = self.start_launcher(sandbox, &guard_info, "guard").await?;
        let launcher_pid = self
            .start_launcher(sandbox, &workload_info, "workload")
            .await?;
        if let Some(record) = self.records.lock().await.get_mut(&sandbox.id) {
            record.launcher_pid = Some(launcher_pid);
            record.guard_launcher_pid = Some(guard_launcher_pid);
        }
        self.set_phase(&sandbox.id, Phase::Running).await;
        self.publish_progress(&sandbox.id, LaunchProgress::LauncherStarted);
        Ok(())
    }

    /// Poll one VM until it runs. `Ok(None)` means the sandbox was deleted
    /// meanwhile and provisioning should stop quietly.
    async fn wait_running(
        &self,
        sandbox: &DriverSandbox,
        tensorlake_id: &str,
        role: &str,
        started: Instant,
    ) -> Result<Option<SandboxInfo>, String> {
        let mut last_pending_reason: Option<String> = None;
        loop {
            if !self.is_active(&sandbox.id).await {
                return Ok(None);
            }
            let info = self
                .client
                .get_sandbox(tensorlake_id)
                .await
                .map_err(|error| format!("get Tensorlake {role} sandbox: {error}"))?;
            match info.status.as_str() {
                "running" => return Ok(Some(info)),
                "pending" => {
                    // Report a new pending reason once, not on every poll.
                    let reason = info
                        .pending_reason
                        .as_deref()
                        .map(str::trim)
                        .filter(|reason| !reason.is_empty())
                        .map(str::to_string);
                    if reason.is_some() && reason != last_pending_reason {
                        self.publish_progress(
                            &sandbox.id,
                            LaunchProgress::Pending(reason.as_deref().unwrap_or_default()),
                        );
                        last_pending_reason = reason;
                    }
                }
                other => {
                    return Err(format!(
                        "Tensorlake {role} sandbox entered state '{other}' before launch{}",
                        failure_detail(&info)
                    ));
                }
            }
            if started.elapsed() > PROVISION_TIMEOUT {
                return Err(format!(
                    "Tensorlake {role} sandbox did not start within 10 minutes"
                ));
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Start the in-image launcher as root and return its PID.
    async fn start_launcher(
        &self,
        sandbox: &DriverSandbox,
        info: &SandboxInfo,
        role: &str,
    ) -> Result<i64, String> {
        let launcher = self
            .client
            .start_process(
                info,
                &StartProcessRequest {
                    command: launch::LAUNCHER_PATH.to_string(),
                    args: vec![launch::BUNDLE_UPLOAD_PATH.to_string()],
                    env: std::collections::BTreeMap::new(),
                    user: "root".to_string(),
                    stdin_mode: "closed",
                },
            )
            .await
            .map_err(|error| format!("start OpenShell {role} launcher: {error}"))?;
        info!(
            sandbox_id = %sandbox.id,
            tensorlake_sandbox_id = %info.sandbox_id,
            launcher_pid = launcher.pid,
            role,
            "Started OpenShell launcher in Tensorlake sandbox"
        );
        Ok(launcher.pid)
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
        let (tensorlake_id, guard_tensorlake_id) = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(sandbox_id) else {
                return Ok(false);
            };
            record.phase = Phase::Deleting;
            let record = record.clone();
            drop(records);
            self.emit_sandbox(&record);
            (record.tensorlake_id, record.guard_tensorlake_id)
        };
        let mut remaining = false;
        for id in [&tensorlake_id, &guard_tensorlake_id] {
            if id.is_empty() {
                continue;
            }
            remaining |= self
                .client
                .delete_sandbox(id)
                .await
                .map_err(|error| Status::unavailable(error.to_string()))?;
        }
        if !remaining {
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
            self.discard_state(sandbox_id).await;
            let _ = self.events.send(WatchSandboxesEvent {
                payload: Some(watch_sandboxes_event::Payload::Deleted(
                    WatchSandboxesDeletedEvent {
                        sandbox_id: sandbox_id.to_string(),
                    },
                )),
            });
        }
    }

    /// Remove the saved request of a sandbox that no longer exists.
    async fn discard_state(&self, sandbox_id: &str) {
        if let Err(error) = self.state.remove(sandbox_id).await {
            warn!(sandbox_id, %error, "Failed to remove the saved sandbox record");
        }
    }

    fn emit_sandbox(&self, record: &Record) {
        let _ = self.events.send(sandbox_event(record.snapshot()));
    }

    /// Publish a launch progress event. The gateway forwards it to the CLI,
    /// which shows the step on its provisioning spinner, so the user sees
    /// what the driver waits for while the VM boots.
    fn publish_progress(&self, sandbox_id: &str, progress: LaunchProgress<'_>) {
        let _ = self.events.send(WatchSandboxesEvent {
            payload: Some(watch_sandboxes_event::Payload::PlatformEvent(
                WatchSandboxesPlatformEvent {
                    sandbox_id: sandbox_id.to_string(),
                    event: Some(progress.into_event()),
                },
            )),
        });
    }

    /// Rebuild records for sandboxes this driver created before a restart.
    /// Launch credentials are not kept, so pending sandboxes cannot finish
    /// launching and are reported as failed. The saved request gives each
    /// record back its `OpenShell` name; the gateway rejects a snapshot
    /// whose name differs from its own record.
    async fn recover(&self) -> Result<(), String> {
        let sandboxes = self
            .client
            .list_sandboxes()
            .await
            .map_err(|error| format!("list Tensorlake sandboxes: {error}"))?;
        let mut recovered = Vec::new();
        for (sandbox_id, workload, guard) in pair_by_name(&sandboxes) {
            let Some(phase) = recovered_phase(workload, guard) else {
                continue;
            };
            let saved = match self.state.load(sandbox_id).await {
                Ok(saved) => saved,
                Err(error) => {
                    warn!(sandbox_id, %error, "Failed to read the saved sandbox record");
                    None
                }
            };
            if saved.is_none() {
                warn!(
                    sandbox_id,
                    "No saved record for this sandbox; the gateway cannot update its status. Delete and recreate it"
                );
            }
            if let Some(record) = recovered_record(sandbox_id, saved, workload, guard, phase) {
                recovered.push((sandbox_id.to_string(), record));
            }
        }
        // A saved request whose VMs are gone, for example terminated with
        // `tl` while the driver was down, has nothing left to recover.
        match self.state.saved_ids().await {
            Ok(ids) => {
                for id in ids {
                    if !recovered
                        .iter()
                        .any(|(recovered_id, _)| *recovered_id == id)
                    {
                        info!(sandbox_id = %id, "Dropping the saved record of a sandbox with no VMs");
                        self.discard_state(&id).await;
                    }
                }
            }
            Err(error) => warn!(%error, "Failed to list the saved sandbox records"),
        }
        let mut records = self.records.lock().await;
        records.extend(recovered);
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
            let info = observed.get(&record.tensorlake_id);
            let guard_info = observed.get(&record.guard_tensorlake_id);
            match reconcile(&record.phase, info, guard_info) {
                Reconcile::Keep => {}
                Reconcile::Forget => self.forget(&sandbox_id).await,
                Reconcile::Set(next) => self.set_phase(&sandbox_id, next).await,
            }
            if record.phase != Phase::Running {
                continue;
            }
            let launchers = [
                (info, record.launcher_pid, "workload"),
                (guard_info, record.guard_launcher_pid, "guard"),
            ];
            for (info, pid, role) in launchers {
                if let (Some(info), Some(pid)) = (info, pid)
                    && observed_phase(info) == Some(Phase::Running)
                {
                    self.check_launcher(&sandbox_id, info, pid, role).await;
                }
            }
        }
        Ok(())
    }

    /// Mark the sandbox failed when a launcher has stopped. Without the
    /// launcher the guard never connects, and the gateway would wait.
    async fn check_launcher(&self, sandbox_id: &str, info: &SandboxInfo, pid: i64, role: &str) {
        match self.client.get_process(info, pid).await {
            Ok(process) => {
                if let Some(failed) = launcher_failure(&info.sandbox_id, process.as_ref(), role) {
                    warn!(%sandbox_id, role, ?failed, "OpenShell launcher stopped");
                    self.set_phase(sandbox_id, failed).await;
                }
            }
            Err(error) => {
                warn!(%sandbox_id, role, %error, "Tensorlake launcher status check failed");
            }
        }
    }
}

/// Group the Tensorlake sandboxes this driver created by `OpenShell` sandbox
/// ID: `(id, workload VM, guard VM)`.
fn pair_by_name(
    sandboxes: &[SandboxInfo],
) -> Vec<(&str, Option<&SandboxInfo>, Option<&SandboxInfo>)> {
    let mut pairs: std::collections::BTreeMap<&str, (Option<&SandboxInfo>, Option<&SandboxInfo>)> =
        std::collections::BTreeMap::new();
    for info in sandboxes {
        let Some((sandbox_id, guard)) = info.name.as_deref().and_then(parse_tensorlake_name) else {
            continue;
        };
        let pair = pairs.entry(sandbox_id).or_default();
        if guard {
            pair.1 = Some(info);
        } else {
            pair.0 = Some(info);
        }
    }
    pairs
        .into_iter()
        .map(|(id, (workload, guard))| (id, workload, guard))
        .collect()
}

/// Phase for a pair found after a driver restart. `None` means nothing is
/// left to track.
/// The record for a sandbox found at restart. `saved` is the request the
/// gateway sent at create, when the state store still has it. A guard VM
/// whose workload VM is gone is still recorded, under the workload's name,
/// so that delete can remove it.
fn recovered_record(
    sandbox_id: &str,
    saved: Option<DriverSandbox>,
    workload: Option<&SandboxInfo>,
    guard: Option<&SandboxInfo>,
    phase: Phase,
) -> Option<Record> {
    let anchor = workload.or(guard)?;
    let mut sandbox = saved.unwrap_or_default();
    sandbox.id = sandbox_id.to_string();
    sandbox.status = None;
    Some(Record {
        sandbox,
        tensorlake_id: anchor.sandbox_id.clone(),
        tensorlake_name: tensorlake_name(sandbox_id),
        guard_tensorlake_id: guard
            .map(|info| info.sandbox_id.clone())
            .unwrap_or_default(),
        identity: None,
        phase,
        launcher_pid: None,
        guard_launcher_pid: None,
    })
}

fn recovered_phase(workload: Option<&SandboxInfo>, guard: Option<&SandboxInfo>) -> Option<Phase> {
    let workload_phase = workload.and_then(observed_phase);
    let guard_phase = guard.and_then(observed_phase);
    let restarted = || Phase::Failed {
        reason: "DriverRestarted".into(),
        message:
            "the Tensorlake driver restarted before this sandbox launched; delete and recreate it"
                .into(),
    };
    let half = |what: &str| Phase::Failed {
        reason: "PairIncomplete".into(),
        message: format!("the {what} VM of this sandbox is gone; delete and recreate it"),
    };
    Some(match (workload_phase, guard_phase) {
        (None, None) => return None,
        (Some(Phase::Pending), _) | (_, Some(Phase::Pending)) => restarted(),
        (None, Some(_)) => half("workload"),
        (Some(_), None) => half("guard"),
        (Some(Phase::Suspended), _) | (_, Some(Phase::Suspended)) => Phase::Suspended,
        (Some(phase), _) => phase,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Reconcile {
    Keep,
    Forget,
    Set(Phase),
}

/// Decide how one poll observation changes a record. `guard` is `None`
/// when Tensorlake no longer lists the guard VM.
fn reconcile(
    current: &Phase,
    workload: Option<&SandboxInfo>,
    guard: Option<&SandboxInfo>,
) -> Reconcile {
    let workload_phase = workload.and_then(observed_phase);
    let guard_phase = guard.and_then(observed_phase);
    let any_present = workload_phase.is_some() || guard_phase.is_some();
    match current {
        // Gone from Tensorlake while we delete it: finish the delete.
        Phase::Deleting if !any_present => return Reconcile::Forget,
        // Keep the current phase while a delete runs.
        Phase::Deleting
        // The provisioning task owns launch progress and reports its own
        // failures.
        | Phase::Pending
        | Phase::Launching => return Reconcile::Keep,
        // A failed launch stays failed while the VMs still exist. Nothing
        // will launch it again.
        Phase::Failed { .. }
            if workload_phase
                .as_ref()
                .is_some_and(|phase| *phase != Phase::Suspended)
                && guard_phase
                    .as_ref()
                    .is_some_and(|phase| *phase != Phase::Suspended) =>
        {
            return Reconcile::Keep;
        }
        _ => {}
    }
    let gone = |what: &str, info: Option<&SandboxInfo>| {
        Reconcile::Set(Phase::Failed {
            reason: "TensorlakeTerminated".into(),
            message: format!(
                "Tensorlake {what} sandbox is no longer running{}",
                info.map(failure_detail).unwrap_or_default()
            ),
        })
    };
    match (workload_phase, guard_phase) {
        (None, _) => gone("workload", workload),
        (Some(_), None) => gone("guard", guard),
        (Some(Phase::Suspended), _) | (_, Some(Phase::Suspended)) => {
            Reconcile::Set(Phase::Suspended)
        }
        (Some(next), _) => Reconcile::Set(next),
    }
}

/// The failed phase for a launcher that is no longer running.
fn launcher_failure(
    tensorlake_id: &str,
    process: Option<&ProcessInfo>,
    role: &str,
) -> Option<Phase> {
    let outcome = match process {
        Some(process) if process.is_running() => return None,
        Some(process) => process.outcome(),
        None => "is no longer known to Tensorlake".to_string(),
    };
    Some(Phase::Failed {
        reason: "LauncherExited".into(),
        message: format!(
            "the OpenShell {role} launcher {outcome}; run `tl sbx logs {tensorlake_id}` for details"
        ),
    })
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

/// Launch steps the driver reports while a sandbox boots.
///
/// Tensorlake provides the image, so there is no pull step to show. The
/// `pulling_image` step is completed together with the VM start, and the
/// remaining time is shown under `starting_sandbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchProgress<'a> {
    /// Tensorlake accepted the create request; the VM is not running yet.
    Accepted,
    /// Tensorlake reported why the VM is still pending.
    Pending(&'a str),
    /// The VM runs.
    VmRunning,
    /// The driver uploads the launch bundle.
    UploadingBundle,
    /// The driver starts the in-image launcher.
    StartingLauncher,
    /// The launcher runs; the gateway now waits for the guard session.
    LauncherStarted,
    /// Provisioning failed; the message explains why.
    Failed(&'a str),
}

impl LaunchProgress<'_> {
    fn into_event(self) -> DriverPlatformEvent {
        let (event_type, reason, message) = match self {
            Self::Accepted => (
                "Normal",
                "Accepted",
                "Tensorlake accepted the sandbox request".to_string(),
            ),
            Self::Pending(reason) => (
                "Normal",
                "Pending",
                format!("Tensorlake sandbox is pending: {reason}"),
            ),
            Self::VmRunning => ("Normal", "Running", "Tensorlake VM is running".to_string()),
            Self::UploadingBundle => (
                "Normal",
                "UploadingBundle",
                "Uploading the OpenShell launch bundle".to_string(),
            ),
            Self::StartingLauncher => (
                "Normal",
                "StartingLauncher",
                "Starting the OpenShell launcher".to_string(),
            ),
            Self::LauncherStarted => (
                "Normal",
                "LauncherStarted",
                "OpenShell launcher started; waiting for the guard session".to_string(),
            ),
            Self::Failed(message) => (
                "Warning",
                "LaunchFailed",
                format!("Tensorlake sandbox launch failed: {message}"),
            ),
        };
        let mut event = DriverPlatformEvent {
            event_time: openshell_core::time::timestamp_from_millis(openshell_core::time::now_ms())
                .ok(),
            source: EVENT_SOURCE.to_string(),
            r#type: event_type.to_string(),
            reason: reason.to_string(),
            message,
            metadata: HashMap::new(),
        };
        let metadata = &mut event.metadata;
        match self {
            Self::Accepted => {
                mark_progress_active(metadata, PROGRESS_STEP_REQUESTING_SANDBOX);
                mark_progress_detail(metadata, "Waiting for the Tensorlake VM");
            }
            Self::Pending(reason) => {
                mark_progress_active(metadata, PROGRESS_STEP_REQUESTING_SANDBOX);
                mark_progress_detail(metadata, reason);
            }
            Self::VmRunning => {
                mark_progress_complete(
                    metadata,
                    PROGRESS_STEP_REQUESTING_SANDBOX,
                    "Tensorlake VM running",
                );
                mark_progress_active(metadata, PROGRESS_STEP_PULLING_IMAGE);
            }
            Self::UploadingBundle => {
                mark_progress_complete(
                    metadata,
                    PROGRESS_STEP_PULLING_IMAGE,
                    "Image provided by Tensorlake",
                );
                mark_progress_active(metadata, PROGRESS_STEP_STARTING_SANDBOX);
                mark_progress_detail(metadata, "Uploading launch bundle");
            }
            Self::StartingLauncher => {
                mark_progress_active(metadata, PROGRESS_STEP_STARTING_SANDBOX);
                mark_progress_detail(metadata, "Starting OpenShell launcher");
            }
            Self::LauncherStarted => {
                mark_progress_active(metadata, PROGRESS_STEP_STARTING_SANDBOX);
                mark_progress_detail(metadata, "Waiting for the OpenShell guard");
            }
            Self::Failed(_) => {}
        }
        event
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

    #[tokio::test]
    async fn only_create_requires_launch_authentication() {
        let driver = driver();
        let mut missing_auth = sandbox(DriverSandboxTemplate::default());
        missing_auth
            .spec
            .as_mut()
            .unwrap()
            .launch_authentication
            .clear();
        assert!(driver.validate_sandbox_create(&missing_auth).is_ok());
        let error = driver.create_sandbox(&missing_auth).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn validate_rejects_unsupported_requests() {
        let driver = driver();
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
        assert_eq!(reconcile(&Phase::Pending, None, None), Reconcile::Keep);
        assert_eq!(reconcile(&Phase::Launching, None, None), Reconcile::Keep);
        assert_eq!(
            reconcile(&Phase::Pending, Some(&info("running")), None),
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
            let vm = info(status);
            assert_eq!(reconcile(&failed, Some(&vm), Some(&vm)), Reconcile::Keep);
        }
        // Once a VM is gone, the failure reason follows the VM.
        for guard in [Some(&info("running")), None] {
            assert!(matches!(
                reconcile(&failed, None, guard),
                Reconcile::Set(Phase::Failed { reason, .. }) if reason == "TensorlakeTerminated"
            ));
        }
        assert!(matches!(
            reconcile(&failed, Some(&info("running")), None),
            Reconcile::Set(Phase::Failed { reason, .. }) if reason == "TensorlakeTerminated"
        ));
    }

    #[test]
    fn reconcile_tracks_running_sandboxes() {
        let guard = info("running");
        assert_eq!(reconcile(&Phase::Deleting, None, None), Reconcile::Forget);
        assert_eq!(
            reconcile(&Phase::Deleting, Some(&info("running")), None),
            Reconcile::Keep
        );
        assert_eq!(
            reconcile(&Phase::Running, Some(&info("suspended")), Some(&guard)),
            Reconcile::Set(Phase::Suspended)
        );
        assert_eq!(
            reconcile(&Phase::Suspended, Some(&info("running")), Some(&guard)),
            Reconcile::Set(Phase::Running)
        );
        assert!(matches!(
            reconcile(&Phase::Running, Some(&info("terminated")), Some(&guard)),
            Reconcile::Set(Phase::Failed { reason, .. }) if reason == "TensorlakeTerminated"
        ));
    }

    #[test]
    fn reconcile_tracks_both_vms() {
        let running = info("running");
        let suspended = info("suspended");
        // Both present and running: keep running.
        assert_eq!(
            reconcile(&Phase::Running, Some(&running), Some(&running)),
            Reconcile::Set(Phase::Running)
        );
        // Delete finishes only when both are gone.
        assert_eq!(
            reconcile(&Phase::Deleting, None, Some(&running)),
            Reconcile::Keep
        );
        assert_eq!(reconcile(&Phase::Deleting, None, None), Reconcile::Forget);
        // A lost guard VM fails the sandbox and names the guard.
        let Reconcile::Set(Phase::Failed { reason, message }) =
            reconcile(&Phase::Running, Some(&running), None)
        else {
            panic!("a lost guard VM must fail the sandbox");
        };
        assert_eq!(reason, "TensorlakeTerminated");
        assert!(message.contains("guard"));
        // A suspended guard VM suspends the sandbox.
        assert_eq!(
            reconcile(&Phase::Running, Some(&running), Some(&suspended)),
            Reconcile::Set(Phase::Suspended)
        );
    }

    fn named(name: &str, status: &str) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: format!("id-{name}"),
            name: Some(name.into()),
            ..info(status)
        }
    }

    const ID: &str = "fda3b8c8-4d1f-4c8e-a762-d863a24e0137";

    #[test]
    fn names_identify_guard_vms() {
        assert_eq!(tensorlake_guard_name(ID), format!("openshell-{ID}-guard"));
        assert_eq!(
            parse_tensorlake_name(&format!("openshell-{ID}")),
            Some((ID, false))
        );
        assert_eq!(
            parse_tensorlake_name(&format!("openshell-{ID}-guard")),
            Some((ID, true))
        );
        // The control sandbox and other operator sandboxes share the prefix.
        assert_eq!(parse_tensorlake_name("openshell-control"), None);
        assert_eq!(parse_tensorlake_name("openshell-control-split"), None);
        assert_eq!(parse_tensorlake_name("openshell-"), None);
        assert_eq!(parse_tensorlake_name("openshell--guard"), None);
        assert_eq!(parse_tensorlake_name(&format!("other-{ID}")), None);
    }

    #[test]
    fn recovered_record_reports_the_saved_name() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let workload = named(&format!("openshell-{id}"), "running");
        let guard = named(&format!("openshell-{id}-guard"), "running");
        let saved = DriverSandbox {
            id: id.into(),
            name: "demo".into(),
            namespace: "default".into(),
            workspace: "ws".into(),
            status: Some(DriverSandboxStatus::default()),
            ..DriverSandbox::default()
        };

        let record = recovered_record(
            id,
            Some(saved),
            Some(&workload),
            Some(&guard),
            Phase::Running,
        )
        .unwrap();
        let snapshot = record.snapshot();
        assert_eq!(snapshot.id, id);
        assert_eq!(snapshot.name, "demo");
        assert_eq!(snapshot.namespace, "default");
        assert_eq!(snapshot.workspace, "ws");
        assert_eq!(record.tensorlake_id, workload.sandbox_id);
        assert_eq!(record.guard_tensorlake_id, guard.sandbox_id);
        let status = snapshot.status.unwrap();
        assert_eq!(status.name, format!("openshell-{id}"));
        assert_eq!(status.conditions[0].reason, "TensorlakeRunning");

        // Without the saved request only the ID is known.
        let record = recovered_record(id, None, None, Some(&guard), Phase::Suspended).unwrap();
        assert_eq!(record.sandbox.id, id);
        assert!(record.sandbox.name.is_empty());
        assert_eq!(record.tensorlake_id, guard.sandbox_id);

        assert!(recovered_record(id, None, None, None, Phase::Running).is_none());
    }

    #[test]
    fn recovery_pairs_vms_by_name() {
        let (a, b, c) = (
            "aaaaaaaa-0000-4000-8000-000000000001",
            "bbbbbbbb-0000-4000-8000-000000000002",
            "cccccccc-0000-4000-8000-000000000003",
        );
        let sandboxes = vec![
            named(&format!("openshell-{a}"), "running"),
            named(&format!("openshell-{a}-guard"), "running"),
            named(&format!("openshell-{b}"), "running"),
            named(&format!("openshell-{c}-guard"), "running"),
            named("openshell-control", "running"),
            named("unrelated", "running"),
        ];
        let pairs = pair_by_name(&sandboxes);
        assert_eq!(pairs.len(), 3);
        let (id, workload, guard) = pairs[0];
        assert_eq!(id, a);
        assert_eq!(workload.unwrap().sandbox_id, format!("id-openshell-{a}"));
        assert_eq!(guard.unwrap().sandbox_id, format!("id-openshell-{a}-guard"));
        assert_eq!(recovered_phase(workload, guard), Some(Phase::Running));
        // A workload VM without its guard is broken.
        let (_, workload, guard) = pairs[1];
        assert!(matches!(
            recovered_phase(workload, guard),
            Some(Phase::Failed { reason, .. }) if reason == "PairIncomplete"
        ));
        // A guard VM without its workload is recorded so delete can reach it.
        let (_, workload, guard) = pairs[2];
        assert!(matches!(
            recovered_phase(workload, guard),
            Some(Phase::Failed { reason, .. }) if reason == "PairIncomplete"
        ));
        // A pending VM at restart cannot finish launching.
        assert!(matches!(
            recovered_phase(
                Some(&named("openshell-d", "running")),
                Some(&named("openshell-d-guard", "pending")),
            ),
            Some(Phase::Failed { reason, .. }) if reason == "DriverRestarted"
        ));
        assert_eq!(
            recovered_phase(Some(&named("openshell-e", "terminated")), None),
            None
        );
    }

    fn process(status: &str, exit_code: Option<i64>) -> ProcessInfo {
        serde_json::from_value(serde_json::json!({
            "pid": 721, "status": status, "exit_code": exit_code, "signal": null
        }))
        .unwrap()
    }

    #[test]
    fn launcher_failure_reports_stopped_launchers() {
        assert_eq!(
            launcher_failure("tl1", Some(&process("running", None)), "workload"),
            None
        );
        let Some(Phase::Failed { reason, message }) =
            launcher_failure("tl1", Some(&process("exited", Some(1))), "guard")
        else {
            panic!("an exited launcher must fail the sandbox");
        };
        assert_eq!(reason, "LauncherExited");
        assert!(message.contains("guard launcher exited with status 1"));
        assert!(message.contains("tl sbx logs tl1"));
        assert!(launcher_failure("tl1", None, "workload").is_some());
    }

    #[test]
    fn launch_progress_maps_to_cli_steps() {
        use openshell_core::progress::{
            PROGRESS_ACTIVE_DETAIL_KEY, PROGRESS_ACTIVE_STEP_KEY, PROGRESS_COMPLETE_LABEL_KEY,
            PROGRESS_COMPLETE_STEP_KEY,
        };

        let accepted = LaunchProgress::Accepted.into_event();
        assert_eq!(accepted.source, EVENT_SOURCE);
        assert_eq!(accepted.r#type, "Normal");
        assert!(accepted.event_time.is_some());
        assert_eq!(
            accepted.metadata.get(PROGRESS_ACTIVE_STEP_KEY).unwrap(),
            PROGRESS_STEP_REQUESTING_SANDBOX
        );

        let pending = LaunchProgress::Pending("waiting for capacity").into_event();
        assert_eq!(
            pending.metadata.get(PROGRESS_ACTIVE_DETAIL_KEY).unwrap(),
            "waiting for capacity"
        );
        assert!(pending.message.contains("waiting for capacity"));

        let running = LaunchProgress::VmRunning.into_event();
        assert_eq!(
            running.metadata.get(PROGRESS_COMPLETE_STEP_KEY).unwrap(),
            PROGRESS_STEP_REQUESTING_SANDBOX
        );
        assert_eq!(
            running.metadata.get(PROGRESS_COMPLETE_LABEL_KEY).unwrap(),
            "Tensorlake VM running"
        );

        let uploading = LaunchProgress::UploadingBundle.into_event();
        assert_eq!(
            uploading.metadata.get(PROGRESS_COMPLETE_STEP_KEY).unwrap(),
            PROGRESS_STEP_PULLING_IMAGE
        );
        assert_eq!(
            uploading.metadata.get(PROGRESS_ACTIVE_STEP_KEY).unwrap(),
            PROGRESS_STEP_STARTING_SANDBOX
        );

        for progress in [
            LaunchProgress::StartingLauncher,
            LaunchProgress::LauncherStarted,
        ] {
            let event = progress.into_event();
            assert_eq!(
                event.metadata.get(PROGRESS_ACTIVE_STEP_KEY).unwrap(),
                PROGRESS_STEP_STARTING_SANDBOX
            );
            assert!(event.metadata.contains_key(PROGRESS_ACTIVE_DETAIL_KEY));
        }

        let failed = LaunchProgress::Failed("upload failed").into_event();
        assert_eq!(failed.r#type, "Warning");
        assert_eq!(failed.reason, "LaunchFailed");
        assert!(failed.message.contains("upload failed"));
        assert!(failed.metadata.is_empty());
    }

    #[tokio::test]
    async fn progress_events_reach_watchers() {
        let driver = driver();
        let mut stream = Box::pin(driver.watch_sandboxes().await);
        driver.publish_progress("sb-1", LaunchProgress::Accepted);
        let event = stream.next().await.unwrap().unwrap();
        let Some(watch_sandboxes_event::Payload::PlatformEvent(platform)) = event.payload else {
            panic!("expected a platform event");
        };
        assert_eq!(platform.sandbox_id, "sb-1");
        assert_eq!(platform.event.unwrap().reason, "Accepted");
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
