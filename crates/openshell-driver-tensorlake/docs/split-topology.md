# Split topology: the guard in its own VM

Status: implemented as the default topology (`--topology split`) and
verified end to end on Tensorlake Cloud on 2026-10-20, including policy
enforcement with binary identity. The combined topology
(`--topology combined`) stays for tests and for accounts that cannot afford
two VMs per sandbox.

## Problem

In the combined topology the guard (`openshell-supervisor`) and the workload
(`openshell-sandbox`) share one Tensorlake VM. The guard holds the gateway
session token and the provider credentials. The workload cannot read them in
normal operation: it runs as a different user, with `no_new_privs`, in a
network namespace that has only loopback. But one kernel exploit in that VM
gives the agent everything. That is container-grade separation. The VM
driver (`openshell-driver-vm`) gives hypervisor-grade separation, because its
guard runs outside the guest. This document describes how the Tensorlake
driver gets the same property.

## Design

Two Tensorlake sandboxes per OpenShell sandbox:

| Tensorlake sandbox | Runs | Network | Holds |
|---|---|---|---|
| `openshell-<id>` (workload VM) | `openshell-sandbox` and the agent in a private network namespace, plus a tunnel forwarder | Internet off, empty allow list. No outbound path at all. | Its own channel key. No gateway or provider secret. |
| `openshell-<id>-guard` (guard VM) | `openshell-supervisor` and a `tl` tunnel | Default internet, or `--allow-out` plus the gateway and Tensorlake hosts | Gateway token, provider credentials, a Tensorlake API key for the tunnel |

```
 control sandbox          guard VM (trusted)                 workload VM (untrusted)
┌───────────────┐  HTTPS ┌──────────────────────────┐        ┌──────────────────────────────────────┐
│ gateway ◄─────┼────────┼─ openshell-supervisor    │        │ 127.0.0.1:8443 socat forwarder       │
│ driver ─► API │        │    │ OpenShell TLS        │ WSS to │    │ veth 169.254.77.1 -> .2:8444    │
└───────────────┘        │    ▼                     │ proxy  │ ┌──▼── netns openshell-workload ────┐│
                         │ 127.0.0.1:18443 tunnel ──┼───────►│ │ openshell-sandbox :8444 TLS       ││
                         └──────────────────────────┘        │ │   └─► agent processes (lo only)   ││
                                                             │ └───────────────────────────────────┘│
                                                             └──────────────────────────────────────┘
```

The guard dials `127.0.0.1:18443` in its own VM. The `tl sbx tunnel` process
there carries the bytes to the Tensorlake sandbox proxy over a WebSocket,
and the proxy delivers them to `127.0.0.1:8443` in the workload VM. A
`socat` forwarder accepts them there and connects into the workload's
network namespace over a veth link, from `169.254.77.1` to the runtime at
`169.254.77.2:8444`. The tunnel and the forwarder do not inspect the bytes.
OpenShell's own TLS handshake runs end to end between guard and workload,
with the per-generation certificate the driver issued.

The forwarder exists because the sandbox runtime rejects boundary peers
that are loopback or share its own address (`reject_workload_tcp_peer` in
`openshell-sandbox`), and the Tensorlake tunnel always delivers to loopback.
Measured: the runtime reset every tunnel connection until the forwarder
gave it a veth peer. The namespace also keeps the workload's loopback as
empty as in the combined topology, so the workload cannot reach the VM's
own SSH daemon or the forwarder.

This is the same shape as the Kubernetes driver: one guard pod per workload
pod, a TLS listener in the workload, a NetworkPolicy that gives the workload
no egress. Tensorlake's network policy is the outer fence here.

## What the protocol already supports

The sandbox protocol does not depend on a Unix socket. The shared types in
`openshell-sandbox-backend` and `openshell-core` offer three transports:

| Side | Type | Used here |
|---|---|---|
| Workload listens | `BoundaryListener::TlsTcp { address }` | `0.0.0.0:8444` inside the workload namespace |
| Guard connects | `SandboxTransport::Tcp { authority, addresses }` | authority `<workload-id>:8443`, address `127.0.0.1:18443` |

The listener must not bind a loopback address, because workload sockets
share loopback, so it binds `0.0.0.0`. The sandbox runtime then protects the
listener port from workload connections itself, and rejects loopback peers
in `accept`, which is why the forwarder connects from a veth address.
`Tcp.addresses` must be concrete `IP:port` pairs, which `127.0.0.1:18443`
is. No change to `openshell-supervisor` or `openshell-sandbox` was needed.

## Why the tunnel and not a sandbox IP

The Tensorlake HTTPS port proxy terminates TLS and presents its own
certificate, so it cannot carry the OpenShell channel. Tensorlake has no
private sandbox-to-sandbox network and no sandbox IP in the API. The tunnel
is the one path that relays raw TCP between two sandboxes today. Measured on
Tensorlake Cloud on 2026-10-20:

| Check | Result |
|---|---|
| Tunnel into a sandbox with internet off | Works. The fence can be fully closed. |
| Target port not in `exposed_ports` | Works. The port is never public. |
| Address the tunnel dials inside the target | `127.0.0.1:<port>` only, not the `eth0` address |
| 100 tunnels from one sandbox | All alive, ~50 ms to open each |
| 500 concurrent connections through one tunnel | All held and answering |
| Throughput through one tunnel | ~300 MiB/s each direction |
| Idle tunnel | Still relaying after 13 minutes |
| `tl sbx tunnel` process | 15 MiB RSS |

If Tensorlake adds private networking with a sandbox IP later, the driver
can put that IP into `Tcp.addresses` and drop the tunnel. Nothing else
changes.

## What the driver does

