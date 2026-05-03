# hyprnav

Local Hyprland workspace navigation tooling for this NixOS setup.

This repository contains two active components:

- `hyprnav`: the Rust/QML workspace environment daemon and overlays.
- `hyprnav-plugin`: the Hyprland plugin that provides preview images and plugin-assisted spawn placement for `hyprnav`.

The local integration source of truth lives in:

- `/etc/nixos/anoromi/hyprland.nix`
- `/etc/nixos/anoromi/config/hypr/hyprland.conf`

## Local Workflow

Build and restart the switcher daemon:

```bash
hyprnav-dev-build
```

Build and reload the Hyprland plugin:

```bash
hyprnav-plugin-dev-reload
```

Load the current local or packaged plugin:

```bash
hyprnav-plugin-load
```

Useful checks:

```bash
hyprctl configerrors
hyprctl plugin list
hyprnav daemon
hyprnav trigger
hyprnav grid
hyprctl -j layers
hyprctl -j clients
```
