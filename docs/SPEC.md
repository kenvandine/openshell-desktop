# Product & Technical Specification: OpenShell Desktop Manager (`openshell-desktop`)

> Status: initial draft. See [`compute-driver-research.md`](compute-driver-research.md)
> for findings that refine the Phase 1 plan below.

## 1. Overview & Objective

`openshell-desktop` is a native GNOME/Ubuntu desktop GUI designed to configure,
launch, and monitor NVIDIA OpenShell agent environments. It packages OpenShell
and its execution harness into an easy-to-use desktop client for developers
working on workstations or laptops.

Under the hood, sandbox workloads are executed inside local LXD containers/VMs
or offloaded seamlessly to an on-premise MicroCloud cluster. The GUI abstracts
complex Landlock filesystem trees, egress network proxies, and LXD configuration
into a simplified, card-based interface with desktop notification integration
for runtime policy approvals.

## 2. Architecture & Components

```
┌────────────────────────────────────────────────────────┐
│               openshell-desktop (GUI)                  │
│       GTK4 / Libadwaita (packaged as strict Snap)      │
└──────────────┬───────────────────────────┬─────────────┘
               │ UNIX Domain Socket / D-Bus│
               ▼                           ▼
┌───────────────────────────────┐ ┌──────────────────────┐
│   OpenShell Gateway Daemon    │ │    LXD Unix Socket   │
│   (snap.openshell.gateway)    │ │ /var/snap/lxd/common │
└──────────────┬────────────────┘ └──────────┬───────────┘
               │ Compute Driver              │
               ▼                             ▼
┌────────────────────────────────────────────────────────┐
│       LXD Engine / MicroCloud Compute Driver           │
│   - Ephemeral or persistent system containers / VMs    │
│   - OVN / Bridge isolated networking                   │
│   - Device passthrough (NVIDIA GPU / AMD APU / DRI)    │
│   - Rootfs Landlock & seccomp profiles                 │
└────────────────────────────────────────────────────────┘
```

### Key Integrations

- **Packaging:** Strictly confined or classic-permitted Snap with plugs for
  `lxd`, `network`, `network-bind`, and desktop interfaces.
- **Compute Driver:** Custom LXD compute driver targeting
  `/var/snap/lxd/common/lxd/unix.socket` using the LXD REST API to manage
  sandboxes.
- **Backend Target Selection:**
  - *Local Workstation/Laptop:* Standalone local LXD instance (default bridge `lxdbr0`).
  - *MicroCloud:* Connects to the local MicroCloud OVN network and storage pool
    (Ceph/ZFS) or remote endpoint over TLS via standard LXD remotes (`lxc remote list`).

## 3. Core Functional Requirements

### 3.1 First-Run Setup & Backend Wizard

- Detects whether the `lxd` snap is installed, initialized (`lxd init`), and
  whether the current user belongs to the `lxd` group.
- One-click initialization toggle:
  - *Local Standalone:* Auto-configures a minimal local storage pool (`dir` or
    `zfs`) and bridge network.
  - *MicroCloud / Remote LXD:* Dropdown to select existing LXD remotes
    authenticated via certificate/token.
- Health indicator in the header bar showing:
  - Gateway status: Active / Stopped
  - Compute driver: LXD (Local) or LXD (MicroCloud: `<cluster-name>`)

### 3.2 Agent & Sandbox Profile Management

- **Preset Templates:**
  - *Code Assistant* (e.g., Claude Code, Cursor agent, local runner): Mounts a
    specific project directory read-write; locks `/etc`, `/usr` to read-only;
    egress allowed strictly to package registries and inference APIs.
  - *Web Researcher / Scraper:* Ephemeral rootfs; strict Landlock; broad
    HTTP/HTTPS egress.
  - *Fully Offline / Airgapped:* All network blocked; local hardware
    acceleration enabled.
- **Resource Controls:**
  - Sliders for CPU limits (`limits.cpu`) and RAM limits (`limits.memory`).
  - GPU passthrough toggle: passes host GPU (`nvidia.com/gpu` or `/dev/dri`)
    directly to the LXD sandbox container.

### 3.3 Visual Policy Builder & Runtime Advisor

- **Filesystem Access:**
  - Directory picker mapping host paths to sandbox paths
    (`/workspace` → `~/Projects/foo`).
  - Access flags: Read-Only, Read-Write, or Hidden.
- **Network Egress Firewall:**
  - Allowlist destinations by domain or CIDR (e.g. `api.anthropic.com`,
    `github.com`, `registry.npmjs.org`).
  - Port restrictions (443, 80, custom).
- **Live Policy Advisor Integration:**
  - Displays blocked outbound calls intercepted by OpenShell's proxy in real time.
  - Native desktop notifications: *"Agent 'DataBot' requested access to
    api.github.com:443. [Allow Once] [Always Allow] [Deny]"*.

### 3.4 Runtime Monitoring & Logs

- **Active Sandboxes List:** running state, container PID, CPU/RAM
  utilization, active policy profile, and running duration.
