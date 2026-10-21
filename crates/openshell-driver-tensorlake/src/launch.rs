// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Launch bundles for the Tensorlake topologies.
//!
//! The driver builds tar archives per sandbox generation. The in-image
//! launcher extracts an archive as root, so each file keeps the owner and
//! mode set here, then starts the role named in the launch parameters.
//!
//! - `/.openshell/channel/sandbox/*` belongs to the workload identity. It holds
//!   only the boundary bootstrap and the boundary TLS server key.
//! - `/.openshell/supervisor/*` belongs to the guard identity (mode 0700). It
//!   holds gateway credentials, which the workload identity cannot read.
//! - `/.openshell/tunnel/*` belongs to the tunnel identity (mode 0700). It
//!   holds the Tensorlake key the guard VM's tunnel authenticates with.
//!
//! The workload VM gets the channel files and listens on a TLS port. The
//! guard VM gets the supervisor and tunnel files; its tunnel dials the
//! workload VM's port on loopback. The workload VM has no outbound network
//! at all (a Tensorlake network policy), so the only path out is the
//! authenticated boundary channel.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use openshell_core::proto::compute::v1::DriverSandbox;
use openshell_isolation_interface::contract::{ProvisioningEvidence, ResolvedWorkloadIdentity};
use openshell_sandbox_backend::boundary_protocol::{
    BoundaryConfig, BoundaryListener, GatewayVerificationKey, SandboxRuntimeDescriptor,
    SandboxTlsClientConfig, SandboxTlsServerConfig, SandboxTransport,
    generate_sandbox_tls_material,
};
use serde::Serialize;

use crate::config::TensorlakeComputeConfig;

/// Path in the VM where the driver uploads the bundle.
pub const BUNDLE_UPLOAD_PATH: &str = "/var/lib/openshell/launch-bundle.tar";
/// Launcher baked into the Tensorlake image.
pub const LAUNCHER_PATH: &str = "/opt/openshell/bin/openshell-tensorlake-launch";
/// Guard identity inside the VM. It differs from the workload identity so the
/// workload cannot read gateway credentials.
pub const SUPERVISOR_UID: u32 = 65_534;
pub const SUPERVISOR_GID: u32 = 65_534;
/// Tunnel identity inside the guard VM. It holds the Tensorlake key, which
/// the guard identity cannot read.
pub const TUNNEL_UID: u32 = 65_533;
pub const TUNNEL_GID: u32 = 65_533;
/// Loopback port in the workload VM where the forwarder accepts the guard's
/// tunnel. The Tensorlake tunnel always delivers to loopback.
pub const WORKLOAD_FORWARD_PORT: u16 = 8443;
/// Port inside the workload network namespace where `openshell-sandbox`
/// listens for the guard. The forwarder connects to it over the veth link,
/// because the runtime rejects loopback peers.
pub const WORKLOAD_LISTEN_PORT: u16 = 8444;
/// Loopback port in the guard VM where the tunnel listens for the guard.
pub const GUARD_TUNNEL_PORT: u16 = 18_443;
/// Forwarder identity inside the workload VM. It owns no files.
pub const FORWARDER_UID: u32 = 65_533;
pub const FORWARDER_GID: u32 = 65_533;

const WORKSPACE: &str = "/sandbox";
const BOOTSTRAP_PATH: &str = "/.openshell/channel/sandbox/bootstrap.json";
const SERVER_CERT_PATH: &str = "/.openshell/channel/sandbox/server.crt";
const SERVER_KEY_PATH: &str = "/.openshell/channel/sandbox/server.key";
const SUPERVISOR_DIR: &str = "/.openshell/supervisor";
const RUNTIME_DESCRIPTOR_PATH: &str = "/.openshell/supervisor/runtime-descriptor.json";
const AUTH_BUNDLE_PATH: &str = "/.openshell/supervisor/auth.json";
const SANDBOX_TOKEN_PATH: &str = "/.openshell/supervisor/sandbox.jwt";
const GATEWAY_CA_PATH: &str = "/.openshell/supervisor/gateway-ca.pem";
const SUPERVISOR_ENV_PATH: &str = "/.openshell/supervisor/env";
const SUPERVISOR_ARGS_PATH: &str = "/.openshell/supervisor/args";
const LAUNCH_PARAMS_PATH: &str = "/.openshell/launch/params";
const SUPERVISOR_RUN_DIR: &str = "/run/openshell";
const SUPERVISOR_HEALTH_SOCKET: &str = "/run/openshell/supervisor-health.sock";
const TUNNEL_DIR: &str = "/.openshell/tunnel";
const TUNNEL_ENV_PATH: &str = "/.openshell/tunnel/env";
const TUNNEL_ARGS_PATH: &str = "/.openshell/tunnel/args";
const TUNNEL_RUN_DIR: &str = "/run/openshell-tunnel";

