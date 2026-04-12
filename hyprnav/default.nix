{
  lib,
  cmake,
  rustPlatform,
  kdePackages,
  pkg-config,
  qt6,
  stdenv,
}:
rustPlatform.buildRustPackage {
  pname = "hyprnav";
  version = "0.1";
  src = builtins.path {
    name = "hyprland-plugins";
    path = ../.;
  };
  sourceRoot = "hyprland-plugins/hyprnav";
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = false;

  nativeBuildInputs = [
    cmake
    pkg-config
    qt6.wrapQtAppsHook
  ];

  buildInputs = [
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
    mkdir -p $out/share/applications
    install -m 0644 ${./hyprnav.desktop} $out/share/applications/hyprnav.desktop
  '';

  meta = with lib; {
    description = "Rust/QML workspace navigation server and overlay for Hyprland";
    license = licenses.bsd3;
    platforms = platforms.linux;
    mainProgram = "hyprnav";
  };
}
