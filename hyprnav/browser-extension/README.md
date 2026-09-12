# Browser navigation

Firefox/Zen extension + `hyprnav tab` native host. A named tab can be used by
several environment slots, each with a different `workspace` query value.

## Nix integration

The flake exports `packages.<system>.hyprnav-browser-extension`, containing
`share/hyprnav/hyprnav.xpi`. It can also be built with
`pkgs.callPackage "${inputs.hyprnav}/hyprnav/browser-extension" {}` using the
system's nixpkgs.

The local `/etc/nixos/anoromi/vicinae-browser.nix` installs this XPI through
Zen's existing extension policies and registers `hyprnav_browser` as a native
host. Its launcher calls the existing profile's `hyprnav` wrapper, preserving
local dev builds. After `nixos-rebuild switch` and restarting Zen, manual
`tab install` and temporary add-on loading are unnecessary. This uses the
unsigned-extension setting already configured for Vicinae.

## Local setup

1. Build with `hyprnav-dev-build`, then run `hyprnav tab install`.
2. In Zen, open `about:debugging#/runtime/this-firefox`, choose **Load Temporary
   Add-on**, and select this directory's `manifest.json`.
3. Run the demo with `bash hyprnav/scripts/browser-demo.sh 3`. Replace `3`
   with the physical Hyprland workspace containing your browser.

The development extension must be loaded again after restarting the browser.
A permanent distribution needs a signed Firefox extension. The native host
registration follows the existing local `hyprnav` wrapper across dev builds.
Only one browser profile can own the bridge at a time. Private tabs are excluded.

Demo: https://interactions-33e667f674b8cc14518350eea9255a5d.anoromi.com/?space=hyprnav-browser-demo

Demo source:
`/home/anoromi/code/experiments/interaction-platform/spaces/hyprnav-browser-demo/`

## Commands

```bash
hyprnav tab open --name app --url 'https://your-app.example/?keep=1#section'
hyprnav tab goto --name app --workspace project-a
hyprnav tab list

hyprnav env ensure --env project
hyprnav slot assign --env project --slot 1 --workspace 3
hyprnav tab assign --env project --slot 1 --name app --workspace project-a
hyprnav goto --env project --slot 1
hyprnav slot resolve --env project --slot 1
hyprnav tab clear --env project --slot 1
```

`tab open --param <name>` selects a different query parameter, default `workspace`.
It adopts one matching tab or creates a tab. Names are stored with browser
session metadata and extension storage. Multiple matching tabs are an error.
A closed tab is reopened from the saved URL on the next navigation. A named tab
that has moved to another origin is rejected, so it cannot silently retarget a
page outside the registered app. Use a new name for a different app URL.

`tab goto` updates only the named parameter in the tab's current URL. Other
query bytes, ordering, duplicate unrelated parameters, path, and fragment are
preserved. A missing parameter is appended. Duplicate target parameters are
rejected. Navigation waits for the browser to report the requested URL before
returning; an unchanged URL only focuses the tab. This uses normal browser
navigation and can reload the page. The app must read the query parameter on
load and persist any state it needs across navigation.

Browser targets run on every slot visit, even when the physical workspace is
occupied. They take precedence over a slot's stored launch command. Missing or
inherited child slots inherit browser navigation until a concrete child slot
stops resolution. Clearing a child target can expose the inherited target.
Removing a slot also removes its browser target. Targets follow the existing
boot-scoped slot lifecycle.

The grid and switcher show browser slots separately. Switcher browser entries
currently follow the physical workspace entries; browser MRU tracking and
per-tab previews are not implemented. They do not show a false active marker
or reuse a screenshot of another tab. The physical browser workspace must
still match the slot mapping; moving the browser requires updating that mapping.

## Checks

```bash
node --test hyprnav/tests/browser-url.test.cjs
npx web-ext lint --source-dir hyprnav/browser-extension
```

Rust library tests cover native frame validation, slot target inheritance and
cleanup, and distinct switcher identities on the same physical workspace.
Use `cargo test --lib` in the activated system's Nix build environment. The
existing binary test target has unresolved CXX/Qt initialization symbols.
