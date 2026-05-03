{
  description = "Local hyprnav workspace navigation tooling";

  inputs = {
    hyprland.url = "github:hyprwm/Hyprland";
    nixpkgs.follows = "hyprland/nixpkgs";
    systems.follows = "hyprland/systems";
  };

  outputs = {
    self,
    hyprland,
    nixpkgs,
    systems,
    ...
  }: let
    inherit (nixpkgs) lib;
    eachSystem = lib.genAttrs (import systems);

    pkgsFor = eachSystem (system:
      import nixpkgs {
        localSystem.system = system;
        overlays = [
          self.overlays.hyprnav
          hyprland.overlays.hyprland-packages
        ];
      });
  in {
    packages = eachSystem (system: {
      hyprnav = pkgsFor.${system}.hyprnav;
      hyprnav-plugin = pkgsFor.${system}.hyprlandPlugins.hyprnav-plugin;
      default = self.packages.${system}.hyprnav;
    });

    overlays = {
      default = self.overlays.hyprnav;

      hyprnav = final: prev: let
        inherit (final) callPackage;
      in {
        hyprnav = callPackage ./hyprnav {};
        hyprlandPlugins =
          (prev.hyprlandPlugins or {})
          // {
            hyprnav-plugin = callPackage ./hyprnav-plugin {};
          };
      };
    };

    checks = eachSystem (system: {
      inherit (self.packages.${system}) hyprnav hyprnav-plugin;
    });

    devShells = eachSystem (system:
      with pkgsFor.${system}; {
        default = mkShell.override {stdenv = gcc14Stdenv;} {
          name = "hyprnav";
          buildInputs = [
            hyprland.packages.${system}.hyprland
            qt6.qtbase
            qt6.qtdeclarative
            qt6.qtwayland
            kdePackages."layer-shell-qt"
            libjpeg
            pkg-config
            cmake
            cargo
            rustc
          ];
          inputsFrom = [hyprland.packages.${system}.hyprland];
        };
      });
  };
}
