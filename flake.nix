{
  description = "Native RDP server for Hyprland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      # aarch64-linux added (2026-11-21) for the jules-asahi MacBook Pro
      # (Apple Silicon, see nonsleepr/nix-config's hosts/jules-asahi) --
      # nothing in this flake or pkg/nix/package.nix is x86-specific (plain
      # rustPlatform.buildRustPackage against stock nixpkgs rustc; the
      # dependency crates that looked riskiest -- ironrdp-*, aws-lc-rs,
      # zstd, openh264-sys2 -- all have confirmed aarch64-linux support:
      # openh264-sys2's build.rs compiles NEON asm on aarch64 instead of
      # invoking the x86-only NASM path, aws-lc explicitly ships
      # hand-optimized ARM asm, and Devolutions' own commercial RDP
      # Gateway product (built on these same IronRDP crates) already
      # ships native ARM64 Docker images.
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = lib.genAttrs supportedSystems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          hypr-rdp = pkgs.callPackage ./pkg/nix/package.nix { };
          default = self.packages.${system}.hypr-rdp;
        }
      );

      apps = forAllSystems (system: {
        hypr-rdp = {
          type = "app";
          program = "${self.packages.${system}.hypr-rdp}/bin/hypr-rdp";
        };
        default = self.apps.${system}.hypr-rdp;
      });

      checks = forAllSystems (system: {
        hypr-rdp = self.packages.${system}.hypr-rdp;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.hypr-rdp ];

            packages = with pkgs; [
              cargo
              clippy
              pulseaudio
              rustc
              rustfmt
            ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);
    };
}
