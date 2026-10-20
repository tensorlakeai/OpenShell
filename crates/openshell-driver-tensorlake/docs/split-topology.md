# Split topology: the guard outside the workload VM

Status: design. Not implemented. It needs one Tensorlake capability that is
not confirmed yet, listed under [What Tensorlake must provide](#what-tensorlake-must-provide).

## Problem

Today the guard (`openshell-supervisor`) and the workload (`openshell-sandbox`)
share one Tensorlake VM. The guard holds the gateway session token and the
provider credentials. A root user in that VM can read them, and anyone with
access to the Tensorlake project can become root with `tl sbx exec --user
root`. So the driver is safe for test credentials only.

The VM driver (`openshell-driver-vm`) does not have this gap. Its guard runs
on the gateway host and reaches the guest over a vsock channel. Credentials
never enter the VM. This document describes how to get the same property on
Tensorlake.

## What the protocol already supports

The sandbox protocol between the guard and the workload is TLS with keys that
the driver creates for each sandbox generation. It does not depend on a Unix
socket. The shared types in `openshell-sandbox-backend` and `openshell-core`
offer three transports:

| Side | Type | Variants |
|---|---|---|
| Workload listens | `BoundaryListener` | `Unix`, `TlsTcp { address }`, `Vsock` |
| Guard connects | `SandboxTransport` | `Unix`, `Tcp { authority, addresses }`, `Vsock` |

The VM driver uses `Vsock`. The Tensorlake driver uses `Unix` today. The
split topology uses `TlsTcp` on the workload side and `Tcp` on the guard side.
`Tcp.addresses` must be concrete `IP:port` pairs; a hostname is not accepted.
No change to `openshell-supervisor` or `openshell-sandbox` is needed.

## Two placements

Both placements move the guard out of the workload VM. They differ in how
many guard processes share one VM.

### A. Guard in the control sandbox (recommended first)

The control sandbox already runs the gateway and the driver. It holds the
Tensorlake key, so it is already the trusted component. Run one
`openshell-supervisor` process per sandbox inside it, the same way the VM
driver runs one supervisor per sandbox on the gateway host.

```
 control sandbox (trusted)                      workload sandbox (untrusted)
┌─────────────────────────────────┐             ┌──────────────────────────┐
│ gateway ◄── loopback ── guard ──┼── TLS/TCP ──► openshell-sandbox        │
│ driver ──► Tensorlake API       │             │   └─► agent processes    │
└─────────────────────────────────┘             └──────────────────────────┘
```

- The guard dials the gateway on loopback. The public gateway URL is still
  needed for users, but not for guards. The idle-timeout problem in the README
  goes away for the workload VM, because the guard's connection to it is
  incoming traffic.
- The workload VM gets a Tensorlake network policy with
  `allow_internet_access: false` and an empty `allow_out`. It needs no
  egress at all: the workload's connections travel over the boundary channel
  to the guard, which applies policy and connects upstream. This network
  policy is the outer fence, and the driver can record it as fence evidence
  from the API response rather than from a launch recipe it cannot inspect.
- One image: the sandbox image keeps only `openshell-sandbox` and a smaller
  launch script. The control image already has the gateway and the driver and
  gains `openshell-supervisor`.
- The driver spawns and reaps the supervisor processes. The VM driver's
  `spawn_host_supervisor` in `crates/openshell-driver-vm/src/driver.rs` is the
  model: an environment and argument list, a run directory per sandbox, and a
  wait on the child.

The cost is that the control sandbox concentrates privilege. It does today
already. Keep it in its own Tensorlake project.

### B. One guard VM per sandbox

Create two Tensorlake sandboxes per OpenShell sandbox: `openshell-<id>` for
the workload and `openshell-<id>-guard` for the guard. The guard VM runs from a
small guard image and dials the gateway over the public URL as today.

This keeps the control sandbox small and gives each guard its own kernel. It
doubles the VM count and the boot time, needs a second image, and needs
pairwise recovery after a driver restart. Do it only if placement A proves
too concentrated.

## Driver changes for placement A

1. **Launch bundle.** Split `launch::build_launch_bundle` into a workload
   bundle (bootstrap, server certificate and key, launch parameters) and a
   guard bundle (runtime descriptor, auth bundle, sandbox token, gateway CA).
   Only the workload bundle is uploaded. The guard bundle is written to a
   private directory in the control sandbox, mode 0700, owned by the driver
   user.
2. **Listener.** The bootstrap's `BoundaryListener` becomes
   `TlsTcp { address: 0.0.0.0:<port> }`. The launch script no longer creates a
   network namespace. It starts `openshell-sandbox launch-capability-free`
   directly.
3. **Transport.** The runtime descriptor's `SandboxTransport` becomes
   `Tcp { authority: "openshell-<id>", addresses: [<workload-ip>:<port>] }`.
   The driver must know the workload VM's address when the VM reports
   `running`. See the next section.
4. **Supervisor process.** After the launcher starts, the driver spawns
   `openshell-supervisor` in the control sandbox with the guard bundle. It
   records the child in the sandbox `Record` and treats child exit the same as
   `LauncherExited` today.
5. **Network policy.** `CreateSandboxRequest.network` is always set for the
   workload VM: `allow_internet_access: false`, `allow_out: []`. The
   `--allow-out` flag then applies to the control sandbox, not to workload
   VMs, and can be removed from the driver.
6. **Fence evidence.** Replace `TensorlakeOuterFenceEvidence.workload_network_namespace`
   with the applied Tensorlake network policy.
7. **Recovery.** On restart, the driver finds workload VMs by name as today,
   and restarts a supervisor for each one from the saved guard bundle. Guard
   bundles therefore must survive a driver restart, which they do in the
   control sandbox's `/var/lib/openshell`.
8. **Control image.** Add `openshell-supervisor` from the published
   supervisor image and `ca-certificates` for upstream TLS.

Steps 1 to 3 are pure Rust changes with unit tests. Steps 4 to 8 need a
Tensorlake account to test end to end.

## What Tensorlake must provide

The guard must open a raw TCP connection to the workload VM and complete a
TLS handshake with the workload's own certificate. The Tensorlake port proxy
cannot carry this channel, because it terminates TLS and presents its own
certificate.

The driver needs one of:

- **Private sandbox-to-sandbox networking.** Sandboxes in one project can
  reach each other on a private IP, and the API returns that IP in the
  sandbox description. This is the clean answer. `SandboxInfo` today exposes
  `sandbox_url` and `routing_hint` only.
- **A raw TCP port forward.** An exposed port that forwards bytes without TLS
  termination, reachable from the control sandbox, plus a stable address for
  it.

In both cases the workload VM must be able to accept the connection with
internet access turned off.

Until one of these exists, the combined topology stays, and the README's
warning about test credentials stands.

## Open questions

- Does a Tensorlake network policy with `allow_internet_access: false` block
  inbound connections too, or only outbound?
- How many supervisor processes can the control sandbox run with 2 vCPUs and
  4 GiB? The VM driver's supervisor is small, but the limit sets the
  per-control-plane sandbox count.
- Can the driver learn the workload VM's address before the launcher starts,
  so the guard bundle is complete at launch time, or must it start the
  supervisor only after the first poll shows the address?