/// Which processes a launch bundle starts in its VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Only `openshell-sandbox`, listening for a remote guard.
    Workload,
    /// Only the tunnel and `openshell-supervisor`.
    Guard,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Workload => "workload",
            Self::Guard => "guard",
        }
    }
}

fn invalid(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Resolve the workload identity without inspecting the image.
///
/// Tensorlake gives the driver no access to the image's account files, so
/// only numeric selectors are accepted. An empty request uses the `OpenShell`
/// default.
pub fn resolve_identity(
    sandbox: &DriverSandbox,
    image: &str,
) -> Result<ResolvedWorkloadIdentity, String> {
    let request = sandbox
        .spec
        .as_ref()
        .and_then(|spec| spec.workload_identity.as_ref());
    let user = request.map_or("", |identity| identity.user.trim());
    let group = request.map_or("", |identity| identity.group.trim());
    let parse = |value: &str, default: u32, kind: &str| -> Result<u32, String> {
        if value.is_empty() {
            return Ok(default);
        }
        value.parse::<u32>().map_err(|_| {
            format!("the Tensorlake driver accepts only a numeric workload {kind}, got '{value}'")
        })
    };
    let uid = parse(
        user,
        openshell_core::sandbox_env::DEFAULT_SANDBOX_UID,
        "user",
    )?;
    let gid = parse(
        group,
        openshell_core::sandbox_env::DEFAULT_SANDBOX_GID,
        "group",
    )?;
    if [SUPERVISOR_UID, TUNNEL_UID, FORWARDER_UID].contains(&uid)
        || [SUPERVISOR_GID, TUNNEL_GID, FORWARDER_GID].contains(&gid)
    {
        return Err(format!(
            "workload identity {uid}:{gid} is reserved for the OpenShell guard"
        ));
    }
    let source = if user.is_empty() && group.is_empty() {
        "default"
    } else {
        "policy"
    };
    ResolvedWorkloadIdentity::new(uid, gid, Vec::new(), source.into(), image.into())
        .map_err(invalid)
}

/// Evidence for the outer fence the driver requested. The driver cannot
/// inspect the VM, so the evidence names the recipe: the workload runs in a
/// network namespace whose only link leads to the tunnel forwarder, in a VM
/// that has internet access off and an empty allow list. Egress needs a
/// mediated decision, revocation takes effect at the guard, and guard loss
/// leaves the workload without a path.
#[derive(Serialize)]
struct TensorlakeOuterFenceEvidence<'a> {
    topology: &'static str,
    tensorlake_sandbox_id: &'a str,
    guard_tensorlake_sandbox_id: &'a str,
    workload_network: &'static str,
    launcher: &'static str,
}

/// Bind the launch recipe to one generation as provisioning evidence.
fn provisioning_evidence(
    tensorlake_sandbox_id: &str,
    guard_tensorlake_sandbox_id: &str,
    generation: &str,
) -> Result<ProvisioningEvidence, String> {
    if tensorlake_sandbox_id.is_empty() || guard_tensorlake_sandbox_id.is_empty() {
        return Err("Tensorlake sandbox ID is required for provisioning evidence".to_string());
    }
    let evidence = TensorlakeOuterFenceEvidence {
        topology: "split",
        tensorlake_sandbox_id,
        guard_tensorlake_sandbox_id,
        workload_network: "private-netns-veth-to-forwarder-no-egress",
        launcher: LAUNCHER_PATH,
    };
    let encoded = serde_json::to_vec(&evidence).map_err(invalid)?;
    let projection =
        ProvisioningEvidence::from_enforcement_evidence(generation, &encoded).map_err(invalid)?;
    projection.validate(generation).map_err(invalid)?;
    Ok(projection)
}

/// Environment for workload children. Control keys are removed.
fn child_environment(sandbox: &DriverSandbox) -> HashMap<String, String> {
    let mut environment = sandbox
        .spec
        .as_ref()
        .and_then(|spec| spec.template.as_ref())
        .map_or_else(HashMap::new, |template| template.environment.clone());
    if let Some(spec) = sandbox.spec.as_ref() {
        environment.extend(spec.environment.clone());
    }
    for protected in [
        openshell_core::sandbox_env::ENDPOINT,
        openshell_core::sandbox_env::GATEWAY_TLS_SERVER_NAME,
        openshell_core::sandbox_env::MAIN_PROCESS_SPEC,
        openshell_core::sandbox_env::NETWORK_RUNTIME_CAPABILITIES,
        openshell_core::sandbox_env::OCI_IMAGE_USER,
        openshell_core::sandbox_env::SANDBOX,
        openshell_core::sandbox_env::SANDBOX_GID,
        openshell_core::sandbox_env::SANDBOX_ID,
        openshell_core::sandbox_env::SANDBOX_TOKEN,
        openshell_core::sandbox_env::SANDBOX_TOKEN_FILE,
        openshell_core::sandbox_env::SANDBOX_UID,
        openshell_core::sandbox_env::SSH_SOCKET_PATH,
        openshell_core::sandbox_env::TLS_CA,
        openshell_core::sandbox_env::TLS_CERT,
        openshell_core::sandbox_env::TLS_KEY,
        openshell_core::sandbox_env::USER_ENVIRONMENT,
    ] {
        environment.remove(protected);
    }
    environment
}

