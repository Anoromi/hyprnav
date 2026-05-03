{
  lib,
  hyprland,
  hyprlandPlugins,
  libjpeg,
}:
hyprlandPlugins.mkHyprlandPlugin {
  pluginName = "hyprnav-plugin";
  version = "0.1";
  src = ./.;

  inherit (hyprland) nativeBuildInputs;
  buildInputs = [ libjpeg ];

  meta = with lib; {
    description = "Preview and spawn integration for hyprnav";
    license = licenses.mit;
    platforms = platforms.linux;
  };
}
