{
  description = "Native RDP server for Hyprland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      supportedSystems = [ "x86_64-linux" ];
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