/// Guard environment, in launch order.
fn supervisor_environment(
    sandbox: &DriverSandbox,
    config: &TensorlakeComputeConfig,
    has_gateway_ca: bool,
) -> Result<Vec<(String, String)>, String> {
    use openshell_core::sandbox_env as env;
    let main_process_spec =
        env::MainProcessConfig::encode_driver_spec(sandbox.spec.as_ref()).map_err(invalid)?;
    let mut environment = vec![
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        (
            env::ADMITTED_ISOLATION_BACKEND.to_string(),
            openshell_sandbox_backend::BACKEND_NAME.to_string(),
        ),
        (env::MAIN_PROCESS_SPEC.to_string(), main_process_spec),
        (env::ENDPOINT.to_string(), config.grpc_endpoint.clone()),
        (env::SANDBOX_ID.to_string(), sandbox.id.clone()),
        (env::SANDBOX.to_string(), sandbox.name.clone()),
        (
            env::SSH_SOCKET_PATH.to_string(),
            openshell_core::container_paths::SSH_SOCKET_PATH.to_string(),
        ),
        (
            env::PROXY_TLS_DIR.to_string(),
            format!("{SUPERVISOR_RUN_DIR}/proxy-tls"),
        ),
        // No transparent interception: the guard never shares the workload's
        // network namespace here, so workload connections arrive over the
        // boundary channel with their identity, as in the Kubernetes driver.
        (env::NETWORK_RUNTIME_CAPABILITIES.to_string(), String::new()),
        (
            env::LOG_LEVEL.to_string(),
            openshell_core::driver_utils::sandbox_log_level(sandbox, &config.log_level),
        ),
        (
            env::TELEMETRY_ENABLED.to_string(),
            openshell_core::telemetry::enabled_env_value().to_string(),
        ),
    ];
    if has_gateway_ca {
        environment.push((env::TLS_CA.to_string(), GATEWAY_CA_PATH.to_string()));
    }
    if sandbox
        .spec
        .as_ref()
        .is_some_and(|spec| !spec.sandbox_token.is_empty())
    {
        environment.push((
            env::SANDBOX_TOKEN_FILE.to_string(),
            SANDBOX_TOKEN_PATH.to_string(),
        ));
    }
    for (key, value) in &environment {
        if key.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!(
                "guard environment entry {key} is not representable"
            ));
        }
    }
    Ok(environment)
}

fn supervisor_args() -> Vec<String> {
    vec![
        format!("--backend-descriptor-file={RUNTIME_DESCRIPTOR_PATH}"),
        format!("--auth-bundle-file={AUTH_BUNDLE_PATH}"),
        format!("--workdir={WORKSPACE}"),
        format!("--health-socket-path={SUPERVISOR_HEALTH_SOCKET}"),
    ]
}

/// NUL-separated list, read by the launcher with `mapfile -d ''`.
fn nul_list<I: IntoIterator<Item = String>>(items: I) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(item.as_bytes());
        out.push(0);
    }
    out
}

pub struct LaunchBundleInput<'a> {
    pub sandbox: &'a DriverSandbox,
    pub config: &'a TensorlakeComputeConfig,
    /// The workload VM.
    pub tensorlake_sandbox_id: &'a str,
    /// The guard VM.
    pub guard_tensorlake_id: &'a str,
    pub identity: &'a ResolvedWorkloadIdentity,
    pub gateway_ca_pem: Option<&'a [u8]>,
}

/// The tar archives for one sandbox generation. Paths are relative to `/`.
#[derive(Debug)]
pub struct LaunchBundles {
    /// Uploaded to the workload VM.
    pub workload: Vec<u8>,
    /// Uploaded to the guard VM.
    pub guard: Vec<u8>,
}

