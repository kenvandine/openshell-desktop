# OpenShell Desktop

A native GNOME desktop manager for [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell)
agent sandboxes, running on local LXD or a MicroCloud cluster.

![Main window](docs/screenshot.png)

> **Status:** early prototype. With the bundled gateway and LXD driver running
> (see [Running](#running)) the app lists, creates, starts, stops and deletes
> sandboxes through the OpenShell gateway (with CPU, memory and GPU limits),
> shows each sandbox's whole policy and edits its network rules, mounts host
> folders into the workspace through LXD, and lets you approve or reject the
> policy advisor's proposals, also from the desktop notification raised when a
> sandbox is blocked. Without the gateway it lists LXD instances read-only.
> Not yet verified on real hardware.

- [Product & technical spec](docs/SPEC.md)
- [Compute driver research](docs/compute-driver-research.md): why we need an
  out-of-tree `openshell-driver-lxd` and how it plugs into the gateway.
- [LXD compute driver](docs/lxd-driver.md): design, OCI image import, and
  what is still missing.

## Building

Requires Rust (2024 edition), GTK ≥ 4.14 and libadwaita ≥ 1.5 (Ubuntu 24.04 or newer).
The app is plain gtk4-rs + libadwaita with a GtkBuilder template, the same
stack as GNOME's own Rust apps, so it picks up Ubuntu's Yaru icons, fonts and
accent colour like any other GNOME app on the desktop.

The console's terminal emulation comes from
[libghostty-vt](https://github.com/ghostty-org/ghostty), which is built with
Zig 0.17 and linked as a shared library:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libclang-dev
scripts/build-libghostty-vt.sh          # builds into vendor/ghostty-vt
export PKG_CONFIG_PATH=$PWD/vendor/ghostty-vt/share/pkgconfig
export LD_LIBRARY_PATH=$PWD/vendor/ghostty-vt/lib
cargo run
```

## Snap

`snap/snapcraft.yaml` builds a strict `core24` snap with the app (using the
GNOME extension) and the `openshell-driver-lxd` daemon, which is disabled by
default. The **Snap** workflow builds it for amd64 and arm64 on every push and
pull request and uploads the `.snap` as a build artifact.

```sh
snapcraft                                # or: snapcraft --use-lxd
sudo snap install --dangerous openshell-desktop_*.snap
sudo snap connect openshell-desktop:lxd lxd:lxd
```

## Debug console

**Open Terminal** starts a shell in the selected LXD instance through LXD's
REST API (`POST /1.0/instances/<name>/exec` plus its websockets), so it needs
only the LXD socket and works from a strictly confined snap. The terminal is a
GTK widget drawing libghostty-vt's render state with Pango. The shell runs
outside OpenShell's policy, and the window says so; the policy-enforced
session will come through the OpenShell gateway.

## Running

The snap bundles `openshell-gateway` and the LXD driver as per-user services
(they stay off until you enable them, since the gateway binds one loopback port):

```sh
sudo snap connect openshell-desktop:lxd lxd:lxd
systemctl --user enable --now snap.openshell-desktop.lxd-driver.service \
  snap.openshell-desktop.gateway.service
```

The gateway listens on port 17671 (`snap set openshell-desktop
gateway-port=…` to change it; OpenShell's own snap uses 17670). It accepts
connections on every interface, because sandbox supervisors reach it over
`lxdbr0`; clients still need its mTLS certificate. See
[docs/lxd-driver.md](docs/lxd-driver.md#bundled-gateway-snap).

## Layout

| Path | What |
|---|---|
| `data/ui/window.ui` | Window layout as a GtkBuilder template: split view, header bar, primary menu |
| `src/ui/window.rs` | `AdwApplicationWindow` subclass bound to the template; state, actions, status pill |
| `src/ui/sidebar.rs` | Sandbox list |
| `src/ui/detail.rs` | Sandbox detail `AdwPreferencesPage` (status, Policy Advisor, mounts, network, resources) |
| `src/ui/policy.rs` | Policy Rules dialog: the full sandbox policy, network rules editable |
| `src/ui/dialogs.rs` | New sandbox, add network rule and mount host folder forms |
| `src/ui/preferences.rs` | Preferences dialog; `src/settings.rs` stores its settings in a key file |
| `src/ui/console.rs` | Debug console window |
| `src/terminal/` | libghostty-vt bindings, safe wrapper and the GTK terminal widget |
| `src/gateway.rs` | Background thread that polls the gateway and runs commands for the UI |
| `src/model.rs` | View-model types; gateway sandboxes enriched with LXD details, or LXD instances alone |
| `crates/openshell-gateway-client` | mTLS client for the gateway API: sandboxes, policy, advisor proposals |
| `crates/openshell-driver-lxd` | OpenShell compute driver: gRPC on a Unix socket for the gateway, sandboxes as LXD containers, OCI → LXD image import |
| `crates/openshell-lxd` | GTK-free LXD REST client and exec console bridge (reusable by the LXD compute driver) |
