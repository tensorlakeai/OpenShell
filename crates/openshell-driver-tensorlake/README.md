# openshell-driver-tensorlake

This driver runs each OpenShell sandbox inside its own
[Tensorlake](https://tensorlake.ai) sandbox. A Tensorlake sandbox is a small
virtual machine (VM). The driver is a separate program that runs next to the
OpenShell gateway and talks to Tensorlake for it.

## Two kinds of sandbox

Two different things are called "sandbox" on this page:

- A **Tensorlake sandbox** is the VM. Tensorlake creates it, runs it, and
  deletes it.
- An **OpenShell sandbox** is what runs inside that VM: a guard, a workload,
  and the policy between them.

The guard (`openshell-supervisor`) holds the credentials and keeps a
connection to the gateway. The workload (`openshell-sandbox`) runs the
agent's commands. The workload has no network of its own. Every connection
it makes goes through the guard, and the guard applies the policy.

By default the guard and the workload run in two different VMs (the
"split" topology). The workload VM has no outbound network at all, and the
guard VM reaches it through a Tensorlake tunnel. Credentials never enter
the VM that runs the agent. The design and its measurements are in
[docs/split-topology.md](docs/split-topology.md).

With `--topology combined`, both run in one VM. That is cheaper, but a root
user in that VM can read the guard's credentials. Use it with test
credentials only.

The user-facing page is
[Tensorlake Runtime](../../docs/how-it-works/sandboxes/tensorlake.mdx).

## How it works

```
openshell CLI ──► gateway ──Unix socket──► openshell-driver-tensorlake ──HTTPS──► Tensorlake API
                     ▲
                     │ the guard calls the gateway (outbound only)
  ┌──────────────────┴── guard VM: openshell-<id>-guard ──┐   ┌── workload VM: openshell-<id> ─────────┐
  │ guard: openshell-supervisor (uid 65534, credentials)  │   │ 127.0.0.1:8443 socat forwarder (65533)  │
  │   │ OpenShell TLS, end to end                         │   │   │ veth 169.254.77.1 -> .2:8444        │
  │   ▼                                                   │   │ ┌─▼─ network namespace ───────────────┐ │
  │ 127.0.0.1:18443 ◄── tl sbx tunnel (uid 65533, TL key) ┼───┼─► openshell-sandbox (uid 1000) :8444  │ │
  │ network: internet, or --allow-out                     │   │ │   └─► agent processes, loopback only │ │
  └───────────────────────────────────────────────────────┘   │ └─────────────────────────────────────┘ │
                                                              │ network: no outbound path at all        │
                                                              └─────────────────────────────────────────┘
```

When the gateway asks for a new sandbox:

1. The driver asks Tensorlake to create two VMs and returns at once. The
   workload VM uses the sandbox image and a network policy with internet
   access off and an empty allow list. The guard VM uses the guard image
   (by default the same image) with 1 vCPU and 1 GiB.
2. In the background, the driver waits until both VMs report `running`.
3. The driver packs two archives. Each file has its owner and permissions
   set. The workload archive holds the channel bootstrap and the channel
   server key. The guard archive holds the gateway credentials, the
   runtime descriptor that points at `127.0.0.1:18443`, and a Tensorlake
   key for the tunnel. The driver uploads each archive with the Tensorlake
   file API and starts `/opt/openshell/bin/openshell-tensorlake-launch` as
   root in each VM with the Tensorlake process API.
4. In the workload VM, the launch script creates a network namespace with
   loopback and one veth link, starts `openshell-sandbox` inside it, where
   it listens on port 8444, and starts a `socat` forwarder as user 65533
   that accepts the tunnel on `127.0.0.1:8443` and connects over the veth
   link. The forwarder exists because the runtime rejects loopback peers
   and the Tensorlake tunnel always delivers to loopback. In the guard VM,
   the script starts `tl sbx tunnel` as user 65533, which listens on
   `127.0.0.1:18443` and relays bytes to port 8443 of the workload VM, and
   the guard as user 65534. Neither user can read the other's files. The
   guard's TLS handshake runs end to end to the workload; the tunnel and
   the forwarder do not inspect the bytes. In both topologies the workload
   runtime is PID 1 of its own PID namespace, so network policy can resolve
   which binary opened a connection.
5. The driver reports the sandbox as ready when both launch scripts
   started. The gateway marks the sandbox ready when the guard connects to
   it. During steps 1 to 5 the driver publishes progress events, so the
   CLI shows the VM request, the VM start, the bundle upload, and the wait
   for the guard instead of a silent spinner.
6. Every few seconds, the driver checks that both launch scripts are still
   running and both VMs still exist. If a script stopped, the driver marks
   the sandbox as failed with reason `LauncherExited`, the role, and the
   exit status. If a VM is gone, the reason is `TensorlakeTerminated`.
   Tensorlake does not restart the script.

Tensorlake cannot push changes to the driver. So the driver asks Tensorlake
for the list of sandboxes every few seconds.

With `--topology combined`, step 1 creates one VM, step 3 packs one
archive, and the launch script starts the workload in a private network
namespace that has only loopback, then the guard, in that VM.

### Where the files live in the VMs

| Path | VM | Owner | Contents |
|---|---|---|---|
| `/.openshell/channel/sandbox/` (mode 0711) | workload | workload | bootstrap file, TLS certificate, and TLS key for the channel to the guard |
| `/.openshell/supervisor/` (mode 0700) | guard | guard | auth bundle, sandbox token, gateway CA, runtime descriptor |
| `/.openshell/supervisor/env`, `args` | guard | root | the guard's environment and arguments |
| `/.openshell/tunnel/` (mode 0700) | guard | tunnel | `env` with the Tensorlake key and the sandbox API URL |
| `/.openshell/tunnel/args` | guard | root | the `tl sbx tunnel` arguments |
| `/.openshell/launch/params` | both | root | the role and the identities for the launch script |
| `/.openshell/launch/resolv.conf` | workload | root | written by the launch script; points the workload at the runtime's DNS relay |

In the combined topology all of these except the tunnel files live in the
one VM.

### The outer fence

The workload runs in a network namespace whose only link leads to the
forwarder, and the workload VM gets a Tensorlake network policy that turns
off every outbound connection, DNS included. The only way out is the
channel to the guard, which arrives through the tunnel. The driver writes this recipe into
the sandbox's config, so the gateway knows which fence this sandbox got.
Tensorlake does not report the applied policy, so the driver cannot confirm
it from outside.

`--allow-out` limits where the guard VM itself can connect. See
[Run the driver](#run-the-driver).

## Build the images

`build-images.sh` builds two images on Tensorlake with the `--cas` flag.
Tensorlake runs each build, so you do not need Docker on your machine.
Images built this way start fast.

| Image | Default name | Contents |
|---|---|---|
| Sandbox | `openshell-tensorlake` | guard, workload, Tensorlake CLI, socat, launch script |
| Control | `openshell-control` | gateway, this driver, start script |

The sandbox image serves both VMs of a sandbox. The workload VM runs the
workload runtime from it, the guard VM runs the tunnel and the guard from
it. Set `--guard-image` to use a different image for guard VMs.

```shell
crates/openshell-driver-tensorlake/build-images.sh
```

The control image records the name of the sandbox image. The driver uses
that image unless you set `OPENSHELL_SANDBOX_IMAGE`. So the two images
always work as a pair.

The script copies the Rust source of your checkout, with uncommitted
changes, into a temporary build folder. Tensorlake uses the folder of the
Dockerfile as the build context, and it does not expand variables in
`FROM`. So both Dockerfiles pin the upstream OpenShell images to the
upstream commit that this branch is based on. Change the pin when you
rebase.

Keep one pair of image names for development, for example
`openshell-tensorlake-test` and `openshell-control-test`. A build under an
existing name replaces that image, and new sandboxes use the new content.
Delete a name you no longer need with `tl sbx image rm <name>`.

### Rebuild after a change

If you changed the launch script or `image/Dockerfile`, rebuild only the
sandbox image. The driver reads the image name on every sandbox create, so
you do not need to restart the control plane:

```shell
tl sbx image create crates/openshell-driver-tensorlake/image/Dockerfile \
  --registered-name openshell-tensorlake-test --cas
```

If you changed the driver, rebuild both images and recreate the control
sandbox. Recreating it discards the gateway state and changes the sandbox
ID, so add the gateway to the CLI again afterwards:

```shell
SANDBOX_IMAGE=openshell-tensorlake-test CONTROL_IMAGE=openshell-control-test \
  crates/openshell-driver-tensorlake/build-images.sh
RECREATE=1 CONTROL_IMAGE=openshell-control-test \
  crates/openshell-driver-tensorlake/control/deploy.sh
```

The sandbox image must not contain `/.openshell`. To add agent tools, build
a new image on top of it.

### Publish for all Tensorlake users

Build from a clean checkout, in the project that owns the public images:

```shell
PUBLIC=1 VERSION=0-1-5 crates/openshell-driver-tensorlake/build-images.sh
```

This registers `openshell-tensorlake-0-1-5` and `openshell-control-0-1-5`
as public images. Any Tensorlake project can use them without a build.
Public names must be unique across all of Tensorlake, and Tensorlake has no
image tags, so the version is part of the name. Do not replace a published
version. Publish a new one.

The images contain no keys. The control plane creates its signing keys at
first start, and `deploy.sh` uploads the Tensorlake key at deploy time.

The same build runs in CI from the `Tensorlake Images` workflow
(`.github/workflows/tensorlake-images.yml`). Start it by hand with a version
and the `public` flag. It needs the `TENSORLAKE_API_KEY` repository secret for
the project that owns the public images.

## Run the driver

The driver and the gateway must run as the same Unix user on the same host.
The gateway needs launch signing (`[openshell.gateway.gateway_jwt]`), and
Tensorlake VMs must be able to reach its address.

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
| `--topology` | `OPENSHELL_TENSORLAKE_TOPOLOGY` | `split`; `combined` shares one VM |
| `--guard-image` | `OPENSHELL_TENSORLAKE_GUARD_IMAGE` | the sandbox image |
| `--guard-cpus` / `--guard-memory-mb` | `OPENSHELL_TENSORLAKE_GUARD_*` | 1 / 1024 |
| `--cpus` / `--memory-mb` / `--disk-mb` | `OPENSHELL_TENSORLAKE_*` | 2 / 4096 / Tensorlake default |
| `--timeout-secs` | `OPENSHELL_TENSORLAKE_TIMEOUT_SECS` | 0 (plan maximum) |
| `--allow-out` | `OPENSHELL_TENSORLAKE_ALLOW_OUT` | empty (internet on for the guard VM) |
| `--poll-interval-secs` | `OPENSHELL_TENSORLAKE_POLL_INTERVAL_SECS` | 3 |
| `--state-dir` | `OPENSHELL_TENSORLAKE_STATE_DIR` | `target/openshell-tensorlake-driver`; the control plane uses `/var/lib/openshell/tensorlake` |
| `--log-level` | `OPENSHELL_LOG_LEVEL` | `info`; also the default level for guards |
| `--otlp-endpoint` | `OPENSHELL_OTLP_ENDPOINT` | none |
| `--gateway-name` | `OPENSHELL_GATEWAY_NAME` | none; used in traces |
| `--admission-config-json` | `OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON` | `{}`; see the gateway's driver admission docs |

The guard trusts only the certificates in `--gateway-tls-ca`. It does not
trust public root certificates. If your gateway has a public certificate,
pass the root certificate that signed it.

The Tensorlake timeout counts idle time, and only incoming traffic resets
it. The guard's traffic to the gateway is outgoing. So a short timeout can
pause a sandbox that is still working. Keep the default `0`, which uses the
longest timeout your plan allows.

When you set `--allow-out`, the driver turns off internet access for the
VM that runs the guard and allows only the hosts you listed, the gateway
host, and in the split topology the Tensorlake host that the tunnel talks
to. The workload VM in the split topology always has internet access off
with an empty allow list.

## Run the control plane on Tensorlake

The control plane is the gateway and this driver. The `control/` folder
runs both in one long-lived Tensorlake sandbox.

```
 your machine                    Tensorlake
┌──────────────┐  login + TLS  ┌───────────────────────────────────┐
│ openshell CLI│ ────────────► │ control sandbox                   │
└──────────────┘               │  gateway :8080 (plain HTTP/2)     │
                               │    ▲ Unix socket                  │
                               │  driver ──► Tensorlake API        │
                               └────▲──────────────────────────────┘
                                    │ https://8080-<sandbox_id>.sandbox.tensorlake.ai
                               guards in sandbox VMs (public port, TLS at the proxy)
```

The Tensorlake port proxy handles TLS. Guards trust the public certificate
bundle in the control image for that connection.

1. Build the images (see [Build the images](#build-the-images)), or use
   the published ones.

2. Create the sandbox and start the control plane. Set the login service
   so that the gateway checks who is calling:

   ```shell
   export TENSORLAKE_API_KEY=...
   export CONTROL_IMAGE=openshell-control-0-1-5   # or your own build
   export OPENSHELL_OIDC_ISSUER=https://idp.example.com/realms/openshell
   export OPENSHELL_OIDC_AUDIENCE=openshell-cli
   crates/openshell-driver-tensorlake/control/deploy.sh
   ```

   The script copies these variables into
   `/etc/openshell-control/control.env` (mode 0600): every
   `OPENSHELL_OIDC_*` and `OPENSHELL_TENSORLAKE_*` variable,
   `OPENSHELL_DB_URL`, `OPENSHELL_SANDBOX_IMAGE`, `OPENSHELL_GATEWAY_NAME`,
   and `OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED`. So every driver setting
   from the table above can be set through its environment variable. Then
   it starts the control plane as a managed process that restarts on
   failure. Run the script again to change settings. Gateway state stays
   in `/var/lib/openshell`. Set `OPENSHELL_DB_URL` to use an external
   database. `CONTROL_NAME` changes the sandbox name from
   `openshell-control`, and `CONTROL_TIMEOUT_SECS` its idle timeout.

3. Connect the CLI with the public URL. The CLI opens a browser so you can
   log in once. It stores the login and renews it on its own:

   ```shell
   openshell gateway add https://8080-<sandbox_id>.sandbox.tensorlake.ai \
     --oidc-issuer https://idp.example.com/realms/openshell \
     --oidc-client-id openshell-cli \
     --oidc-audience openshell-cli
   ```

   On a machine without a browser, set `OPENSHELL_NO_BROWSER=1` first. The
   CLI then prints a code that you enter on another device. For scripts
   and CI, set `OPENSHELL_OIDC_CLIENT_SECRET` and the CLI logs in without a
   browser.

### Why every user must log in

A public gateway is inherent to a remote sandbox provider. Guards in
sandbox VMs must dial the gateway, they cannot hold a Tensorlake key, and
Tensorlake has no inbound tunnel to your machine. So port 8080 must accept
calls from anyone, and the URL is public in every mode. You cannot hide it.
The decision and the fallback for deployments without an identity provider
are in [docs/control-plane-auth.md](docs/control-plane-auth.md).

The login is what makes the public URL safe. The gateway checks every call
for a valid login token from your login service. It checks the token's
signature, issuer, audience, and roles. A call without a valid token gets
`Unauthenticated`. The guards' URL uses the sandbox ID because the proxy
resolves sandbox names only for calls that carry a Tensorlake key.
`deploy.sh` prints the ID.

The full list of gateway settings is in
[Authentication](../../docs/how-it-works/gateways/authentication.mdx).

### What your login service needs

OpenShell uses the OpenID Connect standard, so most company login services
work. Yours must offer:

- Roles inside the token. Keycloak, Okta, and Auth0 can do this. A plain
  Google sign-in cannot by itself.
- A public client for the CLI, with PKCE turned on and a redirect to
  `http://127.0.0.1` on any port. Its client ID goes to
  `--oidc-client-id`.
- Tokens whose `aud` claim matches `OPENSHELL_OIDC_AUDIENCE`.
- Two roles, `openshell-admin` and `openshell-user` by default, given to
  the people who need them. Change the names with
  `OPENSHELL_OIDC_ADMIN_ROLE` and `OPENSHELL_OIDC_USER_ROLE`. Change where
  the gateway looks for roles with `OPENSHELL_OIDC_ROLES_CLAIM` (default
  `realm_access.roles`).

### Test without a login service

The start script refuses to run without `OPENSHELL_OIDC_ISSUER`, unless
you set `OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1`. With that setting,
anyone who has the URL controls the gateway. Use it only for a short test
with test credentials. Reach the gateway through a Tensorlake tunnel, which
needs your Tensorlake key, instead of the public URL:

```shell
tl sbx tunnel openshell-control 8080 --listen-port 18080
openshell gateway add http://127.0.0.1:18080 --name tensorlake
```

The control sandbox holds the Tensorlake API key. Do not run other
workloads in it.

## Limits

- Stop and start are not supported. Delete the sandbox instead.
- The workload identity accepts a numeric user ID and group ID only. The
  default is 1000:1000.
- No GPU, `driver_config`, `platform_config`, or `agent_socket_path`.
- The driver keeps launch state in memory and a copy of each sandbox
  request, without its credentials, under `--state-dir`. After a driver
  restart, it finds running sandboxes again by their Tensorlake names
  (`openshell-<id>` and `openshell-<id>-guard`) and reports them under the
  names in the saved requests. Keep the state directory with the gateway
  database: a recovered sandbox with no saved request has no name, and the
  gateway then rejects every status update for it until it is deleted. A
  pair with a VM still pending is reported as
  failed with reason `DriverRestarted`. A pair with one VM missing is
  reported as failed with reason `PairIncomplete`. A pair whose VMs were
  running but whose launch scripts had not started yet is recovered as
  running, and the gateway then waits for a guard that never connects.
  Delete and recreate such a sandbox.
- Anyone with access to your Tensorlake project can run
  `tl sbx exec --user root` in the guard VM and read its credentials, or in
  the workload VM and get around OpenShell policy. Keep the control plane
  in a project of its own.
- Each guard VM holds a Tensorlake API key for its tunnel. That key can
  open tunnels to any sandbox in the project.
- The split topology uses two Tensorlake VMs per sandbox.

## Testing

There are three layers. The first runs on your machine with no Tensorlake
account. The other two create real VMs and need a Tensorlake key.

### Unit tests and lint

```shell
cargo test -p openshell-driver-tensorlake
cargo clippy -p openshell-driver-tensorlake --all-targets -- -D warnings
bash -n crates/openshell-driver-tensorlake/image/openshell-tensorlake-launch.sh \
  crates/openshell-driver-tensorlake/control/*.sh \
  crates/openshell-driver-tensorlake/build-images.sh
```

The unit tests cover:

- Config validation, the redacted debug output, and the hosts that
  `--allow-out` adds for the guard VM.
- The archive layout: every file's path, owner, and mode, and that the
  split workload bundle holds no gateway token, no Tensorlake key, and no
  guard file.
- Request mapping: CPU and memory quantities, Tensorlake names, and the
  requests the driver rejects.
- Pairing of workload and guard VMs by name, the phase after a restart,
  and the phase after each poll observation.
- Launcher failure messages and the progress events the CLI shows.

They do not run the launch script, the Tensorlake API, the tunnel, or
policy enforcement. `mise run lint` and `mise run test:rust` run the same
checks for the whole workspace.

### Conformance suite

The CLI conformance suite in `tests/suites/conformance/cli` drives a
gateway through the `openshell` binary. It needs `OPENSHELL_BIN` and a
registered gateway, selected with `OPENSHELL_GATEWAY`. Its test files:

| Test file | Scenarios | Run on Tensorlake |
|---|---|---|
| `smoke` | create, exec, delete | yes |
| `file_transfer` | round trip, git filtering, path safety | yes |
| `policy_advisor` | policy-local, mechanistic-proposal, new-hostname-proposal | yes |
| `lifecycle` | stop, start, delete while stopped | no; the driver does not support stop and start |

The `mechanistic-proposal` scenario opens a TCP connection to `1.1.1.1:443`
from Bash inside the sandbox and expects `DENIED`. It then checks that the
proposal names the Bash binary, so it also proves binary identity through
the tunnel.

### Verified on Tensorlake

End-to-end runs on Tensorlake Cloud confirmed:

- Split topology (2026-10-20): create to Ready in about 4 seconds with two
  VMs; `exec` runs as the workload user; the workload VM holds no gateway
  token or Tensorlake key and only the runtime, the forwarder, and the
  launch script run in it; DNS resolves through the guard; the default
  policy denies egress; after `openshell policy update` allows one host
  for `/usr/bin/curl`, that fetch returns 200, another host and the same
  binary at another path stay denied, and the guard logs the binary
  identity; 20 sequential fetches through guard and tunnel took 3.7
  seconds; delete removes both VMs; a guard launcher exit is reported as
  `LauncherExited`.
- Both topologies: the VM kernel supports Landlock, seccomp, network, mount,
  and PID namespaces; the file API writes as root and the process API runs
  the launch script as root; the guard reaches the gateway through the port
  proxy; image builds accept the multi-stage Dockerfiles and the `--public`
  flag.

Three findings shaped the split topology. The sandbox runtime rejects
boundary peers on loopback, so the tunnel needs the veth forwarder. Binary
identity resolution walks a process's ancestors to its PID namespace init,
so the runtime must run in its own PID namespace, which also fixes policy
enforcement in the combined topology. And the transparent TCP capability
is for guards that share the workload's network namespace, so this driver
no longer grants it.

### Still to confirm

- The conformance scenarios `smoke`, `file_transfer`, and `policy_advisor`
  pass.
- The port proxy keeps a long-lived connection open, and the idle timeout
  does not pause a sandbox whose only traffic is the guard's outgoing
  session.
- A driver restart recovers running sandbox pairs.
- The combined topology end to end after the PID namespace change.
- Which Tensorlake hosts the tunnel needs, so `--allow-out` can be tight on
  the guard VM.

### Runbook

This runbook covers the items in [Still to confirm](#still-to-confirm).
Remove the steps that pass once they are recorded above.

Use two shells. Shell A keeps the tunnel open. Shell B runs everything
else. Some steps run `deploy.sh` again, so keep the exported variables in
shell B for the whole run. The steps assume the control sandbox is
`openshell-control`, the CLI gateway is named `tensorlake`, and the tunnel
listens on port 18080.

Each OpenShell sandbox has two Tensorlake VMs: `openshell-<id>` for the
workload and `openshell-<id>-guard` for the guard, where `<id>` is the
OpenShell sandbox ID. `tl sbx ls` shows both names. The commands below
call them `$WORKLOAD_VM` and `$GUARD_VM`.

#### 0. Build and deploy the control plane

Build the two images. Skip this if you already have a pair you trust:

```shell
cd ~/Documents/work/OpenShell
SANDBOX_IMAGE=openshell-tensorlake-test CONTROL_IMAGE=openshell-control-test \
  crates/openshell-driver-tensorlake/build-images.sh
```

Export the deploy settings. For a test gateway without a login service:

```shell
export TENSORLAKE_API_KEY=your_api_key
export CONTROL_IMAGE=openshell-control-test
export OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1
```

With a login service, replace the last line with
`export OPENSHELL_OIDC_ISSUER=https://issuer.example.com`.

Deploy. Use `RECREATE=1` if a control sandbox already exists and you want
the new image:

```shell
RECREATE=1 crates/openshell-driver-tensorlake/control/deploy.sh
```

The script prints the public URL and the sandbox ID. Check that the
control plane came up:

```shell
tl sbx logs openshell-control | tail -20
```

Expected: a line `control plane started at https://8080-...` and no
`error` lines.

Open the tunnel in shell A:

```shell
tl sbx tunnel openshell-control 8080 --listen-port 18080
```

Back in shell B, build the CLI, register the gateway, and check it:

```shell
cargo build -p openshell-cli
export OPENSHELL_BIN=$PWD/target/debug/openshell
$OPENSHELL_BIN gateway add http://127.0.0.1:18080 --name tensorlake
export OPENSHELL_GATEWAY=tensorlake
$OPENSHELL_BIN gateway list
$OPENSHELL_BIN status
```

If the gateway name is already registered from an earlier run, the add
fails. Remove the old entry first with
`$OPENSHELL_BIN gateway remove tensorlake`, or pick a new name and export
that instead. Both commands must succeed before you go on.

Create one sandbox and watch the launch steps:

```shell
$OPENSHELL_BIN sandbox create --name hello -- echo hello
```

Expected: the CLI shows the VM request, the VM start, the bundle upload,
and the wait for the guard, then prints `hello`. `tl sbx ls` shows two
new VMs. Delete it with `$OPENSHELL_BIN sandbox delete hello`.

#### 1. Conformance suite

Allow up to an hour. Each scenario creates its own sandboxes and deletes
them. Run the three test files that fit this driver and save the output:

```shell
cargo test --locked \
  --manifest-path tests/suites/conformance/Cargo.toml \
  --package openshell-test-conformance-cli \
  --test smoke --test file_transfer --test policy_advisor \
  --no-fail-fast -- --test-threads=1 --nocapture 2>&1 \
  | tee ~/tensorlake-conformance.log
```

Do not run `--test lifecycle`. It stops and starts a sandbox, which this
driver rejects by design.

In the log, the line to look for is `DENIED` from the `denied-tcp-open`
step of `mechanistic-proposal`.

#### 2. Launch script death

The driver watches the launch script in both VMs. Test both roles.

```shell
$OPENSHELL_BIN sandbox create --name kill-test --detach
$OPENSHELL_BIN sandbox get kill-test
```

Repeat the get until it shows Ready. Then find the two VM names and
confirm that the launch script runs in each:

```shell
tl sbx ls | grep openshell-
export WORKLOAD_VM=openshell-<id>
export GUARD_VM=openshell-<id>-guard
tl sbx ps $WORKLOAD_VM
tl sbx ps $GUARD_VM
```

Kill the script in the guard VM first:

```shell
tl sbx exec --user root $GUARD_VM pkill -f openshell-tensorlake-launch
sleep 10
$OPENSHELL_BIN sandbox get kill-test
```

Expected: Ready is False, the reason is `LauncherExited`, and the message
names the guard launcher and the guard VM. Delete the sandbox, create it
again, and repeat with `$WORKLOAD_VM`. Expected: the same reason, with
the message naming the workload launcher. Keep the second sandbox for
step 7.

#### 3. Driver restart

Create one sandbox and wait for Ready. Then create a second and do not
wait:

```shell
$OPENSHELL_BIN sandbox create --name restart-a --detach
$OPENSHELL_BIN sandbox get restart-a
$OPENSHELL_BIN sandbox create --name restart-b --detach
```

Restart the control plane by running the deploy script again with the
same variables. Without `RECREATE=1`, it keeps the sandbox and the gateway
state and only restarts the process:

```shell
crates/openshell-driver-tensorlake/control/deploy.sh
```

If the tunnel in shell A dropped, start it again. Then check:

```shell
tl sbx logs openshell-control | grep -i recovered
$OPENSHELL_BIN sandbox list
$OPENSHELL_BIN sandbox exec --name restart-a --no-tty -- echo hello
$OPENSHELL_BIN sandbox get restart-b
```

Expected: a `Recovered Tensorlake sandboxes` log line, `restart-a` still
Ready with the exec printing `hello`, and `restart-b` Failed with reason
`DriverRestarted` if a VM was still pending at restart time. If both VMs
of `restart-b` were already running, the driver recovers it as running.
It then becomes Ready only if its launch scripts had started before the
restart. Record which case you saw. The gateway log must show no
`name cannot be changed after creation` error: the driver reports each
recovered sandbox under the name in its saved request.

#### 4. Idle timeout

Read the timeout that the control sandbox got from the default of `0`:

```shell
tl sbx describe openshell-control
```

Note the timeout value. That is the plan maximum. Then run a bounded test
with a 3 minute timeout. `deploy.sh` forwards every `OPENSHELL_TENSORLAKE_*`
variable to the driver:

```shell
OPENSHELL_TENSORLAKE_TIMEOUT_SECS=180 crates/openshell-driver-tensorlake/control/deploy.sh
$OPENSHELL_BIN sandbox create --name idle-test --detach
$OPENSHELL_BIN sandbox get idle-test
```

Wait for Ready, then leave it alone for 5 minutes and check:

```shell
sleep 300
$OPENSHELL_BIN sandbox get idle-test
tl sbx ls | grep openshell-
```

Expected: the sandbox shows Suspended, because the guard's outgoing
session did not reset the idle clock. That confirms the plan maximum must
be long enough for a user session. Afterwards, deploy again without the
variable to restore the default:

```shell
crates/openshell-driver-tensorlake/control/deploy.sh
```

#### 5. Combined topology

```shell
OPENSHELL_TENSORLAKE_TOPOLOGY=combined crates/openshell-driver-tensorlake/control/deploy.sh
$OPENSHELL_BIN sandbox create --name combined-test --detach
$OPENSHELL_BIN sandbox get combined-test
```

Wait for Ready. Then check that one VM serves the sandbox, that the
workload runs as the workload user, and that policy still denies egress
with a binary identity:

```shell
tl sbx ls | grep openshell-
$OPENSHELL_BIN sandbox exec --name combined-test --no-tty -- id -u
$OPENSHELL_BIN sandbox exec --name combined-test --no-tty -- \
  bash -c 'if exec 3<>/dev/tcp/1.1.1.1/443; then echo ALLOWED; else echo DENIED; fi'
tl sbx logs openshell-<id> | grep -i "denied\|identity" | tail -5
```

Expected: one VM named `openshell-<id>` and no `-guard` VM, `1000`,
`DENIED`, and a guard log line that names the Bash binary by path rather
than `transparent_tcp_identity_unavailable`. Delete the sandbox and
deploy again without the variable.

#### 6. Tunnel hosts and `--allow-out`

Find which hosts the tunnel in a guard VM talks to. Use a Ready sandbox,
for example `restart-a`:

```shell
tl sbx exec --user root $GUARD_VM ss -tnp
tl sbx exec --user root $GUARD_VM cat /etc/hosts
```

Note the peer addresses of the `tl` process and resolve them. Then
deploy with an allow list and check that a sandbox still becomes Ready:

```shell
OPENSHELL_TENSORLAKE_ALLOW_OUT=api.anthropic.com crates/openshell-driver-tensorlake/control/deploy.sh
$OPENSHELL_BIN sandbox create --name allow-test -- echo hello
```

Expected: `hello`. If the sandbox never becomes Ready, read
`tl sbx logs $GUARD_VM`. A tunnel that cannot reach its host exits, and
the driver reports `LauncherExited` for the guard. Make
`effective_allow_out` in `src/config.rs` add the hosts you found, then
deploy again without the variable.

#### 7. Delete

```shell
$OPENSHELL_BIN sandbox delete kill-test restart-a restart-b idle-test allow-test
sleep 10
$OPENSHELL_BIN sandbox list
tl sbx ls
```

Expected: no sandboxes in the first list, and no names starting with
`openshell-` in the second, apart from `openshell-control`.

#### What to collect

- The conformance log file.
- The output of each `get`, `list`, and `ps` command.
- The last 50 lines of `tl sbx logs openshell-control`.
- The tunnel peer addresses from step 6.

## After the runbook passes

This section is a temporary plan. Remove it once the work is done.

### 1. Case study: run Claude Code on Tensorlake through OpenShell

The hello test proves the plumbing. The case study proves the three things
a real user needs: the guard injects a real provider credential, the policy
blocks a real agent's stray traffic, and a custom agent image works. The
`--from` flag on sandbox create maps to a Tensorlake image name, so the
driver already supports this.

Build a derived image on Tensorlake. Its Dockerfile starts
`FROM openshell-tensorlake-test` and installs Claude Code. Register it:

```shell
tl sbx image create path/to/Dockerfile \
  --registered-name openshell-tensorlake-claude --cas
```

Import the provider profile and create the provider. The API key stays in
the gateway and never enters the workload:

```shell
openshell profile import --file providers/claude-code.yaml
export ANTHROPIC_API_KEY=your_api_key
openshell provider create --name anthropic --type anthropic --from-existing
```

Run the agent:

```shell
openshell sandbox create --name demo \
  --from openshell-tensorlake-claude \
  --provider anthropic \
  -- claude -p "write a hello world in python and run it"
```

Then show one denied request, for example the agent reaching PyPI. Approve
the drafted policy rule and run again.

The output is a tutorial page under `docs/tutorials/`, modeled on
"Run your first agent", plus a section in the design doc. The first run
will hit denials for npm or PyPI. Budget time to settle the policy before
you record the final run.

### 2. Publish the first public image pair

Run the `Tensorlake Images` workflow with a version and `public: true`, or
run `PUBLIC=1 VERSION=<version> build-images.sh` from a clean checkout in
the project that owns the public images. Do this only after the runbook
passes, because a published version cannot be replaced.

### 3. Later

- Stop and start.
- GPU.
- Tensorlake keys scoped to one tunnel target, so a guard VM's key cannot
  reach other sandboxes.
- A tunnel that reconnects after a dropped WebSocket, so the sandbox pauses
  instead of failing.