1. **Create.** Two Tensorlake create calls: the workload VM from the sandbox
   image with `allow_internet_access: false` and an empty `allow_out`, and
   the guard VM from the guard image with the operator's `--allow-out` plus
   the gateway host and the Tensorlake API host. Create returns after both
   are accepted.
2. **Wait.** A background task polls both VMs until they report `running`.
3. **Workload bundle.** Uploaded to the workload VM: the boundary bootstrap
   with the `TlsTcp` listener, the channel server certificate and key, and
   the launch parameters with `ROLE=workload`. The launcher creates the
   `openshell-workload` network namespace with loopback and a veth link,
   starts `openshell-sandbox launch-capability-free` inside it with a
   resolver file that points at the runtime's DNS relay, and starts the
   `socat` forwarder as user 65533 in the VM's own namespace.
4. **Guard bundle.** Uploaded to the guard VM: the runtime descriptor with
   the `Tcp` transport, the auth bundle, sandbox token, gateway CA, the
   guard's environment and arguments, and a tunnel file with the Tensorlake
   API key and the workload VM's ID. `ROLE=guard`. The launcher starts the
   tunnel as user 65533 and the guard as user 65534. Neither can read the
   other's files. The launcher exits when either process exits.
5. **Run.** The driver reports Running when both launchers started. The
   gateway marks the sandbox ready when the guard connects. The guard's
   first connection to the workload retries until the workload listener is
   up, so start order does not matter.
6. **Poll.** Every few seconds the driver lists Tensorlake sandboxes. If
   either VM is gone, the sandbox is failed. Both launchers are checked;
   an exit is reported as `LauncherExited` with the VM to look at.
7. **Delete.** Both VMs are deleted. The record is forgotten when both are
   gone.
8. **Recover.** After a driver restart, VMs are paired by name:
   `openshell-<id>` and `openshell-<id>-guard`. A pair with one half missing
   is reported as failed.

### Fence evidence

The driver writes the outer fence into the provisioning evidence that the
guard confirms with the workload: the recipe for the workload VM (a private network namespace whose only
link leads to the forwarder, in a VM with `allow_internet_access: false`
and an empty allow list) and the IDs of both VMs. The driver cannot inspect the VM, and Tensorlake does
not return the applied policy in `SandboxInfo`, so this is the requested
recipe, as in the combined topology.

## Trust model

| Attacker | Combined | Split |
|---|---|---|
| Agent with a local kernel exploit | Reads gateway token and credentials | Gets root in a VM that holds only its own channel key. Credentials are in another VM. Egress is still blocked by the Tensorlake fence and still mediated by the guard. |
| Tensorlake project member with `tl sbx exec --user root` | Reads everything in the one VM | Reads everything in the guard VM. Same as today. |
| Anyone with the guard VM's API key | n/a | Can open tunnels to any sandbox in that project. |

The new exposure is the API key in each guard VM. Two mitigations:

- Run the control plane and its sandboxes in a dedicated Tensorlake project,
  so a leaked guard key reaches nothing else. This is already the README's
  advice.
- Tensorlake could issue keys scoped to one target sandbox for tunnels. That
  is the clean fix and needs platform work.

## Cost

Two VMs per sandbox. The guard VM defaults to 1 vCPU and 1 GiB; the guard
uses about 32 MiB and the tunnel 15 MiB. Two VMs boot in parallel, so create
to ready stays a few seconds. All workload egress flows guard VM to proxy to
workload VM. The tunnel measured about 300 MiB/s, far above what an agent
needs, but each guard VM also pays the CPU for TLS and policy on that
traffic. Size `--guard-cpus` up for agents that move bulk data.

## Alternative kept out: guards in the control sandbox

Running every guard inside the control sandbox saves the guard VMs. It also
puts every credential of the project in one kernel, shares 2 vCPUs across
all guards, and drops every session when the control sandbox restarts.
Measured: a 4 GiB control sandbox would fit about 50 guards with `tl`
helper processes or about 100 with the tunnel linked in. The per-sandbox
guard VM was chosen instead because the security property is the point of
this work, and because it matches the Kubernetes driver and the customer
request that motivated it.

## What the end-to-end test found

Three fixes were needed before the first sandbox passed, in this order:

1. **Loopback peers.** The sandbox runtime reset every tunnel connection,
   because it rejects boundary peers that are loopback or share its own
   address, and the Tensorlake tunnel always delivers to `127.0.0.1`. The
   veth forwarder gives the runtime a non-loopback peer.
2. **PID namespace.** With a working channel, every connection was denied
   with `transparent_tcp_identity_unavailable`. Binary identity resolution
   walks a connecting process's ancestors up to its PID namespace init. In
   the VM's root PID namespace that walk reached the root-owned launcher,
   which the runtime cannot read. The runtime now runs as PID 1 of its own
   PID namespace with a fresh procfs, in both topologies.
3. **Transparent TCP capability.** The guard was granted
   `policy-dns-transparent-tcp`, which Docker and Podman use because their
   guard shares the workload's network namespace. This guard never does, so
   the capability is gone, as in the Kubernetes driver.

A policy change reaches the guard on its next settings poll, a few seconds
after `openshell policy update` returns. Pass `--wait` to block until it is
effective.

## Open items

- Which hosts `tl sbx tunnel` contacts, so `--allow-out` on the guard VM
  can be tight. Until measured, leave the guard VM's internet on and rely
  on OpenShell policy, as the combined topology did.
- Tunnel reconnection. If the WebSocket drops, the guard launcher exits and
  the sandbox is reported failed. A reconnecting tunnel would turn that into
  a pause.
- Scoped Tensorlake keys for tunnels.
