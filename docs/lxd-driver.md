# `openshell-driver-lxd`: design and status

_Last updated: 2026-10-22. Built against NVIDIA/OpenShell
`39d95b42cb403d883e2f545a3861131ccd3d2ebf`; the driver depends on OpenShell's
own `openshell-core`, `openshell-isolation-interface` and
`openshell-sandbox-backend` crates at that revision for the protocol types,
the bootstrap/boundary formats and the launch-authentication model._

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
openshell-driver-lxd --socket /run/openshell/lxd.sock \
  --grpc-endpoint https://127.0.0.1:17670 --tls-ca /path/to/gateway-ca.crt
```

Flags worth knowing (all have `OPENSHELL_*` environment equivalents):

| Flag | Purpose |
|---|---|
| `--grpc-endpoint` | Gateway endpoint as the supervisor dials it. Required. |
| `--tls-ca` | Gateway CA the supervisor trusts. |
| `--sandbox-runtime-image` | OCI image shipping `/openshell-sandbox` (default: OpenShell's published image for the pinned version). |
| `--supervisor-image` | OCI image shipping `/openshell-supervisor`. |
| `--supervisor-network` | Run the supervisor on an LXD managed network instead of the host network namespace. |
| `--host-gateway-ip` | Address the supervisor uses for the host (default loopback, or the bridge address). |
| `--project`, `--storage-pool`, `--gpu`, `--nvidia-runtime` | LXD placement and GPU passthrough. |

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
are copy-on-write on ZFS/btrfs/Ceph). The OCI config (`Env`, `User`,
`WorkingDir`) is re-read from the registry on each create, which
also pins the sandbox to a digest.

Alternatives considered: running OpenShell's stock Podman driver nested in a
single LXD container (fast, but sandboxes stop being LXD instances, which the
desktop app and MicroCloud placement rely on), or targeting Incus (drops
Ubuntu's LXD snap and MicroCloud).

## Sandbox = two LXD containers

OpenShell isolates a sandbox with two containers, and so does this driver.

| Instance | Name | Runs | Network |
|---|---|---|---|
| workload | `openshell-<id>` | `/opt/openshell/bin/openshell-sandbox --bootstrap …` as init, under the resolved non-root identity; it launches the agent only after the supervisor authenticates | none (no profiles, no NIC) |
| supervisor | `openshell-sup-<id>` | `/openshell-supervisor`: gateway session, policy engine, egress proxy | host network namespace (`lxc.net.0.type = none`), or a bridged NIC with `--supervisor-network` |

They share one custom storage volume (`openshell-control-<id>`, mounted at
`/.openshell/channel/control` in both) that holds the runtime-initiated control
socket. The supervisor binds it, the workload runtime dials it with retry, and
the connection is mutually authenticated with per-sandbox mTLS material.

### Create

1. Validate the request; decode `launch_authentication` (required).
2. Pull the supervisor image and the workload image into LXD (see above), and
   extract `/openshell-sandbox` from the runtime image (cached by digest).
3. Create the control volume (`security.shared=true`) and the workload
   instance, stopped. Read `/etc/passwd` and `/etc/group` through the file API,
   refuse images that pre-populate `/.openshell`, and resolve the workload
   identity against the image's own accounts. Images with no `USER` get
   OpenShell's default non-root identity.
4. Create the supervisor instance, stopped.
5. Stage files through the file API while both are stopped:
   - workload: the runtime binary, `bootstrap.json` and the mTLS server key and
     certificate (identity-owned, 0600);
   - supervisor: `runtime-descriptor.json`, `auth.json`, a restart copy of the
     identity and child environment, the gateway CA, `/etc/resolv.conf`, and
     the sandbox token if the gateway sent one.

   The workload never receives supervisor credentials, and the supervisor never
   receives the workload's private key.
6. Verify the outer fence (the workload has no `nic` device, hashed into the
   provisioning evidence), start the workload, hand the control directory to
   the workload identity (a custom volume's root is root-owned), then start the
   supervisor.

Any failure removes both instances and the volume.

### Stop, start, delete, observe

- **Stop** stops the supervisor and then the workload, forcing after 30 s.
- **Start** (the gateway sends fresh `launch_authentication` and a generation
  ID) stops both, rebuilds the bootstrap files from the restart copy, and
  repeats steps 5 and 6. A running pair on a different generation is refused.
- **Delete** removes the supervisor, the workload and the volume;
  `deleted=false` if the sandbox was already gone.
- **Ready** is `True/ContainerRunning` only when **both** instances run. A
  stop the gateway asked for reports `ContainerStopped`; the workload exiting on
  its own, or the supervisor dying, reports `ContainerExited` (terminal).
- **Fail closed:** if the gateway wants a sandbox running and the workload is up
  but the supervisor is not, nothing enforces policy, so the driver stops the
  workload.
- **Watch** follows LXD's lifecycle events, publishes one sandbox per pair, and
  resyncs on reconnect.

### Per-instance settings

- **Init**: `raw.lxc` sets `lxc.init.cmd`, `lxc.init.uid`/`gid`/`groups`,
  `lxc.init.cwd` and `lxc.no_new_privs = 1`. Unprivileged, nesting off.
- **Environment**: OpenShell's supervisor variables go on the supervisor
  (`OPENSHELL_SANDBOX_ID`, `OPENSHELL_ENDPOINT`, the main-process spec, the
  user environment as JSON, …). The agent's environment (image `ENV`, template,
  sandbox, minus `OPENSHELL_*`) travels inside `bootstrap.json`, so it never
  reaches the privileged runtime.
- **Resources**: `cpu_limit` → `limits.cpu.allowance` (`500m` → `50ms/100ms`),
  `memory_limit` → `limits.memory`; applied to the workload.
- **GPU**: one physical GPU device on the workload with `--gpu <id|pci:addr>`,
  plus `nvidia.runtime=true` with `--nvidia-runtime`.
- **Handshake**: protocol 1.0 with `openshell.compute.contract`; the
  `resource_admission_policy` acknowledgement echoes
  `--admission-config-json` (default `{}`), which must match the gateway's
  policy for the `lxd` driver.

## Not verified yet

This has been exercised against a fake LXD in the test suite, not against a real
LXD, a real gateway, or OpenShell's published images. These are the assumptions
to check first:

- **Host network for the supervisor.** `lxc.net.0.type = none` in `raw.lxc` is
  meant to share the host's network namespace (what Podman's `netns host` does).
  If LXD's AppArmor profile or liblxc refuses it, use `--supervisor-network
  lxdbr0`; that needs a supervisor image that brings its own interface up, since
  LXD only configures networking through the guest.
- **Nested sandboxing.** The runtime uses Landlock and a seccomp notification
  broker inside an unprivileged LXD container. LXD's seccomp and AppArmor
  profiles may block parts of that; `security.syscalls.intercept.*` or a
  custom profile might be needed.
- **Shared user namespace.** The two instances must use the same idmap so the
  supervisor can bind a socket the workload can dial. That holds for LXD's
  default shared map (no profiles, `security.idmap.isolated` unset).
- **Control directory ownership.** The driver chowns the mounted volume through
  the file API after the workload starts; the runtime retries dialing until the
  supervisor binds, so the brief window should be harmless.
- **Registry access.** Pulls are anonymous. The default workload image
  (`nvcr.io/nvidia/base/ubuntu:24.04`) and OpenShell's images may need
  credentials.
- **Gateway reachability.** Inside the snap the gateway cannot reach the driver
  socket until the gateway is bundled or shared through a content interface.

## Next steps

1. Run it against a real gateway and LXD (CI with the LXD snap on
   `ubuntu-24.04`) and fix what the list above turns up.
2. Registry credentials (Docker config / snap-managed secrets), zstd layers,
   and preserving file capabilities/xattrs from layers.
3. Corporate proxy and extra CA bundles for the supervisor, user mounts.
4. One LXD project per workspace (`EnsureWorkspace`), and MicroCloud targets
   over the LXD TLS API.