- **Inspect View:**
  - Real-time stream of audit logs (file access, network requests approved/denied).
  - Embedded terminal/console tab attaching directly to the LXD container via
    `lxc exec <sandbox-id> -- /bin/bash`.
- **Killswitch:** Immediate "Terminate & Purge" button that tears down the
  container and deletes ephemeral storage snapshots.

## 4. UI/UX Wireframe & Screen Layout

The UI adheres to the GNOME HIG using Libadwaita.

```
┌────────────────────────────────────────────────────────────────────────┐
│ ≡  OpenShell Manager          [ Local LXD: Active ● ]       —  □  ✕   │
├──────────────────────┬─────────────────────────────────────────────────┤
│ Sandboxes            │ Active Sandbox: code-eval-01                    │
│                      ├─────────────────────────────────────────────────┤
│  ● code-eval-01      │ Status: Running (LXD Container)                 │
│    Python · 1.2 GB   │ Uptime: 24m · Memory: 840 MB / 4 GB             │
│                      │                                                 │
│  ○ data-scraper      │ [ Open Terminal ]   [ Policy Rules ]   [ Stop ] │
│    Node · Stopped    │                                                 │
│                      ├─────────────────────────────────────────────────┤
│  ● doc-indexer       │ Policy Advisor (Recent Prompts)                 │
│    Rust · 512 MB     │                                                 │
│                      │ ⚠️ Blocked: GET https://registry.npmjs.org      │
│                      │    Requested by: /usr/bin/npm                   │
│                      │    [ Allow Domain ]  [ Ignore ]                 │
│                      ├─────────────────────────────────────────────────┤
│                      │ Filesystem Mounts                               │
│                      │ 📁 ~/Projects/web-app ──► /workspace (RW)       │
│                      │ 🔒 /etc, /usr, /lib   ──► System (RO)           │
│                      ├─────────────────────────────────────────────────┤
│ [+] New Sandbox      │ Network Rules                                   │
│ ⚙ Settings           │ ✓ api.openai.com:443    ✓ huggingface.co:443   │
└──────────────────────┴─────────────────────────────────────────────────┘
```

## 5. Implementation Roadmap

### Phase 1: LXD Compute Driver Plugin for OpenShell

- Build an out-of-tree OpenShell compute driver implementing the OpenShell
  `compute_driver.proto` gRPC service over a Unix socket (see research doc):
  - `CreateSandbox` → LXD REST API (`POST /1.0/instances`).
  - `StartSandbox`, `StopSandbox`, `DeleteSandbox`, `GetSandbox`,
    `ListSandboxes`, `WatchSandboxes`.
  - Apply Landlock/seccomp profiles via LXD security configuration where they
    complement the in-sandbox OpenShell supervisor.

### Phase 2: Core Desktop Application (`openshell-desktop`)

- Frontend in Rust (gtk4-rs + libadwaita, GtkBuilder templates; no cross-platform
  UI layer, since the target is the Ubuntu/GNOME desktop).
- Connect to OpenShell's local management socket and the LXD daemon over
  `/var/snap/lxd/common/lxd/unix.socket`.
- Implement the visual policy editor and profile templates.
- Debug console: LXD REST exec + websockets (no `lxc` binary, so it works in a
  strict snap), drawn by a GTK widget on libghostty-vt. A policy-enforced
  terminal will use OpenShell's gateway sessions instead.

### Phase 3: Desktop Notifications & Policy Advisor Daemon

- Hook into OpenShell's Policy Advisor event stream.
- Dispatch interactive FreeDesktop notification prompts
  (`org.freedesktop.Notifications`) with action buttons for instant rule
  grant/denial.

### Phase 4: Snap Packaging & Distribution

- `snapcraft.yaml` combining the GUI frontend, the OpenShell CLI/gateway, and
  the LXD compute driver:

  ```yaml
  name: openshell-desktop
  base: core24
  confinement: strict
  plugs:
    - lxd
    - desktop
    - desktop-legacy
    - wayland
    - x11
    - network
    - network-bind
  ```

- Provide auto-connect assertions or clear desktop permission requests for the
  LXD socket.

## Concrete Next Steps

- [x] **Driver interface:** Verify whether the targeted OpenShell release
  requires a bespoke compute driver binary or can drive LXD directly.
  → Answered in [`compute-driver-research.md`](compute-driver-research.md).
- [x] **Prototype the GUI:** Bootstrap a GTK4/Libadwaita application skeleton
  (`Adw.ApplicationWindow`, `Adw.PreferencesPage` for sandbox policies).
- [x] **LXD compute driver (lifecycle):** `crates/openshell-driver-lxd`, see
  [`lxd-driver.md`](lxd-driver.md). Supervisor companion still to do.
- [ ] **Socket testing:** Exercise basic container lifecycle operations against
  the local `/var/snap/lxd/common/lxd/unix.socket` to validate security
  profiles before hooking in the OpenShell gateway.

## Sources

- NVIDIA OpenShell documentation & architecture
- NVIDIA OpenShell Policy Advisor & engine
- LXD documentation & REST API reference
- GNOME Human Interface Guidelines (Libadwaita)
