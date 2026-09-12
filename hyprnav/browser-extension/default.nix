{ stdenvNoCC, zip }:
stdenvNoCC.mkDerivation {
  pname = "hyprnav-browser-extension";
  version = "0.1.0";
  src = ./.;
  nativeBuildInputs = [ zip ];
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/share/hyprnav"
    zip -X "$out/share/hyprnav/hyprnav.xpi" manifest.json background.js url.js
    runHook postInstall
  '';
}
