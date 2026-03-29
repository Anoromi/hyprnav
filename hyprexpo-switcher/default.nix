{
  lib,
  cmake,
  kdePackages,
  pkg-config,
  qt6,
  stdenv,
}:
stdenv.mkDerivation {
  pname = "hyprexpo-switcher";
  version = "0.1";
  src = builtins.path {
    name = "hyprland-plugins";
    path = ../.;
  };
  sourceRoot = "hyprland-plugins/hyprexpo-switcher";

  nativeBuildInputs = [
    cmake
    qt6.wrapQtAppsHook
  ];

  buildInputs = [
    kdePackages."layer-shell-qt"
    (lib.getDev kdePackages."layer-shell-qt")
    qt6.qtbase
    qt6.qtdeclarative
    qt6.qtwayland
  ];

  cmakeFlags = [
    "-DLayerShellQt_DIR=${lib.getDev kdePackages."layer-shell-qt"}/lib/cmake/LayerShellQt"
  ];

  postInstall = ''
    mkdir -p $out/share/applications
    cat > $out/share/applications/hyprexpo-switcher.desktop <<'EOF'
    [Desktop Entry]
    Name=Hyprexpo Switcher
    Exec=hyprexpo-switcher daemon
    Type=Application
    StartupWMClass=hyprexpo-switcher
    Categories=Utility;
    NoDisplay=true
    EOF
  '';

  meta = with lib; {
    description = "Qt/QML workspace switcher for hyprexpo previews";
    license = licenses.bsd3;
    platforms = platforms.linux;
    mainProgram = "hyprexpo-switcher";
  };
}
