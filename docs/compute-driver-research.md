# Research: Does OpenShell need a bespoke LXD compute driver?

_Last checked: 2026-10-22, against NVIDIA/OpenShell `main` and the published docs._

## Short answer

**Yes, an LXD backend needs its own driver binary, and OpenShell supports that
cleanly as an out-of-tree "extension" compute driver.** OpenShell has no
built-in LXD/Incus driver and cannot hand sandboxes to LXD on its own; it only
knows how to talk to its compute drivers. But the gateway can be pointed at any
process that serves the `openshell.compute.v1.ComputeDriver` gRPC service on a
Unix socket, so we do not need to fork or patch OpenShell.

## What OpenShell ships

Built-in compute drivers (one active per gateway):

| Driver | Backend | Notes |
|---|---|---|
| `docker` | Docker API socket (bollard) | auto-detected |
| `podman` | rootless Podman 5.x socket | auto-detected |
| `kubernetes` | `agents.x-k8s.io` `Sandbox` CRD | auto-detected first |
| `vm` | libkrun microVMs (KVM; QEMU for GPU) | never auto-detected |
| `mxc` | Windows | |

Auto-detection order when `compute_drivers` is unset: Kubernetes → Podman → Docker.

## How an external driver plugs in

Gateway TOML:

```toml
[openshell.gateway]
compute_drivers = ["lxd"]          # any non-reserved name

[openshell.drivers.lxd]
socket_path = "/run/openshell/lxd.sock"
```

or at launch: `openshell-gateway --drivers lxd --compute-driver-socket /run/openshell/lxd.sock`.

- The gateway **connects to** the socket; it does not spawn or supervise the
  driver. We run it as our own service (e.g. a snap daemon), and must restrict
  socket permissions to the gateway uid.
- Protocol negotiation: `GetCapabilities` must return `extension` peer metadata
  declaring protocol `1.0` and capability `openshell.compute.contract`.
  Upgrading the gateway's protocol version means upgrading the driver too.
- The gateway also requires an exact match on `resource_admission_policy`
  (empty = legacy driver, needs explicit opt-out by the operator).

### RPC surface (`proto/compute_driver.proto`, package `openshell.compute.v1`)

| RPC | LXD mapping (proposed) |
|---|---|
| `GetCapabilities` | static; advertise CPU/memory/GPU limit support |
| `ValidateSandboxCreate` | check image, profile, storage pool, resource asks |
| `CreateSandbox` | `POST /1.0/instances` (+ device/config from template) |
| `StartSandbox` / `StopSandbox` | `PUT /1.0/instances/<name>/state` |
| `DeleteSandbox` | `DELETE /1.0/instances/<name>` (+ ephemeral snapshots) |
| `GetSandbox` / `ListSandboxes` | `GET /1.0/instances?recursion=2` filtered by label |
| `WatchSandboxes` (stream) | `GET /1.0/events?type=lifecycle` websocket |
| `EnsureWorkspace` / `DeleteWorkspace` | LXD project per workspace |
| `AuthenticateSandbox` | optional; can defer (`supports_sandbox_authentication=false`) |

Useful request fields: `DriverSandboxTemplate.image` is an **OCI image
reference**; resources arrive as Kubernetes-style quantities (`"500m"`, `"4Gi"`)
that map to `limits.cpu` / `limits.memory`; `platform_config` and
`driver_config` are opaque `Struct`s whose schema the driver owns, which is
where LXD-specific knobs (instance type container/VM, storage pool, GPU device,
remote/cluster target) can live.

## Design implications for the LXD driver

1. **The sandbox security model lives in OpenShell's supervisor, not the
   driver.** The Docker driver runs two containers per sandbox: a workload
   container (`openshell-sandbox` as PID 1: Landlock baseline, seccomp
   notification broker, no network) and a companion `openshell-supervisor`
   container (policy engine, proxy/L7 inspection, credentials, gateway
   session). The LXD driver must reproduce this topology rather than invent its
   own Landlock/egress enforcement. LXD's own isolation (unprivileged
   containers, `security.*`, network ACLs) becomes the outer fence.
2. **Images are OCI.** Either rely on LXD's OCI image support (recent LXD
   releases; needs confirming on the LXD version we target) or have the driver
   unpack OCI images into LXD images. This is the biggest open technical question.
3. **The Policy Advisor events come from the gateway, not LXD.** The desktop
   app should read sandbox state and advisor prompts from the gateway API and
   only use the LXD socket directly for setup/health checks and the console.
4. **MicroCloud comes almost for free**: the driver talks to an LXD endpoint;
   pointing it at a clustered remote over TLS is configuration.

## Recommendation

- Build `openshell-driver-lxd` as a separate Rust binary (same language as
  OpenShell's own drivers, so we can vendor `compute_driver.proto` and reuse
  `tonic`), modelled closely on `crates/openshell-driver-podman`.
- Keep it in this repo as a Cargo workspace member once work starts, or a
  sibling repo if it should be usable without the desktop app.
- First spike: implement `GetCapabilities` + `CreateSandbox`/`DeleteSandbox`
  against a local LXD with a stock OCI image, and confirm the gateway accepts
  the driver.

## References

- Drivers overview: https://docs.nvidia.com/openshell/extensibility/drivers.md
- Compute drivers reference: https://docs.nvidia.com/openshell/v0.0.116/reference/sandbox-compute-drivers.md
- Proto: https://github.com/NVIDIA/OpenShell/blob/main/proto/compute_driver.proto
- Docker driver runtime model: https://github.com/NVIDIA/OpenShell/blob/main/crates/openshell-driver-docker/README.md
- Podman driver: https://github.com/NVIDIA/OpenShell/blob/main/crates/openshell-driver-podman/README.md
