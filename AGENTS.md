# Purpose

This fork is not being treated as a general-purpose `hyprland-plugins` checkout.
For local agent work, this repo is specifically for `hyprnav` and
`hyprnav-plugin` development and their integration into the local
`/etc/nixos` Hyprland workflow.

# Primary Scope

- Default to `hyprnav` and `hyprnav-plugin`.
- Treat unrelated plugin directories as out of scope unless the user explicitly asks.
- Evaluate changes by how they affect local Hyprland integration for either
  component, not upstream repo completeness.
- Do not default to upstream-wide cleanup, cross-plugin refactors, or generic
  repo maintenance.

# External Integration Points

These files are part of the active project surface even though they live
outside this repo:

- `/etc/nixos/anoromi/hyprland.nix`
- `/etc/nixos/anoromi/config/hypr/hyprland.conf`

When making local workflow changes, treat this repo and those `/etc/nixos`
files as one integration boundary.

# Local Runtime Contract

Preserve these defaults unless the user explicitly asks to change them:

- Local plugin path: `/home/anoromi/code/stolen/hyprland-plugins/hyprnav-plugin/hyprnav-plugin.so`
- Preserve the wrapped/local command name `hyprnav`
- Preserve `hyprnav-dev-build`
- Preserve the current switcher trigger bindings unless explicitly asked to
  change them

Do not replace the local switcher wrapper/dev-build flow unless explicitly
requested.

# Local Dev Workflow

Default workflow:

1. Change `hyprnav-plugin` and/or `hyprnav` depending on the task.
2. For plugin changes:
   - rebuild and reload with `hyprnav-plugin-dev-reload`
   - verify the plugin is loaded
   - verify preview/spawn integration through the `hyprnav` flows
3. For switcher changes:
   - rebuild with `hyprnav-dev-build`
   - restart the running switcher daemon if needed
   - test using the existing switcher trigger flow
4. Touch `/etc/nixos` only when integration behavior actually needs it.

System-level updates still require:

- `sudo nixos-rebuild switch --flake /etc/nixos#nixos`

Do not assume the upstream repo flake or devShell is the source of truth for
local development. Prefer the local NixOS integration workflow already in use.

# Validation

Use these checks for local verification:

- `hyprctl configerrors`
- `hyprctl plugin list`
- `hyprnav-plugin-dev-reload`
- `hyprnav-dev-build`
- `hyprnav daemon`
- `hyprnav trigger`
- `hyprnav grid`
- `hyprctl -j layers`
- `hyprctl -j clients`
- validate the switcher as a Hyprland overlay, not a normal client

# Default Constraints

- Keep future guidance short and operational.
- Prefer repo-root and `/etc/nixos` integration guidance over upstream generic
  README guidance when they conflict for local work.
- Do not remove or rename the local test flow without user approval.
- Do not remove or rename the local switcher wrapper/dev-build flow without
  user approval.
- Do not widen scope to the rest of the plugin suite unless explicitly asked.
- If the acting agent is an OpenAI model and the task involves UI work, always
  use the `uncodixfy` skill.
