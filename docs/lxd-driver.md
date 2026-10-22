# `openshell-driver-lxd`: design and status

_Last updated: 2026-10-22. Protos vendored from NVIDIA/OpenShell
`39d95b42cb403d883e2f545a3861131ccd3d2ebf`._

The LXD compute driver lives in [`crates/openshell-driver-lxd`](../crates/openshell-driver-lxd).
It is a standalone binary that serves `openshell.compute.v1.ComputeDriver` on a
Unix socket for the OpenShell gateway (see
[compute-driver-research.md](compute-driver-research.md) for why it is out of
tree) and maps sandbox lifecycle onto LXD's REST API over LXD's own Unix
socket. It never runs `lxc`, so it works inside a strict snap with the `lxd`
plug.

```toml
# gateway.toml
[openshell.gateway]
compute_drivers = ["lxd"]

[openshell.drivers.lxd]
socket_path = "/run/openshell/lxd.sock"
```

```sh
openshell-driver-lxd --socket /run/openshell/lxd.sock --project default --storage-pool default
```

## How LXD runs OpenShell's OCI images

OpenShell hands every driver an **OCI image reference** (the workload image,
and later the supervisor/runtime images). Canonical LXD cannot run OCI images:
checked against `canonical/lxd` main (7.0 development), its new image
registries speak only `simplestreams` and `lxd`, and there is no `oci`
protocol. Incus has native OCI support since 6.3, but Ubuntu ships LXD.

The driver therefore imports OCI images itself
([`src/oci.rs`](../crates/openshell-driver-lxd/src/oci.rs)):

1. Resolve the reference to a manifest digest for the host architecture with
   [`oci-client`](https://crates.io/crates/oci-client) (anonymous pulls for now).
2. If LXD already has an image aliased `openshell-oci-<digest>`, reuse it.
3. Otherwise download the layers, merge them top-down with overlayfs whiteout
   semantics (`.wh.<name>` deletes, `.wh..wh..opq` hides a directory's lower
   contents, an upper file hides a lower directory) and stream the result into
   an LXD unified tarball (`metadata.yaml` + `rootfs/`), then
   `POST /1.0/images` and alias it.

This is pure Rust with no `skopeo`/`umoci` in the snap, and every sandbox from
one image shares a single LXD image (and its storage-pool snapshot, so creates
are copy-on-write on ZFS/btrfs/Ceph). The OCI config (`Entrypoint`, `Cmd`,
`Env`, `User`, `WorkingDir`) is re-read from the registry on each create, which
also pins the sandbox to a digest.

Alternatives considered: running OpenShell's stock Podman driver nested in a
single LXD container (fast, but sandboxes stop being LXD instances, which the
desktop app and MicroCloud placement rely on), or targeting Incus (drops
Ubuntu's LXD snap and MicroCloud).

## Sandbox = LXD container

| OpenShell | LXD |
|---|---|
| `CreateSandbox` | import image if needed, `POST /1.0/instances` (stopped), read `/etc/passwd` and `/etc/group` through the file API, resolve identity, set the init process, start |
| `StartSandbox` / `StopSandbox` | `PUT /1.0/instances/<name>/state` (clean stop, then forced after 30s) |
| `DeleteSandbox` | stop, `DELETE /1.0/instances/<name>`; `deleted=false` if already gone |
| `GetSandbox` / `ListSandboxes` | `GET /1.0/instances?recursion=1`, filtered on `user.openshell.managed-by` |
| `WatchSandboxes` | `GET /1.0/events?type=lifecycle` websocket; snapshot first, reconnects and resyncs on drop |
| `EnsureWorkspace` / `DeleteWorkspace` | no-op; the workspace is recorded on each instance |
| `ValidateSandboxCreate` | image, quantities, environment names, GPU and driver config checked without touching LXD |

- **Name**: `openshell-<sandbox id>` (or a stable hash when the ID is not a
  valid hostname). Metadata lives in `user.openshell.*` config keys.
- **No network**: the instance is created with `profiles: []`, so it gets
  only a root disk on the configured pool and no NIC at all. This is the same
  outer fence the Docker and Podman drivers get from `network_mode=none`.
- **Init**: `raw.lxc` sets `lxc.init.cmd` to the sandbox command (or the
  image entrypoint + cmd), `lxc.init.uid`/`gid`/`groups`/`cwd`, and
  `lxc.no_new_privs = 1`. Unprivileged container, nesting off.
- **Identity**: policy identity overrides the image `USER`; names are
  resolved against the image's own account files; uid/gid 0 are refused, as in
  OpenShell's drivers. Reported back as `resolved_identity`.
- **Environment**: image `Env`, then template, then sandbox environment, as
  `environment.*` keys.
- **Resources**: `cpu_limit` → `limits.cpu.allowance` (`500m` → `50ms/100ms`),
  `memory_limit` → `limits.memory` in bytes. Requests are ignored.
- **GPU**: one physical GPU device when the driver is started with
  `--gpu <id|pci:addr>`, plus `nvidia.runtime=true` with `--nvidia-runtime`.
- **Ready condition**: Running → `True/ContainerRunning`; Stopped after a
  gateway stop → `ContainerStopped`; Stopped on its own → `ContainerExited`
  (terminal), matching the reasons OpenShell's restart recovery understands.
- **Handshake**: protocol 1.0 with `openshell.compute.contract`; the
  `resource_admission_policy` acknowledgement echoes
  `--admission-config-json` (default `{}`), which must match the gateway's
  policy for the `lxd` driver.

## Not done yet

The driver does **not** yet run OpenShell's supervisor. The Docker and Podman
drivers run two containers per sandbox: the workload with `openshell-sandbox`
as PID 1 (Landlock, seccomp notification broker), and an `openshell-supervisor`
companion that holds the gateway session, policy engine and egress proxy,
joined by a private Unix-socket volume and per-sandbox mTLS material. Until
that lands here:

- the workload runs the image command directly, with no network;
- because the driver reports `driver_reports_runtime_readiness=false`, the
  gateway waits for a supervisor session that never arrives, so sandboxes
  never turn Ready in OpenShell. That is fail-closed, but it means this
  driver is plumbing, not yet usable end to end.

Next steps, roughly in order:

1. **Supervisor companion.** A second LXD container per sandbox from the
   supervisor image, sharing a custom storage volume for the channel socket;
   stage `launch_authentication` and the bootstrap bundle the way
   `openshell-driver-podman/src/isolation.rs` does. This needs OpenShell's
   `openshell-sandbox-backend` boundary types; depend on OpenShell's crates as
   git dependencies pinned to a release rather than reimplementing them.
2. Validate against a real gateway and LXD (CI with the LXD snap on
   `ubuntu-24.04`), including nested seccomp notification and Landlock inside
   an unprivileged LXD container.
3. Registry credentials (Docker config / snap-managed secrets), zstd layers,
   and preserving file capabilities/xattrs from layers.
4. One LXD project per workspace (`EnsureWorkspace`), and MicroCloud targets
   over the LXD TLS API.
