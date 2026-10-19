// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Launch bundle for the combined Tensorlake topology.
//!
//! The guard (`openshell-supervisor`) and the starter (`openshell-sandbox`)
//! run in the same Tensorlake VM. The driver builds one tar archive per
//! sandbox generation. The in-image launcher extracts it as root, so each file
//! keeps the owner and mode set here:
//!
//! - `/.openshell/channel/sandbox/*` belongs to the workload identity. It holds
//!   only the boundary bootstrap and the boundary TLS server key.
//! - `/.openshell/supervisor/*` belongs to the guard identity (mode 0700). It
//!   holds gateway credentials, which the workload identity cannot read.
//!
//! The launcher starts the workload in a new network namespace that has only
//! loopback. This is the outer fence: the workload has no network path except
//! the authenticated boundary channel, even if the guard exits.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use openshell_core::proto::compute::v1::DriverSandbox;
use openshell_isolation_interface::contract::{
    OuterFenceGuarantee, OuterFenceGuarantees, ResolvedWorkloadIdentity,
};
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

const WORKSPACE: &str = "/sandbox";
const SOCKET_PATH: &str = "/.openshell/channel/sandbox/control.sock";
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
    if uid == SUPERVISOR_UID || gid == SUPERVISOR_GID {
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

/// Evidence for the outer fence that the launcher builds. The driver cannot
/// inspect the VM, so the evidence names the launch recipe it requested.
#[derive(Serialize)]
struct TensorlakeOuterFenceEvidence<'a> {
    tensorlake_sandbox_id: &'a str,
    workload_network_namespace: &'static str,
    launcher: &'static str,
}

fn outer_fence(
    tensorlake_sandbox_id: &str,
    generation: &str,
) -> Result<OuterFenceGuarantees, String> {
    let evidence = TensorlakeOuterFenceEvidence {
        tensorlake_sandbox_id,
        workload_network_namespace: "private-loopback-only",
        launcher: LAUNCHER_PATH,
    };
    // A loopback-only network namespace has no route out. Egress needs a
    // mediated decision, revocation takes effect at the guard, and guard loss
    // leaves the namespace without a path.
    let projection = OuterFenceGuarantees::from_enforcement_evidence(
        generation,
        [
            OuterFenceGuarantee::DefaultDenyEgress,
            OuterFenceGuarantee::NoUnmanagedEgressPath,
            OuterFenceGuarantee::RevocationVerified,
            OuterFenceGuarantee::ControllerLossFailsClosed,
        ],
        &serde_json::to_vec(&evidence).map_err(invalid)?,
    )
    .map_err(invalid)?;
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
        (
            env::NETWORK_RUNTIME_CAPABILITIES.to_string(),
            env::POLICY_DNS_TRANSPARENT_TCP_CAPABILITY.to_string(),
        ),
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
    pub tensorlake_sandbox_id: &'a str,
    pub identity: &'a ResolvedWorkloadIdentity,
    pub gateway_ca_pem: Option<&'a [u8]>,
}

