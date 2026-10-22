# OpenShell Desktop

A native GNOME desktop manager for [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell)
agent sandboxes, running on local LXD or a MicroCloud cluster.

![Main window](docs/screenshot.png)

> **Status:** early prototype. The UI is a skeleton backed by sample data; the
> only live integration is an LXD health probe on the snap socket.

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

## Debug console

**Open Terminal** starts a shell in the selected LXD instance through LXD's
REST API (`POST /1.0/instances/<name>/exec` plus its websockets), so it needs
only the LXD socket and works from a strictly confined snap. The terminal is a
GTK widget drawing libghostty-vt's render state with Pango. The shell runs
outside OpenShell's policy, and the window says so; the policy-enforced
session will come through the OpenShell gateway.

## Layout

| Path | What |
|---|---|
| `data/ui/window.ui` | Window layout as a GtkBuilder template: split view, header bar, primary menu |
| `src/ui/window.rs` | `AdwApplicationWindow` subclass bound to the template; state, actions, status pill |
| `src/ui/sidebar.rs` | Sandbox list |
| `src/ui/detail.rs` | Sandbox detail `AdwPreferencesPage` (status, Policy Advisor, mounts, network, resources) |
| `src/ui/console.rs` | Debug console window |
| `src/terminal/` | libghostty-vt bindings, safe wrapper and the GTK terminal widget |
| `src/model.rs` | View-model types; live LXD instances or sample data |
| `crates/openshell-driver-lxd` | OpenShell compute driver: gRPC on a Unix socket for the gateway, sandboxes as LXD containers, OCI → LXD image import |
| `crates/openshell-lxd` | GTK-free LXD REST client and exec console bridge (reusable by the LXD compute driver) |