/// Build the launch bundles.
pub fn build_launch_bundles(input: LaunchBundleInput<'_>) -> Result<LaunchBundles, String> {
    let LaunchBundleInput {
        sandbox,
        config,
        tensorlake_sandbox_id,
        guard_tensorlake_id,
        identity,
        gateway_ca_pem,
    } = input;
    let spec = sandbox
        .spec
        .as_ref()
        .ok_or_else(|| "sandbox.spec is required".to_string())?;
    if spec.launch_authentication.is_empty() {
        return Err("Tensorlake sandboxes require launch-scoped gateway authentication".into());
    }
    let launch_authentication: openshell_core::jwt::SandboxLaunchAuthentication =
        serde_json::from_slice(&spec.launch_authentication)
            .map_err(|error| format!("decode launch authentication: {error}"))?;
    launch_authentication.validate().map_err(invalid)?;

    let session_id = launch_authentication.supervisor.session_id;
    let generation = launch_authentication
        .supervisor
        .runtime_generation
        .to_string();
    let tls = generate_sandbox_tls_material(session_id).map_err(invalid)?;
    let provisioning_evidence =
        provisioning_evidence(tensorlake_sandbox_id, guard_tensorlake_id, &generation)?;
    let mut resource_claims = BTreeMap::from([
        (
            "tensorlake.sandbox_id".to_string(),
            tensorlake_sandbox_id.to_string(),
        ),
        (
            "tensorlake.image".to_string(),
            identity.resource_digest.clone(),
        ),
    ]);
    resource_claims.insert(
        "tensorlake.guard_sandbox_id".to_string(),
        guard_tensorlake_id.to_string(),
    );
    let verification_keys = launch_authentication
        .verification_keys
        .iter()
        .map(|key| {
            String::from_utf8(key.public_key_pem.clone())
                .map(|public_key_pem| GatewayVerificationKey {
                    key_id: key.key_id.clone(),
                    public_key_pem,
                })
                .map_err(invalid)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let server_tls = SandboxTlsServerConfig {
        certificate_chain_path: PathBuf::from(SERVER_CERT_PATH),
        private_key_path: PathBuf::from(SERVER_KEY_PATH),
    };
    // The TLS listener must not bind loopback, because workload sockets share
    // it. `0.0.0.0` also accepts the tunnel's connection, which arrives on
    // loopback; the sandbox runtime protects the port from workload sockets.
    let listener = BoundaryListener::TlsTcp {
        address: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), WORKLOAD_LISTEN_PORT),
        tls: server_tls,
    };
    let transport = SandboxTransport::Tcp {
        authority: format!("{tensorlake_sandbox_id}:{WORKLOAD_LISTEN_PORT}"),
        addresses: vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            GUARD_TUNNEL_PORT,
        )],
    };
    let boundary = BoundaryConfig {
        boundary_id: sandbox.id.clone(),
        generation: generation.clone(),
        session_id,
        session_rotation: launch_authentication.supervisor.session_rotation,
        auth_epoch: launch_authentication.supervisor.auth_epoch,
        gateway_id: launch_authentication.gateway_id.clone(),
        verification_keys,
        listener,
        resource_claims: resource_claims.clone(),
        resource_claim_files: BTreeMap::new(),
        workload_identity: identity.clone(),
        provisioning_evidence: provisioning_evidence.clone(),
        child_env: child_environment(sandbox),
    };
    let runtime_descriptor = SandboxRuntimeDescriptor {
        boundary_id: sandbox.id.clone(),
        generation,
        session_id,
        transport,
        tls: SandboxTlsClientConfig {
            server_name: tls.server_name.clone(),
            trust_anchor_pem: tls.trust_anchor_pem.clone(),
        },
        host_gateway_ip: None,
        resource_claims,
        workload_identity: identity.clone(),
        provisioning_evidence,
    };
    let guard_files = GuardFiles {
        runtime_descriptor: &runtime_descriptor,
        auth_bundle: &launch_authentication.supervisor,
        sandbox_token: &spec.sandbox_token,
        gateway_ca_pem,
        environment: supervisor_environment(sandbox, config, gateway_ca_pem.is_some())?,
    };

    let mut workload_archive = Archive::default();
    workload_archive.directory(".openshell", 0o755, ROOT)?;
    add_workload_files(&mut workload_archive, identity, &boundary, &tls)?;
    add_params(&mut workload_archive, Role::Workload, identity)?;
    let mut guard_archive = Archive::default();
    guard_archive.directory(".openshell", 0o755, ROOT)?;
    add_guard_files(&mut guard_archive, &guard_files)?;
    add_tunnel_files(&mut guard_archive, config, tensorlake_sandbox_id)?;
    add_params(&mut guard_archive, Role::Guard, identity)?;
    Ok(LaunchBundles {
        workload: workload_archive.finish()?,
        guard: guard_archive.finish()?,
    })
}

const ROOT: Owner = Owner::new(0, 0);

