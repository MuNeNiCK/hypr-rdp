{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  clang,
  ffmpeg,
  libdrm,
  libgbm,
  libva,
  libxkbcommon,
  mesa,
  pipewire,
  wayland,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "hypr-rdp";
  version = cargoToml.package.version;

  src = lib.cleanSource ../..;

  cargoHash = "sha256-MmBf+DqdPttwGR2hj+C8sJKx18bgGwIw2zSvu2psDRU=";

  nativeBuildInputs = [
    pkg-config
    cmake
    clang
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    ffmpeg
    libdrm
    libgbm
    libva
    libxkbcommon
    mesa
    pipewire
    wayland
  ];

  doCheck = false;

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/MuNeNICK/hypr-rdp";
    license = lib.licenses.mit;
    mainProgram = "hypr-rdp";
    platforms = lib.platforms.linux;
  };
}
