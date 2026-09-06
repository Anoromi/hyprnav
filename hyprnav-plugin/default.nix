{
  lib,
  hyprland,
  hyprlandPlugins,
}:
hyprlandPlugins.mkHyprlandPlugin {
  pluginName = "hyprnav-plugin";
  version = "0.1";
  src = ./.;

  inherit (hyprland) nativeBuildInputs;
  buildInputs = hyprland.buildInputs;

  meta = with lib; {
    description = "Spawn placement integration for hyprnav";
    license = licenses.mit;
    platforms = platforms.linux;
  };
}
