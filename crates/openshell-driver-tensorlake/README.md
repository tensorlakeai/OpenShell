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

## Build the image

```shell
docker build -f crates/openshell-driver-tensorlake/image/Dockerfile \
  --build-arg OPENSHELL_TAG=<release tag> -t openshell-tensorlake .
```

Register the image with Tensorlake (`tl sbx image create`). The image must
not contain `/.openshell`. Derive from it to add agent tools.

## Run

The driver and the gateway must run as the same Unix user on the same host.
The gateway needs launch signing (`[openshell.gateway.gateway_jwt]`), and its
gRPC endpoint must be reachable from Tensorlake VMs.

```shell
export TENSORLAKE_API_KEY=...
openshell-driver-tensorlake \
  --bind-socket /run/openshell/tensorlake.sock \
  --grpc-endpoint https://gateway.example.com:8080 \
  --sandbox-image openshell-tensorlake \
  --allow-out api.anthropic.com,github.com

openshell-gateway --compute-driver tensorlake \
  --compute-driver-socket /run/openshell/tensorlake.sock
```

| Flag | Environment | Default |
|---|---|---|
| `--bind-socket` | `OPENSHELL_COMPUTE_DRIVER_SOCKET` | required |
| `--grpc-endpoint` | `OPENSHELL_GRPC_ENDPOINT` | required |
| `--gateway-tls-ca` | `OPENSHELL_TENSORLAKE_GATEWAY_TLS_CA` | none |
| `--tensorlake-api-url` | `TENSORLAKE_API_URL` | `https://sandbox.tensorlake.ai` |
| `--tensorlake-api-key` | `TENSORLAKE_API_KEY` | required |
| `--sandbox-image` | `OPENSHELL_SANDBOX_IMAGE` | required |
| `--cpus` / `--memory-mb` / `--disk-mb` | `OPENSHELL_TENSORLAKE_*` | 2 / 4096 / Tensorlake default |
| `--timeout-secs` | `OPENSHELL_TENSORLAKE_TIMEOUT_SECS` | Tensorlake default |
| `--allow-out` | `OPENSHELL_TENSORLAKE_ALLOW_OUT` | empty (internet on) |
| `--poll-interval-secs` | `OPENSHELL_TENSORLAKE_POLL_INTERVAL_SECS` | 3 |

When `--allow-out` is set, the driver turns off Tensorlake internet access and
adds the gateway host to the list.

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
- the guard can reach the gateway over a long-lived gRPC connection.