/// Channel files for the workload identity: the bootstrap and the boundary
/// server key. No gateway or provider secret.
fn add_workload_files(
    archive: &mut Archive,
    identity: &ResolvedWorkloadIdentity,
    boundary: &BoundaryConfig,
    tls: &openshell_sandbox_backend::boundary_protocol::SandboxTlsMaterial,
) -> Result<(), String> {
    let workload = Owner::new(identity.uid, identity.gid);
    archive.directory(".openshell/channel", 0o755, ROOT)?;
    archive.directory(".openshell/channel/sandbox", 0o711, workload)?;
    archive.file(
        BOOTSTRAP_PATH,
        0o600,
        workload,
        &serde_json::to_vec(boundary).map_err(invalid)?,
    )?;
    archive.file(
        SERVER_CERT_PATH,
        0o600,
        workload,
        tls.certificate_chain_pem.as_bytes(),
    )?;
    archive.file(
        SERVER_KEY_PATH,
        0o600,
        workload,
        tls.private_key_pem.as_bytes(),
    )
}

struct GuardFiles<'a> {
    runtime_descriptor: &'a SandboxRuntimeDescriptor,
    auth_bundle: &'a openshell_core::jwt::SupervisorAuthBundle,
    sandbox_token: &'a str,
    gateway_ca_pem: Option<&'a [u8]>,
    environment: Vec<(String, String)>,
}

/// Credentials and settings for the guard identity.
fn add_guard_files(archive: &mut Archive, files: &GuardFiles<'_>) -> Result<(), String> {
    let guard = Owner::new(SUPERVISOR_UID, SUPERVISOR_GID);
    archive.directory(SUPERVISOR_DIR, 0o700, guard)?;
    archive.file(
        RUNTIME_DESCRIPTOR_PATH,
        0o600,
        guard,
        &serde_json::to_vec(files.runtime_descriptor).map_err(invalid)?,
    )?;
    archive.file(
        AUTH_BUNDLE_PATH,
        0o600,
        guard,
        &serde_json::to_vec(files.auth_bundle).map_err(invalid)?,
    )?;
    if !files.sandbox_token.is_empty() {
        archive.file(
            SANDBOX_TOKEN_PATH,
            0o600,
            guard,
            format!("{}\n", files.sandbox_token).as_bytes(),
        )?;
    }
    if let Some(ca) = files.gateway_ca_pem {
        archive.file(GATEWAY_CA_PATH, 0o600, guard, ca)?;
    }
    archive.file(
        SUPERVISOR_ENV_PATH,
        0o600,
        ROOT,
        &nul_list(
            files
                .environment
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        ),
    )?;
    archive.file(
        SUPERVISOR_ARGS_PATH,
        0o600,
        ROOT,
        &nul_list(supervisor_args()),
    )
}

/// The Tensorlake key, the Tensorlake API URLs, and the `tl sbx tunnel`
/// arguments for the tunnel identity in the guard VM.
///
/// The launch script starts the tunnel with `env -i`, so this file is the
/// whole environment the `tl` CLI sees. `tl` reads the sandbox API URL
/// from `TENSORLAKE_SANDBOX_API_URL` and the platform API URL, where it
/// checks the key, from `TENSORLAKE_API_URL`.
fn add_tunnel_files(
    archive: &mut Archive,
    config: &TensorlakeComputeConfig,
    workload_tensorlake_id: &str,
) -> Result<(), String> {
    let tunnel = Owner::new(TUNNEL_UID, TUNNEL_GID);
    if config.api_key.contains(['\0', '\n']) {
        return Err("Tensorlake API key is not representable in the tunnel environment".into());
    }
    if config.api_url.contains(['\0', '\n']) || config.platform_api_url.contains(['\0', '\n']) {
        return Err("Tensorlake API URL is not representable in the tunnel environment".into());
    }
    archive.directory(TUNNEL_DIR, 0o700, tunnel)?;
    archive.file(
        TUNNEL_ENV_PATH,
        0o600,
        tunnel,
        &nul_list([
            format!("TENSORLAKE_API_KEY={}", config.api_key),
            format!("TENSORLAKE_SANDBOX_API_URL={}", config.api_url),
            format!("TENSORLAKE_API_URL={}", config.platform_api_url),
        ]),
    )?;
    archive.file(
        TUNNEL_ARGS_PATH,
        0o600,
        ROOT,
        &nul_list(tunnel_args(workload_tensorlake_id)),
    )
}

fn tunnel_args(workload_tensorlake_id: &str) -> Vec<String> {
    vec![
        "sbx".to_string(),
        "tunnel".to_string(),
        workload_tensorlake_id.to_string(),
        WORKLOAD_FORWARD_PORT.to_string(),
        "--listen-port".to_string(),
        GUARD_TUNNEL_PORT.to_string(),
    ]
}