/// Build the launch bundle tar archive. Paths are relative to `/`.
pub fn build_launch_bundle(input: LaunchBundleInput<'_>) -> Result<Vec<u8>, String> {
    let LaunchBundleInput {
        sandbox,
        config,
        tensorlake_sandbox_id,
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
    let outer_fence = outer_fence(tensorlake_sandbox_id, &generation)?;
    let resource_claims = BTreeMap::from([
        (
            "tensorlake.sandbox_id".to_string(),
            tensorlake_sandbox_id.to_string(),
        ),
        (
            "tensorlake.image".to_string(),
            identity.resource_digest.clone(),
        ),
    ]);
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

    let boundary = BoundaryConfig {
        boundary_id: sandbox.id.clone(),
        generation: generation.clone(),
        session_id,
        session_rotation: launch_authentication.supervisor.session_rotation,
        auth_epoch: launch_authentication.supervisor.auth_epoch,
        gateway_id: launch_authentication.gateway_id.clone(),
        verification_keys,
        listener: BoundaryListener::Unix {
            socket_path: PathBuf::from(SOCKET_PATH),
            tls: SandboxTlsServerConfig {
                certificate_chain_path: PathBuf::from(SERVER_CERT_PATH),
                private_key_path: PathBuf::from(SERVER_KEY_PATH),
            },
        },
        resource_claims: resource_claims.clone(),
        resource_claim_files: BTreeMap::new(),
        workload_identity: identity.clone(),
        outer_fence: outer_fence.clone(),
        child_env: child_environment(sandbox),
    };
    let runtime_descriptor = SandboxRuntimeDescriptor {
        boundary_id: sandbox.id.clone(),
        generation,
        session_id,
        transport: SandboxTransport::Unix {
            socket_path: PathBuf::from(SOCKET_PATH),
        },
        tls: SandboxTlsClientConfig {
            server_name: tls.server_name.clone(),
            trust_anchor_pem: tls.trust_anchor_pem.clone(),
        },
        host_gateway_ip: None,
        resource_claims,
        workload_identity: identity.clone(),
        outer_fence,
    };

    let workload = Owner::new(identity.uid, identity.gid);
    let guard = Owner::new(SUPERVISOR_UID, SUPERVISOR_GID);
    let root = Owner::new(0, 0);
    let mut archive = Archive::default();
    archive.directory(".openshell", 0o755, root)?;
    archive.directory(".openshell/channel", 0o755, root)?;
    archive.directory(".openshell/channel/sandbox", 0o711, workload)?;
    archive.file(
        BOOTSTRAP_PATH,
        0o600,
        workload,
        &serde_json::to_vec(&boundary).map_err(invalid)?,
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
    )?;

    archive.directory(SUPERVISOR_DIR, 0o700, guard)?;
    archive.file(
        RUNTIME_DESCRIPTOR_PATH,
        0o600,
        guard,
        &serde_json::to_vec(&runtime_descriptor).map_err(invalid)?,
    )?;
    archive.file(
        AUTH_BUNDLE_PATH,
        0o600,
        guard,
        &serde_json::to_vec(&launch_authentication.supervisor).map_err(invalid)?,
    )?;
    if !spec.sandbox_token.is_empty() {
        archive.file(
            SANDBOX_TOKEN_PATH,
            0o600,
            guard,
            format!("{}\n", spec.sandbox_token).as_bytes(),
        )?;
    }
    if let Some(ca) = gateway_ca_pem {
        archive.file(GATEWAY_CA_PATH, 0o600, guard, ca)?;
    }
    let environment = supervisor_environment(sandbox, config, gateway_ca_pem.is_some())?;
    archive.file(
        SUPERVISOR_ENV_PATH,
        0o600,
        root,
        &nul_list(
            environment
                .into_iter()
                .map(|(key, value)| format!("{key}={value}")),
        ),
    )?;
    archive.file(
        SUPERVISOR_ARGS_PATH,
        0o600,
        root,
        &nul_list(supervisor_args()),
    )?;

    archive.directory(".openshell/launch", 0o700, root)?;
    let params = format!(
        "WORKLOAD_UID={}\nWORKLOAD_GID={}\nSUPERVISOR_UID={SUPERVISOR_UID}\nSUPERVISOR_GID={SUPERVISOR_GID}\nWORKSPACE={WORKSPACE}\nSUPERVISOR_RUN_DIR={SUPERVISOR_RUN_DIR}\n",
        identity.uid, identity.gid,
    );
    archive.file(LAUNCH_PARAMS_PATH, 0o600, root, params.as_bytes())?;
    archive.finish()
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
    fn outer_fence_is_complete() {
        let fence = outer_fence("tl-1", "gen-1").unwrap();
        assert!(fence.validate("gen-1").is_ok());
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
        let result = build_launch_bundle(LaunchBundleInput {
            sandbox: &sandbox,
            config: &config,
            tensorlake_sandbox_id: "tl-1",
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

    #[test]
    fn bundle_separates_workload_and_guard_material() {
        let mut sandbox = sandbox_with_identity("", "");
        let spec = sandbox.spec.as_mut().unwrap();
        spec.launch_authentication = launch_authentication();
        spec.sandbox_token = "sandbox.jwt.value".into();
        spec.environment.insert("FOO".into(), "bar".into());
        let identity = resolve_identity(&sandbox, "img").unwrap();
        let config = crate::config::tests::test_config();
        let bytes = build_launch_bundle(LaunchBundleInput {
            sandbox: &sandbox,
            config: &config,
            tensorlake_sandbox_id: "tl-1",
            identity: &identity,
            gateway_ca_pem: Some(b"ca"),
        })
        .unwrap();
        let entries = read_entries(&bytes);

        // Workload side: bootstrap and boundary key only, owned by the workload.
        let (mode, uid, bootstrap) = &entries[BOOTSTRAP_PATH.trim_start_matches('/')];
        assert_eq!((*mode, *uid), (0o600, 1000));
        let bootstrap = String::from_utf8_lossy(bootstrap);
        assert!(bootstrap.contains("\"FOO\""));
        assert!(!bootstrap.contains("gateway.token.value"));
        assert!(!bootstrap.contains("sandbox.jwt.value"));

        // Guard side: credentials owned by the guard identity.
        for path in [AUTH_BUNDLE_PATH, SANDBOX_TOKEN_PATH, GATEWAY_CA_PATH] {
            let (mode, uid, _) = &entries[path.trim_start_matches('/')];
            assert_eq!((*mode, *uid), (0o600, u64::from(SUPERVISOR_UID)), "{path}");
        }
        let (mode, uid, _) = &entries[SUPERVISOR_DIR.trim_start_matches('/')];
        assert_eq!((*mode, *uid), (0o700, u64::from(SUPERVISOR_UID)));

        let (_, _, env) = &entries[SUPERVISOR_ENV_PATH.trim_start_matches('/')];
        let env = String::from_utf8_lossy(env);
        assert!(env.contains("OPENSHELL_ENDPOINT=https://gateway.example.com:8080\0"));
        assert!(env.contains(&format!("OPENSHELL_TLS_CA={GATEWAY_CA_PATH}\0")));
        assert!(env.contains(&format!(
            "OPENSHELL_SANDBOX_TOKEN_FILE={SANDBOX_TOKEN_PATH}\0"
        )));
        assert!(!env.contains("sandbox.jwt.value"));

        let (_, _, params) = &entries[LAUNCH_PARAMS_PATH.trim_start_matches('/')];
        assert!(String::from_utf8_lossy(params).contains("WORKLOAD_UID=1000\n"));
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
