# hyprnav-plugin

`hyprnav-plugin` is the local Hyprland plugin used by `hyprnav`.

It provides:

- preview image rendering for workspace cards
- a preview refresh dispatcher
- plugin-assisted spawn placement through the runtime spawn socket

## Dispatcher

```hyprlang
hyprnav:preview <workspace-id> [workspace-id...]
```

## Config

```hyprlang
plugin {
    hyprnav-plugin {
        preview_height = 480
    }
}
```
