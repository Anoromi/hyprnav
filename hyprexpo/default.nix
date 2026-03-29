{
  lib,
  hyprland,
  hyprlandPlugins,
  libjpeg,
}:
hyprlandPlugins.mkHyprlandPlugin {
  pluginName = "hyprexpo";
  version = "0.1";
  src = ./.;

  inherit (hyprland) nativeBuildInputs;
  buildInputs = [ libjpeg ];

  meta = with lib; {
    homepage = "https://github.com/hyprwm/hyprland-plugins/tree/main/hyprexpo";
    description = "Hyprland workspaces overview plugin";
    license = licenses.bsd3;
    platforms = platforms.linux;
  };
}
