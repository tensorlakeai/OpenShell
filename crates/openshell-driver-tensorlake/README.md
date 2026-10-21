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

The guard and the workload run in two different VMs. The workload VM has
no outbound network at all, and the guard VM reaches it through a
Tensorlake tunnel. Credentials never enter the VM that runs the agent. See
[Design decisions](#design-decisions).

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
   the forwarder do not inspect the bytes. The workload runtime is PID 1
   of its own PID namespace, so network policy can resolve which binary
   opened a connection.
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

### Stop and start

`openshell sandbox stop` ends the OpenShell processes in both VMs and
suspends the VMs. Tensorlake keeps their disks, so the workspace and the
agent's files stay. To end the processes, the driver runs the launch script
with `--stop` as root: the script signals the previous launch script, kills
what is left in the workload namespace and under the unprivileged
identities, removes the namespace, and removes `/.openshell` and the
runtime directories. Then the driver suspends the VM.

`openshell sandbox start` resumes the VMs and runs steps 2 to 5 again with
fresh credentials from the gateway. The guard gets a new session and the
workload a new channel key; the old ones are gone with `/.openshell`. The
main process starts again from the beginning, as in the other drivers.

A failed sandbox cannot be started. Delete and recreate it. The one
exception: when both VMs of a failed sandbox are suspended, the sandbox
shows as stopped, and start launches it again.

A guard VM suspended by hand, with `tl sbx suspend`, also shows the sandbox
as stopped, and `openshell sandbox start` brings it back. Do not suspend
the workload VM by hand: Tensorlake does not suspend it while the runtime
holds its network namespace, and the command waits for nothing (seen on
2026-10-21). A VM resumed by hand runs nothing of OpenShell; the sandbox
stays stopped until you start it.

### Where the files live in the VMs

| Path | VM | Owner | Contents |
|---|---|---|---|
| `/.openshell/channel/sandbox/` (mode 0711) | workload | workload | bootstrap file, TLS certificate, and TLS key for the channel to the guard |
| `/.openshell/supervisor/` (mode 0700) | guard | guard | auth bundle, sandbox token, gateway CA, runtime descriptor |
| `/.openshell/supervisor/env`, `args` | guard | root | the guard's environment and arguments |
| `/.openshell/tunnel/` (mode 0700) | guard | tunnel | `env` with the Tensorlake key, the sandbox API URL, and the platform API URL |
| `/.openshell/tunnel/args` | guard | root | the `tl sbx tunnel` arguments |
| `/.openshell/launch/params` | both | root | the role and the identities for the launch script |
| `/.openshell/launch/resolv.conf` | workload | root | written by the launch script; points the workload at the runtime's DNS relay |
| `/.openshell/launch/pid`, `uids` | both | root | written by the launch script; the PID of the script and the identities it started, so the next launch or a stop can end them |

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

### Design decisions

**Why two VMs.** In one VM, a kernel exploit gives the agent the gateway
token and the provider credentials. In two VMs, it gives the agent root in
a VM that holds only its own channel key. Egress is still blocked by the
Tensorlake fence and still mediated by the guard. This matches the
Kubernetes driver, which runs one guard pod per workload pod.

| Attacker | Gets |
|---|---|
| Agent with a kernel exploit | Root in a VM with no credentials |
| Project member with `tl sbx exec --user root` | Everything in the guard VM |
| Holder of a guard VM's Tensorlake key | Tunnels to any sandbox in the project |

The guard key is the new exposure. Keep the control plane in a project of
its own. Keys scoped to one tunnel target would close it and need platform
work.

**Why the tunnel.** Tensorlake has no private network between sandboxes
and no sandbox IP in its API. The HTTPS port proxy terminates TLS, so it
cannot carry the OpenShell channel. The tunnel relays raw TCP and works
with the workload's internet off. Measured on 2026-10-20: 100 tunnels from
one sandbox, 500 concurrent connections through one tunnel, about 300 MiB/s
each way, still relaying after 13 minutes idle, 15 MiB RSS per tunnel. If
Tensorlake adds a sandbox IP, the driver puts it into the `Tcp` transport
and drops the tunnel. Nothing else changes.

**Why not guards in the control sandbox.** That saves the guard VMs, but
it puts every credential of the project in one kernel, shares 2 vCPUs
across all guards, and drops every session when the control sandbox
restarts. A 4 GiB control sandbox would fit about 50 guards.

**Cost.** Two VMs per sandbox. The guard VM defaults to 1 vCPU and 1 GiB;
the guard uses about 32 MiB and the tunnel 15 MiB. Both VMs boot in
parallel, so create to ready stays a few seconds. All workload egress
crosses the guard VM, which pays for TLS and policy on it. Raise
`--guard-cpus` for agents that move bulk data.

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
| `--tensorlake-platform-api-url` | `TENSORLAKE_PLATFORM_API_URL` | `https://api.tensorlake.ai`; where the tunnel checks its key |
| `--tensorlake-api-key` | `TENSORLAKE_API_KEY` | required |
| `--sandbox-image` | `OPENSHELL_SANDBOX_IMAGE` | required |
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

The Tensorlake timeout counts idle time. Keep the default `0`, which uses
the longest timeout your plan allows (7200 seconds on 2026-10-20). In a
test with a 180 second timeout, a sandbox with no user traffic was still
running and Ready after 5 minutes, so the guard's session and the tunnel
appear to keep the clock reset. Do not rely on that for long pauses.

When you set `--allow-out`, the VM that runs the guard can connect only to
the hosts you listed, the gateway host, and the Tensorlake hosts the
tunnel talks to: the sandbox API host, every subdomain of it (the port
proxy lives there), and the platform API host. Tensorlake's resolver then
answers only for those names. Internet access stays on in
the Tensorlake policy, because turning it off also turns off DNS, and the
tunnel cannot find its own API. The workload VM always has internet access
off with an empty allow list.

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
   and `OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED`. So the driver settings
   with an `OPENSHELL_TENSORLAKE_*` variable, the sandbox image, and the
   gateway name can be set this way. The start script sets the socket, the
   gateway endpoint, the gateway CA, and the state directory itself. The
   Tensorlake API URLs, the log level, and the OTLP endpoint cannot be
   changed from the control plane. Then the script starts the control
   plane as a managed process that restarts on failure. Run the script
   again to change settings. Gateway state stays in `/var/lib/openshell`.
   Set `OPENSHELL_DB_URL` to use an external database. `CONTROL_NAME`
   changes the sandbox name from `openshell-control`, and
   `CONTROL_TIMEOUT_SECS` its idle timeout.

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

## Scope

A single team with its own Tensorlake project can run it today.

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
  the phase after each poll observation, and the conditions reported for
  stopped and starting sandboxes.
- Launcher failure messages and the progress events the CLI shows.
- The shape of the Tensorlake API requests, and the saved sandbox records
  that recovery reads.

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
| `lifecycle` | stop, start, delete while stopped | yes |

The `mechanistic-proposal` scenario opens a TCP connection to `1.1.1.1:443`
from Bash inside the sandbox and expects `DENIED`. It then checks that the
proposal names the Bash binary, so it also proves binary identity through
the tunnel.

### Verified on Tensorlake

End-to-end runs on Tensorlake Cloud confirmed:

- On 2026-10-20: create to Ready in about 4 seconds with two
  VMs; `exec` runs as the workload user; the workload VM holds no gateway
  token or Tensorlake key and only the runtime, the forwarder, and the
  launch script run in it; DNS resolves through the guard; the default
  policy denies egress; after `openshell policy update` allows one host
  for `/usr/bin/curl`, that fetch returns 200, another host and the same
  binary at another path stay denied, and the guard logs the binary
  identity; 20 sequential fetches through guard and tunnel took 3.7
  seconds; delete removes both VMs; a guard launcher exit is reported as
  `LauncherExited`.
- The VM kernel supports Landlock, seccomp, network, mount,
  and PID namespaces; the file API writes as root and the process API runs
  the launch script as root; the guard reaches the gateway through the port
  proxy; image builds accept the multi-stage Dockerfiles and the `--public`
  flag.
- On 2026-10-20, with the control plane on Tensorlake: the conformance
  test files `smoke`, `file_transfer`, and `policy_advisor` pass, 7
  scenarios in about 2 minutes, and `mechanistic-proposal` sees `DENIED`
  through the tunnel. Killing the launch script in either VM gives
  `LauncherExited` with the role in the message. After a control plane
  restart, the driver recovered 3 sandboxes; a Ready sandbox stayed Ready
  and `exec` worked, and no `name cannot be changed` error appeared. A
  sandbox created 3 seconds before the restart had both VMs running but no
  launch script, and stayed in `Provisioning`. Delete and recreate such a sandbox.
  With a 180 second idle timeout, a sandbox with no user traffic was still
  running and Ready after 5 minutes. The tunnel first calls
  `api.tensorlake.ai` to check its key, then `sandbox.tensorlake.ai`; the
  guard reaches the gateway through `*.sandbox.tensorlake.ai`. A
  Tensorlake policy with internet off and an allow list blocks all DNS,
  so the tunnel failed with `dns error`; with internet on and an allow
  list, listed hosts connect, an unlisted IP is reset at the TLS
  handshake, and the resolver answers only for listed names. With
  `--allow-out api.anthropic.com`, create to `hello` works and the guard
  VM reaches that host. Delete removes both VMs of every sandbox.

- On 2026-10-21, stop and start: the conformance test file `lifecycle`
  passes. Stop takes about 5 seconds, including the launcher's stop run in
  both VMs and the two suspend requests. Start resumes both VMs and
  reaches Ready in about 4 seconds; a file written to `/sandbox` before
  the stop is still there, the main process runs a second time, and a
  stopped sandbox can be deleted. A guard VM suspended with `tl sbx
  suspend` shows the sandbox as `Stopped` within 3 seconds, and start
  brings it back. `tl sbx suspend` on a running workload VM did not finish
  within 120 seconds; the driver's stop ends the runtime first and then
  suspends without delay. The `smoke` test file still passes.

Three findings shaped the design. The sandbox runtime rejects
boundary peers on loopback, so the tunnel needs the veth forwarder. Binary
identity resolution walks a process's ancestors to its PID namespace init,
so the runtime must run in its own PID namespace. And the transparent TCP capability
is for guards that share the workload's network namespace, so this driver
no longer grants it.

## Next steps

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
the project that owns the public images. Publish from a commit whose
tests passed, because a published version cannot be replaced.

### 3. Later

- GPU.
- Tensorlake keys scoped to one tunnel target, so a guard VM's key cannot
  reach other sandboxes.
- A tunnel that reconnects after a dropped WebSocket, so the sandbox pauses
  instead of failing.
