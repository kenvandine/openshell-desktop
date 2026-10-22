# OpenShell Desktop

A native GNOME desktop manager for [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell)
agent sandboxes, running on local LXD or a MicroCloud cluster.

![Main window](docs/screenshot.png)

> **Status:** early prototype. The UI is a skeleton backed by sample data; the
> only live integration is an LXD health probe on the snap socket.

- [Product & technical spec](docs/SPEC.md)
- [Compute driver research](docs/compute-driver-research.md): why we need an
  out-of-tree `openshell-driver-lxd` and how it plugs into the gateway.

## Building

Requires Rust (2024 edition), GTK ≥ 4.14 and libadwaita ≥ 1.5 (Ubuntu 24.04 or newer).

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev
cargo run
```

## Layout

| Path | What |
|---|---|
| `src/ui/window.rs` | Main Relm4 component: split view, header status pill, toasts |
| `src/ui/sidebar.rs` | Sandbox list |
| `src/ui/detail.rs` | Sandbox detail `AdwPreferencesPage` (status, Policy Advisor, mounts, network, resources) |
| `src/model.rs` | View-model types and sample data |
| `src/backend/lxd.rs` | LXD socket health probe |
