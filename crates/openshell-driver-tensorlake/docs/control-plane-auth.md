# Control-plane authentication on Tensorlake

Status: proposed. The current `control/` setup uses OIDC, or no user
authentication for tests.

## Problem

The control plane (gateway and driver) runs in a Tensorlake sandbox. Guards
in other sandboxes reach the gateway through the Tensorlake port proxy:

```
https://8080-<sandbox-id>.sandbox.tensorlake.ai
```

Guards cannot hold a Tensorlake key, so this port must accept anonymous
requests at the proxy. The gateway must then authenticate users itself.

Today there are two choices:

- OIDC: safe, but needs an identity provider.
- `OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1`: anyone with the URL controls
  the gateway. Test only.

We need a production setup without an identity provider.

## Facts from the code

1. **Guard calls reject anonymous callers.** A guard calls 17 gateway
   methods. 14 accept only a sandbox principal (sandbox JWT). 3 are dual
   (`GetSandboxConfig`, `UpdateConfig`, `GetDraftPolicy`) and also accept a
   user. With `allow_unauthenticated_users = false` and a configured
   authenticator, a caller without credentials gets `Unauthenticated` on
   all of them (`crates/openshell-server/src/multiplex.rs`, `AuthGrpcRouter`).
2. **Client certificates are optional at the TLS handshake.** With a client
   CA, the gateway accepts connections without a certificate
   (`allow_unauthenticated()` in `crates/openshell-server/src/tls.rs`). A
   verified certificate becomes a user only when `mtls_auth.enabled` is set.
   So one TLS port serves mTLS users and guards with sandbox JWTs.
3. **A Tensorlake tunnel is raw TCP to `127.0.0.1:<port>`** in the sandbox,
   and every connection needs a Tensorlake credential. The port does not
   need to be exposed. TLS stays end to end, so the client certificate
   reaches the gateway.
4. The gateway has one gRPC listener and no per-listener auth settings.

## Design

```
                        CONTROL SANDBOX
                     ┌────────────────────────────────────────────┐
 guards ── HTTPS ──► │ :8080 (exposed)  local gRPC proxy           │
 (sandbox JWT)       │   re-encrypts ─────────┐                    │
                     │                        ▼                    │
 users ── tl tunnel ───────────────────► 127.0.0.1:8443 gateway    │
 (Tensorlake key +   │                  TLS, mTLS users on,        │
  client cert)       │                  anonymous users off        │
                     └────────────────────────────────────────────┘
```

| Caller | Path | Checks |
|---|---|---|
| Guard | port proxy → local proxy → gateway | sandbox JWT |
| User (CLI) | `tl sbx tunnel 8443` → gateway | Tensorlake credential (tunnel), then client certificate (mTLS) |
| Anyone else | port proxy → local proxy → gateway | no credential, so `Unauthenticated` |

### Components

- **Gateway**: binds `127.0.0.1:8443` with TLS. Settings:
  `mtls_auth.enabled = true`, `allow_unauthenticated_users = false`, sandbox
  JWT signing as today. Server certificate SANs include `127.0.0.1` and
  `localhost`, for the tunnel.
- **Local gRPC proxy** (nginx `grpc_pass grpcs://127.0.0.1:8443`): listens on
  `:8080` with plaintext HTTP/2 behind the Tensorlake port proxy. It
  forwards all gRPC to the gateway over TLS, without a client certificate.
  Long timeouts keep supervisor streams open.
- **Certificates**: `openshell-gateway generate-certs` creates the CA, the
  server certificate, and a client certificate on first start. The user
  downloads the client bundle once with `tl sbx cp`. Treat it as a secret.
- **Driver**: no change. `--grpc-endpoint` stays the public URL, and
  `--gateway-tls-ca` stays the public CA bundle, because guards still go
  through the Tensorlake port proxy.

### User setup

```shell
tl sbx cp openshell-control:/var/lib/openshell/keys/client/ ~/.config/openshell/...
tl sbx tunnel openshell-control 8443 --listen-port 18443
openshell gateway add https://127.0.0.1:18443 --local --name tensorlake
```

The exact CLI registration and certificate paths need confirmation during
implementation.

## Security notes

- Two checks for users: Tensorlake project access and the client
  certificate.
- Anyone with Tensorlake project access can already run
  `tl sbx exec --user root` in the control sandbox and read its keys. The
  client certificate does not protect against project members. Use a
  separate Tensorlake project for the control plane.
- The public port exposes only what a sandbox JWT can call. The 3 dual
  methods still need a user or sandbox principal.
- Health endpoints stay unauthenticated, as today.

## Later: OIDC with Tensorlake identity

If the Tensorlake login system is an OIDC provider, the gateway can trust
Tensorlake accounts directly (`OPENSHELL_OIDC_ISSUER`). Users then log in
with their Tensorlake identity, and no client certificate is needed. This
needs Tensorlake-side setup.

## Open questions

- Does the Tensorlake port proxy keep long-lived gRPC streams open?
- Does `openshell gateway add` support a tunneled mTLS endpoint without an
  SSH remote? If not, which registration fits best?
- nginx or another small proxy (for example Envoy) in the control image?
