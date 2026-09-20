{
  lib,
  cmake,
  rustPlatform,
  kdePackages,
  pkg-config,
  qt6,
  stdenv,
  wayland,
  wayland-scanner,
  wayland-protocols,
  hyprland-protocols,
  wlr-protocols,
  libjpeg_turbo,
}:
rustPlatform.buildRustPackage {
  pname = "hyprnav";
  version = "0.1";
  src = builtins.path {
    name = "hyprnav-project";
    path = ../.;
  };
  sourceRoot = "hyprnav-project/hyprnav";
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = false;

  nativeBuildInputs = [
    cmake
    pkg-config
    qt6.wrapQtAppsHook
    wayland-scanner
  ];

  buildInputs = [
    wayland
    wayland-protocols
    hyprland-protocols
    wlr-protocols
    libjpeg_turbo
    kdePackages."layer-shell-qt"
    (lib.getDev kdePackages."layer-shell-qt")
    qt6.qtbase
    qt6.qtdeclarative
    qt6.qtwayland
  ];

  preBuild = ''
    qtMergeRoot="$PWD/.qt-merged"
    mkdir -p "$qtMergeRoot/bin" "$qtMergeRoot/include" "$qtMergeRoot/lib" "$qtMergeRoot/libexec"

    for libDir in ${qt6.qtbase}/lib ${qt6.qtdeclarative}/lib ${qt6.qtwayland}/lib ${kdePackages."layer-shell-qt"}/lib; do
      if [ -d "$libDir" ]; then
        ln -sf "$libDir"/* "$qtMergeRoot/lib/" 2>/dev/null || true
      fi
    done
    for includeDir in ${qt6.qtbase}/include ${qt6.qtdeclarative}/include ${qt6.qtwayland}/include ${
      lib.getDev kdePackages."layer-shell-qt"
    }/include; do
      if [ -d "$includeDir" ]; then
        ln -sf "$includeDir"/* "$qtMergeRoot/include/" 2>/dev/null || true
      fi
    done
    for toolDir in ${qt6.qtbase}/libexec ${qt6.qtdeclarative}/libexec; do
      if [ -d "$toolDir" ]; then
        ln -sf "$toolDir"/* "$qtMergeRoot/libexec/" 2>/dev/null || true
      fi
    done

    cat > "$qtMergeRoot/bin/qmake" <<'EOF'
    #!${stdenv.shell}
    set -euo pipefail
    real_qmake="${qt6.qtbase}/bin/qmake"
    merge_root="$(cd "$(dirname "$0")/.." && pwd)"
    if [ "''${1-}" = "-query" ] && [ "$#" -ge 2 ]; then
      case "$2" in
        QT_HOST_PREFIX|QT_HOST_PREFIX/get|QT_INSTALL_PREFIX|QT_INSTALL_PREFIX/get) printf '%s\n' "$merge_root"; exit 0 ;;
        QT_HOST_BINS|QT_HOST_BINS/get|QT_INSTALL_BINS|QT_INSTALL_BINS/get) printf '%s\n' "$merge_root/bin"; exit 0 ;;
        QT_HOST_LIBEXECS|QT_HOST_LIBEXECS/get|QT_INSTALL_LIBEXECS|QT_INSTALL_LIBEXECS/get) printf '%s\n' "$merge_root/libexec"; exit 0 ;;
        QT_INSTALL_HEADERS|QT_INSTALL_HEADERS/get) printf '%s\n' "$merge_root/include"; exit 0 ;;
        QT_INSTALL_LIBS|QT_INSTALL_LIBS/get) printf '%s\n' "$merge_root/lib"; exit 0 ;;
      esac
    fi
    exec "$real_qmake" "$@"
    EOF
    chmod +x "$qtMergeRoot/bin/qmake"
    export QMAKE="$qtMergeRoot/bin/qmake"
  '';

  postInstall = ''
    mkdir -p $out/share/hyprnav
    cp -r browser-extension $out/share/hyprnav/browser-extension
    mkdir -p $out/share/hyprnav/chromium
    cp browser-extension/manifest.chromium.json $out/share/hyprnav/chromium/manifest.json
    cp browser-extension/background.js browser-extension/url.js $out/share/hyprnav/chromium/
    mkdir -p $out/share/applications
    install -m 0644 ${./hyprnav.desktop} $out/share/applications/hyprnav.desktop
      install -Dm755 ../scripts/hyprnav-share-picker $out/bin/hyprnav-share-picker

    # hyprnav-capture: the C helper that pairs Hyprland window addresses with
    # ext-foreign-toplevel identifiers and streams JPEG frames of single
    # windows. Its protocol glue is generated here; nothing generated is in
    # the tree. It is also installed under its identification-only name.
    make -C tools/capture clean \
      WAYLAND_SCANNER=wayland-scanner \
      WAYLAND_PROTOCOLS_DIR=${wayland-protocols}/share/wayland-protocols \
      HYPRLAND_PROTOCOLS_DIR=${hyprland-protocols}/share/hyprland-protocols \
      WLR_PROTOCOLS_DIR=${wlr-protocols}/share/wlr-protocols
    make -C tools/capture \
      WAYLAND_SCANNER=wayland-scanner \
      WAYLAND_PROTOCOLS_DIR=${wayland-protocols}/share/wayland-protocols \
      HYPRLAND_PROTOCOLS_DIR=${hyprland-protocols}/share/hyprland-protocols \
      WLR_PROTOCOLS_DIR=${wlr-protocols}/share/wlr-protocols
    install -Dm755 tools/capture/hyprnav-capture $out/bin/hyprnav-capture
    ln -sf hyprnav-capture $out/bin/hyprnav-toplevel-map
  '';

  meta = with lib; {
    description = "Rust/QML workspace navigation server and overlay for Hyprland";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "hyprnav";
  };
}
