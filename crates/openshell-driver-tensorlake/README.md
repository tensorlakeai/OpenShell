# openshell-driver-tensorlake

Out-of-process compute driver that runs each OpenShell sandbox in its own
[Tensorlake](https://tensorlake.ai) sandbox (a Firecracker microVM).

This is the basic, combined topology: the guard (`openshell-supervisor`) and
the workload runtime (`openshell-sandbox`) run in the same Tensorlake VM. The
guard holds gateway and provider credentials inside that VM. Use it with
non-production credentials only. A later split topology moves the guard into
its own VM.

## How it works

```
openshell CLI ──► gateway ──UDS──► openshell-driver-tensorlake ──HTTPS──► Tensorlake API
                     ▲
                     │ guard dials the gateway (outbound only)
  ┌──────────────────┴───── Tensorlake VM ────────────────────┐
  │ openshell-supervisor (uid 65534, holds credentials)       │
  │        ▲ TLS over /.openshell/channel/sandbox/control.sock│
  │ openshell-sandbox (uid 1000, own network namespace with   │
  │        only loopback) ──► agent processes                 │
  └───────────────────────────────────────────────────────────┘
```

1. `CreateSandbox` calls `POST /sandboxes` with `wait: false` and returns.
2. A background task waits until the VM is `running`.
3. The driver builds a launch bundle (tar) with owners and modes set per file,
   uploads it with the Tensorlake file API, and starts
   `/opt/openshell/bin/openshell-tensorlake-launch` as root with the
   Tensorlake process API.
4. The launcher extracts the bundle, deletes it, starts the workload runtime in
   a loopback-only network namespace, and starts the guard as uid 65534.
5. The driver reports `Ready=True` when the launcher starts. The gateway marks
   the sandbox ready when the guard session connects.

A poll loop reads `GET /sandboxes` because Tensorlake has no watch API.

### Credential layout in the VM

| Path | Owner | Contents |
|---|---|---|
| `/.openshell/channel/sandbox/` | workload | boundary bootstrap and boundary TLS key |
| `/.openshell/supervisor/` (0700) | guard | auth bundle, sandbox JWT, gateway CA, runtime descriptor |
| `/.openshell/supervisor/env`, `args` | root | guard environment and arguments |

The workload identity cannot read the guard's files. Root in the VM can.

### Outer fence

The launcher runs the workload in a new network namespace that has only
loopback, so the workload has no network path except the boundary channel.
The driver reports all four outer-fence guarantees based on this launch
recipe. It cannot inspect the VM to confirm them.

Tensorlake `allow_out` is an optional second wall for the guard's own egress.

## Build the images

`build-images.sh` builds two CAS images on Tensorlake (`--cas`). Tensorlake
runs each build, so local Docker is not needed. CAS images start fast and
run on the Tensorlake CAS dataplane.

| Image | Default name | Contents |
|---|---|---|
| Sandbox | `openshell-tensorlake` | guard, workload runtime, launcher |
| Control | `openshell-control` | gateway, this driver, start script |

```shell
crates/openshell-driver-tensorlake/build-images.sh
```

The control image records the sandbox image name. The driver uses that
sandbox image unless `OPENSHELL_SANDBOX_IMAGE` is set, so the two images
always work as a pair.

The script copies the Rust source of your checkout, with uncommitted
changes, into a temporary build context. Tensorlake uses the Dockerfile
directory as the context, and does not expand variables in `FROM`. So both
Dockerfiles pin the upstream OpenShell images to the upstream commit that
this branch is based on. Change the pin when you rebase.

The sandbox image must not contain `/.openshell`. Derive from it to add agent
tools.

### Publish for all Tensorlake users

Build from a clean checkout, in the project that owns the public images:

```shell
PUBLIC=1 VERSION=0-1-5 crates/openshell-driver-tensorlake/build-images.sh
```

This registers `openshell-tensorlake-0-1-5` and `openshell-control-0-1-5`
with `--public`. Any Tensorlake project can then use them without a build.
Public names must be unique across Tensorlake, and Tensorlake has no image
tags, so put the version in the name. Do not replace a published version;
publish a new one.

The images contain no keys. The control plane creates its signing keys at
first start, and `deploy.sh` uploads the Tensorlake key at deploy time.

## Run

The driver and the gateway must run as the same Unix user on the same host.
The gateway needs launch signing (`[openshell.gateway.gateway_jwt]`), and its
gRPC endpoint must be reachable from Tensorlake VMs.

```shell
export TENSORLAKE_API_KEY=...
openshell-driver-tensorlake \
  --bind-socket /run/openshell/tensorlake.sock \
  --grpc-endpoint https://gateway.example.com:8080 \
  --gateway-tls-ca /etc/openshell/gateway-ca.pem \
  --sandbox-image openshell-tensorlake \
  --allow-out api.anthropic.com,github.com

openshell-gateway --compute-driver tensorlake \
  --compute-driver-socket /run/openshell/tensorlake.sock
```

| Flag | Environment | Default |
|---|---|---|
| `--bind-socket` | `OPENSHELL_COMPUTE_DRIVER_SOCKET` | required |
| `--grpc-endpoint` | `OPENSHELL_GRPC_ENDPOINT` | required |
| `--gateway-tls-ca` | `OPENSHELL_TENSORLAKE_GATEWAY_TLS_CA` | required for an `https` endpoint |
| `--tensorlake-api-url` | `TENSORLAKE_API_URL` | `https://sandbox.tensorlake.ai` |
| `--tensorlake-api-key` | `TENSORLAKE_API_KEY` | required |
| `--sandbox-image` | `OPENSHELL_SANDBOX_IMAGE` | required |
| `--cpus` / `--memory-mb` / `--disk-mb` | `OPENSHELL_TENSORLAKE_*` | 2 / 4096 / Tensorlake default |
| `--timeout-secs` | `OPENSHELL_TENSORLAKE_TIMEOUT_SECS` | 0 (plan maximum) |
| `--allow-out` | `OPENSHELL_TENSORLAKE_ALLOW_OUT` | empty (internet on) |
| `--poll-interval-secs` | `OPENSHELL_TENSORLAKE_POLL_INTERVAL_SECS` | 3 |

The guard trusts only the `--gateway-tls-ca` bundle, not public roots. For a
gateway with a public certificate, pass the root CA that signed it.

The Tensorlake timeout counts idle time, and only incoming traffic resets
it. Guard traffic to the gateway is outgoing, so a short timeout suspends a
working sandbox. Keep the default `0`, which uses the plan maximum.

When `--allow-out` is set, the driver turns off Tensorlake internet access and
adds the gateway host to the list.

## Run the control plane on Tensorlake

`control/` runs the gateway and this driver together in one long-lived
Tensorlake sandbox.

```
 your machine                    Tensorlake
┌──────────────┐  tl tunnel    ┌───────────────────────────────────┐
│ openshell CLI│ ────────────► │ control sandbox                   │
└──────────────┘               │  gateway :8080 (plaintext HTTP/2) │
                               │    ▲ UDS                          │
                               │  driver ──► Tensorlake API        │
                               └────▲──────────────────────────────┘
                                    │ https://8080-<name>.sandbox.tensorlake.ai
                               guards in sandbox VMs (public port, TLS at proxy)
```

The Tensorlake port proxy terminates TLS. Guards trust the public CA bundle
of the control image for that certificate.

1. Build the images (see [Build the images](#build-the-images)), or use
   published ones.

2. Create the sandbox and start the control plane:

   ```shell
   export TENSORLAKE_API_KEY=...
   export OPENSHELL_OIDC_ISSUER=https://issuer.example.com
   export CONTROL_IMAGE=openshell-control-0-1-5   # or your own build
   crates/openshell-driver-tensorlake/control/deploy.sh
   ```

   The script stores settings in `/etc/openshell-control/control.env` (mode
   0600) and starts a managed process with `--restart always`. Run it again
   to change settings. Gateway state stays in `/var/lib/openshell`. Set
   `OPENSHELL_DB_URL` to use an external database.

3. Connect the CLI. With OIDC, use the public URL:

   ```shell
   openshell gateway add https://8080-openshell-control.sandbox.tensorlake.ai \
     --oidc-issuer https://issuer.example.com
   ```

   Without OIDC, use a Tensorlake tunnel:

   ```shell
   tl sbx tunnel openshell-control 8080 --listen-port 18080
   openshell gateway add http://127.0.0.1:18080 --name tensorlake
   ```

Guards cannot hold a Tensorlake key, so port 8080 accepts anonymous
requests. The gateway must authenticate users itself. The start script
refuses to run without `OPENSHELL_OIDC_ISSUER`, unless you set
`OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1`. With that setting, anyone with
the URL controls the gateway. Use it only for a short test with
non-production credentials.

The control sandbox holds the Tensorlake API key. Do not run other workloads
in it.

## Limits

- Stop and start return `Unimplemented`. Delete the sandbox instead.
- Workload identity accepts numeric UID and GID only. The default is 1000:1000.
- No GPU, `driver_config`, `platform_config`, or `agent_socket_path`.
- The driver keeps launch state in memory. After a driver restart, running
  sandboxes are recovered from Tensorlake names (`openshell-<id>`). Sandboxes
  that had not launched are reported as failed.
- A user with Tensorlake account access can run `tl sbx exec --user root` and
  bypass OpenShell.

## Not yet verified on Tensorlake

The unit tests cover bundle layout, request mapping, and status mapping. An
end-to-end run on Tensorlake still needs to confirm that:

- the guest kernel enables Landlock and seccomp, and allows `unshare --net`;
- the file API writes as root, and the process API runs the launcher as root;
- the guard can reach the gateway over a long-lived gRPC connection;
- the port proxy sends gRPC to a plaintext HTTP/2 backend, and keeps
  long-lived streams open;
- CAS builds accept the multi-stage Dockerfiles and `--public`;
- the CAS dataplane enables Landlock, seccomp, and `unshare --net`, and
  supports the file and process APIs, exposed ports, and long-lived managed
  processes.