/// Launch parameters the launcher reads: the role, the identities, and the
/// directories it must create.
fn add_params(
    archive: &mut Archive,
    role: Role,
    identity: &ResolvedWorkloadIdentity,
) -> Result<(), String> {
    use std::fmt::Write as _;
    let mut params = format!("ROLE={}\nWORKSPACE={WORKSPACE}\n", role.as_str());
    match role {
        Role::Workload => writeln!(
            params,
            "WORKLOAD_UID={}\nWORKLOAD_GID={}\nFORWARDER_UID={FORWARDER_UID}\nFORWARDER_GID={FORWARDER_GID}\nWORKLOAD_LISTEN_PORT={WORKLOAD_LISTEN_PORT}\nWORKLOAD_FORWARD_PORT={WORKLOAD_FORWARD_PORT}",
            identity.uid, identity.gid
        ),
        Role::Guard => writeln!(
            params,
            "SUPERVISOR_UID={SUPERVISOR_UID}\nSUPERVISOR_GID={SUPERVISOR_GID}\nSUPERVISOR_RUN_DIR={SUPERVISOR_RUN_DIR}\nTUNNEL_UID={TUNNEL_UID}\nTUNNEL_GID={TUNNEL_GID}\nTUNNEL_RUN_DIR={TUNNEL_RUN_DIR}"
        ),
    }
    .expect("writing to a String cannot fail");
    archive.directory(".openshell/launch", 0o700, ROOT)?;
    archive.file(LAUNCH_PARAMS_PATH, 0o600, ROOT, params.as_bytes())
}

#[derive(Clone, Copy)]
struct Owner {
    uid: u32,
    gid: u32,
}

impl Owner {
    const fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

#[derive(Default)]
struct Archive {
    builder: Option<tar::Builder<Vec<u8>>>,
}

impl Archive {
    fn builder(&mut self) -> &mut tar::Builder<Vec<u8>> {
        self.builder
            .get_or_insert_with(|| tar::Builder::new(Vec::new()))
    }

    fn directory(&mut self, path: &str, mode: u32, owner: Owner) -> Result<(), String> {
        self.append(path, mode, owner, tar::EntryType::Directory, &[])
    }

    fn file(&mut self, path: &str, mode: u32, owner: Owner, content: &[u8]) -> Result<(), String> {
        self.append(path, mode, owner, tar::EntryType::Regular, content)
    }

    fn append(
        &mut self,
        path: &str,
        mode: u32,
        owner: Owner,
        kind: tar::EntryType,
        content: &[u8],
    ) -> Result<(), String> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(u64::from(owner.uid));
        header.set_gid(u64::from(owner.gid));
        header.set_size(content.len() as u64);
        header.set_mtime(0);
        header.set_cksum();
        self.builder()
            .append_data(&mut header, path.trim_start_matches('/'), content)
            .map_err(invalid)
    }

