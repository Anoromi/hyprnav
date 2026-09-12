#!/usr/bin/env bash
set -euo pipefail
# The argument is the physical Hyprland workspace containing your browser.
browser_workspace="${1:-3}"
browser_kind="${2:-firefox}"
[[ "$browser_kind" == firefox || "$browser_kind" == chromium ]] || { echo "Expected firefox or chromium" >&2; exit 1; }
[[ "$browser_workspace" =~ ^[1-9][0-9]*$ ]] || { echo 'Expected a positive browser workspace ID' >&2; exit 1; }
hyprnav tab open --browser "$browser_kind" --name demo --url 'https://interactions-33e667f674b8cc14518350eea9255a5d.anoromi.com/?space=hyprnav-browser-demo'
hyprnav env ensure --env hyprnav-browser-demo
slot=0
for workspace in work personal research; do
  slot=$((slot + 1))
  hyprnav slot assign --env hyprnav-browser-demo --slot "$slot" --workspace "$browser_workspace"
  hyprnav tab assign --browser "$browser_kind" --env hyprnav-browser-demo --slot "$slot" --name demo --workspace "$workspace"
done
hyprnav goto --env hyprnav-browser-demo --slot 1
