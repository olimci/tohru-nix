{
  description = "Safely materialise Nix-built file trees";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-darwin" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      lib.mkApp =
        { pkgs, ... }@manifest:
        import ./nix/application.nix {
          inherit pkgs;
          runtime = self.packages.${pkgs.stdenv.hostPlatform.system}.tohru;
        } (removeAttrs manifest [ "pkgs" ]);

      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          tohru = pkgs.callPackage ./nix/package.nix { };
          default = self.packages.${system}.tohru;
        });

      checks = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          inherit (self.packages.${system}) tohru;
          application =
            let app = self.lib.mkApp {
              inherit pkgs;
              images.check = {
                root = "/tmp";
                layers = [ { source = pkgs.runCommand "tohru-check-tree" { } ''
                  mkdir -p "$out/empty"
                  printf 'managed\n' > "$out/file"
                ''; } ];
              };
            };
            in pkgs.runCommand "tohru-application-check" { } ''
              ${app.program} --state "$TMPDIR/state" list > "$out"
            '';
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.clippy pkgs.rustc pkgs.rustfmt ];
          };
        });
    };
}