    fn finish(mut self) -> Result<Vec<u8>, String> {
        self.builder().finish().map_err(invalid)?;
        self.builder
            .take()
            .expect("builder was created above")
            .into_inner()
            .map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::proto::compute::v1::{DriverSandboxSpec, WorkloadIdentityRequest};
    use std::io::Read;

    fn sandbox_with_identity(user: &str, group: &str) -> DriverSandbox {
        DriverSandbox {
            id: "sb-1".into(),
            name: "demo".into(),
            spec: Some(DriverSandboxSpec {
                workload_identity: Some(WorkloadIdentityRequest {
                    user: user.into(),
                    group: group.into(),
                }),
                ..DriverSandboxSpec::default()
            }),
            ..DriverSandbox::default()
        }
    }

    #[test]
    fn identity_defaults_to_openshell_sandbox_user() {
        let identity = resolve_identity(&sandbox_with_identity("", ""), "img").unwrap();
        assert_eq!((identity.uid, identity.gid), (1000, 1000));
        assert_eq!(identity.source, "default");
    }

    #[test]
    fn identity_accepts_numeric_selectors() {
        let identity = resolve_identity(&sandbox_with_identity("2000", "3000"), "img").unwrap();
        assert_eq!((identity.uid, identity.gid), (2000, 3000));
        assert_eq!(identity.source, "policy");
    }

    #[test]
    fn identity_rejects_names_root_and_guard() {
        assert!(resolve_identity(&sandbox_with_identity("sandbox", ""), "img").is_err());
        assert!(resolve_identity(&sandbox_with_identity("0", ""), "img").is_err());
        assert!(resolve_identity(&sandbox_with_identity("65534", ""), "img").is_err());
    }

    #[test]
    fn provisioning_evidence_binds_generation() {
        let evidence = provisioning_evidence("tl-1", "tl-1-guard", "gen-1").unwrap();
        assert!(evidence.validate("gen-1").is_ok());
        assert!(evidence.validate("gen-2").is_err());
        let other = provisioning_evidence("tl-2", "tl-2-guard", "gen-1").unwrap();
        assert_ne!(evidence.evidence_digest, other.evidence_digest);
        assert!(provisioning_evidence("", "tl-1-guard", "gen-1").is_err());
        assert!(provisioning_evidence("tl-1", "", "gen-1").is_err());
        assert!(provisioning_evidence("tl-1", "tl-1-guard", "").is_err());
    }

    #[test]
    fn child_environment_strips_control_keys() {
        let mut sandbox = sandbox_with_identity("", "");
        let spec = sandbox.spec.as_mut().unwrap();
        spec.environment.insert(
            openshell_core::sandbox_env::SANDBOX_TOKEN.into(),
            "x".into(),
        );
        spec.environment.insert("FOO".into(), "bar".into());
        let environment = child_environment(&sandbox);
        assert_eq!(environment.get("FOO").map(String::as_str), Some("bar"));
        assert!(!environment.contains_key(openshell_core::sandbox_env::SANDBOX_TOKEN));
    }

    #[test]
    fn bundle_requires_launch_authentication() {
        let sandbox = sandbox_with_identity("", "");
        let identity = resolve_identity(&sandbox, "img").unwrap();
        let config = crate::config::tests::test_config();
        let result = build_launch_bundles(LaunchBundleInput {
            sandbox: &sandbox,
            config: &config,
            tensorlake_sandbox_id: "tl-1",
            guard_tensorlake_id: "tl-1-guard",
            identity: &identity,
            gateway_ca_pem: None,
        });
        assert!(result.unwrap_err().contains("launch-scoped"));
    }

    fn launch_authentication() -> Vec<u8> {
        use openshell_core::jwt::{
            CredentialEpoch, SandboxLaunchAuthentication, SecretJwt, SessionVerificationKey,
            SupervisorAuthBundle,
        };
        serde_json::to_vec(&SandboxLaunchAuthentication {
            supervisor: SupervisorAuthBundle {
                session_id: openshell_core::SandboxSessionId::new(),
                runtime_generation: openshell_core::sandbox_generation::SandboxGenerationId::parse(
                    "generation-1",
                )
                .unwrap(),
                session_rotation: openshell_core::jwt::SessionRotation::new(1).unwrap(),
                auth_epoch: CredentialEpoch::new(1).unwrap(),
                gateway_token: SecretJwt::parse("gateway.token.value").unwrap(),
                gateway_expires_at: i64::MAX,
                sandbox_token: SecretJwt::parse("sandbox.token.value").unwrap(),
                sandbox_expires_at: i64::MAX,
                ssh_host_private_key: None,
                ocsf_otlp_headers: None,
            },
            gateway_id: "gateway-test".to_string(),
            verification_keys: vec![SessionVerificationKey {
                key_id: "test-key".to_string(),
                public_key_pem: b"public-key".to_vec(),
            }],
        })
        .unwrap()
    }

    fn read_entries(bytes: &[u8]) -> HashMap<String, (u32, u64, Vec<u8>)> {
        let mut reader = tar::Archive::new(bytes);
        let mut entries = HashMap::new();
        for entry in reader.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            entries.insert(
                entry.path().unwrap().display().to_string(),
                (header.mode().unwrap(), header.uid().unwrap(), content),
            );
        }
        entries
    }

    fn launch_sandbox() -> DriverSandbox {
        let mut sandbox = sandbox_with_identity("", "");
        let spec = sandbox.spec.as_mut().unwrap();
        spec.launch_authentication = launch_authentication();
        spec.sandbox_token = "sandbox.jwt.value".into();
        spec.environment.insert("FOO".into(), "bar".into());
        sandbox
    }

