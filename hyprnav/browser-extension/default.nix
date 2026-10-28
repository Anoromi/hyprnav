{ stdenvNoCC, zip }:
stdenvNoCC.mkDerivation {
  pname = "hyprnav-browser-extension";
  version = "0.3.0";
  src = ./.;
  nativeBuildInputs = [ zip ];
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/share/hyprnav/chromium"
    zip -X "$out/share/hyprnav/hyprnav.xpi" manifest.json background.js url.js popup.html popup.js
    cp manifest.chromium.json "$out/share/hyprnav/chromium/manifest.json"
    cp background.js url.js popup.html popup.js "$out/share/hyprnav/chromium/"
    runHook postInstall
  '';
}