    fn entry<'a>(
        entries: &'a HashMap<String, (u32, u64, Vec<u8>)>,
        path: &str,
    ) -> &'a (u32, u64, Vec<u8>) {
        entries
            .get(path.trim_start_matches('/'))
            .unwrap_or_else(|| panic!("{path} missing from bundle"))
    }

    #[test]
    fn split_bundles_keep_every_secret_out_of_the_workload_vm() {
        let sandbox = launch_sandbox();
        let identity = resolve_identity(&sandbox, "img").unwrap();
        let mut config = crate::config::tests::test_config();
        config.api_key = "tl_secret_key".into();
        config.api_url = "https://sandbox.example.test".into();
        config.platform_api_url = "https://api.example.test".into();
        let bundles = build_launch_bundles(LaunchBundleInput {
            sandbox: &sandbox,
            config: &config,
            tensorlake_sandbox_id: "tl-1",
            guard_tensorlake_id: "tl-1-guard",
            identity: &identity,
            gateway_ca_pem: Some(b"ca"),
        })
        .unwrap();

        // Workload VM: channel files and the workload role only.
        let workload = read_entries(&bundles.workload);
        let workload_text = String::from_utf8_lossy(&bundles.workload);
        for secret in ["gateway.token.value", "sandbox.jwt.value", "tl_secret_key"] {
            assert!(
                !workload_text.contains(secret),
                "{secret} leaked to the workload VM"
            );
        }
        for path in [
            AUTH_BUNDLE_PATH,
            SANDBOX_TOKEN_PATH,
            SUPERVISOR_DIR,
            TUNNEL_ENV_PATH,
        ] {
            assert!(
                !workload.contains_key(path.trim_start_matches('/')),
                "{path}"
            );
        }
        let bootstrap = String::from_utf8_lossy(&entry(&workload, BOOTSTRAP_PATH).2);
        assert!(bootstrap.contains("\"kind\":\"tls-tcp\""));
        assert!(bootstrap.contains(&format!("0.0.0.0:{WORKLOAD_LISTEN_PORT}")));
        assert!(bootstrap.contains("tl-1-guard"));
        let params = String::from_utf8_lossy(&entry(&workload, LAUNCH_PARAMS_PATH).2);
        assert!(params.contains("ROLE=workload\n"));
        assert!(params.contains("WORKLOAD_UID=1000\n"));
        assert!(params.contains("FORWARDER_UID=65533\n"));
        assert!(params.contains("WORKLOAD_LISTEN_PORT=8444\n"));
        assert!(params.contains("WORKLOAD_FORWARD_PORT=8443\n"));
        assert!(!params.contains("SUPERVISOR_UID"));

        // Guard VM: credentials for the guard, the key for the tunnel, no
        // channel server key.
        let guard = read_entries(&bundles.guard);
        assert!(!guard.contains_key(SERVER_KEY_PATH.trim_start_matches('/')));
        assert!(!guard.contains_key(BOOTSTRAP_PATH.trim_start_matches('/')));
        let descriptor = String::from_utf8_lossy(&entry(&guard, RUNTIME_DESCRIPTOR_PATH).2);
        assert!(descriptor.contains(&format!("127.0.0.1:{GUARD_TUNNEL_PORT}")));
        assert!(descriptor.contains(&format!("tl-1:{WORKLOAD_LISTEN_PORT}")));
        let (mode, uid, env) = entry(&guard, TUNNEL_ENV_PATH);
        assert_eq!((*mode, *uid), (0o600, u64::from(TUNNEL_UID)));
        assert_eq!(
            env,
            b"TENSORLAKE_API_KEY=tl_secret_key\0TENSORLAKE_SANDBOX_API_URL=https://sandbox.example.test\0TENSORLAKE_API_URL=https://api.example.test\0"
        );
        let (mode, uid, _) = entry(&guard, TUNNEL_DIR);
        assert_eq!((*mode, *uid), (0o700, u64::from(TUNNEL_UID)));
        let args = String::from_utf8_lossy(&entry(&guard, TUNNEL_ARGS_PATH).2);
        assert_eq!(
            args,
            format!(
                "sbx\0tunnel\0tl-1\0{WORKLOAD_FORWARD_PORT}\0--listen-port\0{GUARD_TUNNEL_PORT}\0"
            )
        );
        let (mode, uid, _) = entry(&guard, AUTH_BUNDLE_PATH);
        assert_eq!((*mode, *uid), (0o600, u64::from(SUPERVISOR_UID)));
        let params = String::from_utf8_lossy(&entry(&guard, LAUNCH_PARAMS_PATH).2);
        assert!(params.contains("ROLE=guard\n"));
        assert!(params.contains("TUNNEL_UID=65533\n"));
        assert!(params.contains("SUPERVISOR_UID=65534\n"));
        assert!(!params.contains("WORKLOAD_UID"));
    }

    #[test]
    fn archive_sets_owner_and_mode() {
        let mut archive = Archive::default();
        archive
            .directory(
                SUPERVISOR_DIR,
                0o700,
                Owner::new(SUPERVISOR_UID, SUPERVISOR_GID),
            )
            .unwrap();
        archive
            .file(
                AUTH_BUNDLE_PATH,
                0o600,
                Owner::new(SUPERVISOR_UID, SUPERVISOR_GID),
                b"{}",
            )
            .unwrap();
        let bytes = archive.finish().unwrap();
        let mut reader = tar::Archive::new(bytes.as_slice());
        let mut entries = Vec::new();
        for entry in reader.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            entries.push((
                entry.path().unwrap().display().to_string(),
                header.mode().unwrap(),
                header.uid().unwrap(),
                content,
            ));
        }
        assert_eq!(entries[0].0, ".openshell/supervisor");
        assert_eq!(entries[0].1, 0o700);
        assert_eq!(entries[1].0, ".openshell/supervisor/auth.json");
        assert_eq!(entries[1].1, 0o600);
        assert_eq!(entries[1].2, u64::from(SUPERVISOR_UID));
        assert_eq!(entries[1].3, b"{}");
    }

    #[test]
    fn nul_list_terminates_each_entry() {
        assert_eq!(nul_list(["a=1".to_string(), "b".to_string()]), b"a=1\0b\0");
    }
}
