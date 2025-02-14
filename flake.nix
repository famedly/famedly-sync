{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    fenix.url = "github:nix-community/fenix";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      outputsForSystems =
        outputs: nixpkgs.lib.foldAttrs (val: col: col // val) { } (builtins.map outputs systems);
    in
    outputsForSystems (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rustToolchain = fenix.packages.${system}.stable;

        cargoNix = nixpkgs.legacyPackages.${system}.callPackage ./Cargo.nix {
          buildRustCrateForPkgs = pkgs: pkgs.buildRustCrate.override { inherit (rustToolchain) rustc cargo; };
          defaultCrateOverrides = pkgs.defaultCrateOverrides // {
            famedly-sync = _: {
              # Override all vergen stats so that the build is
              # actually deterministic
              VERGEN_IDEMPOTENT = "true";

              # Patch the silly `build.rs` that breaks vergen's
              # deterministic build features
              patchPhase = ''
                sed 's/.fail_on_error()//' -i build.rs
              '';
            };
          };
        };
      in
      {
        packages.${system} = {
          famedly-sync = cargoNix.workspaceMembers.famedly-sync.build;

          # We can't use the native testing features since we depend
          # on setting up a test environment with `cargo nextest`. Our
          # tests cannot run in nix' sandbox.
          #
          # We *could* just build the test binaries and then a script
          # which depends on them, but crate2nix carefully hides
          # them. We'll probably need to open an issue about this.
          #
          # TODO(tlater): Find a way to get this to work
          #
          # tests = self.packages.${system}.famedly-sync.override {
          #   runTests = true;
          # };

          container = pkgs.dockerTools.buildImage {
            name = "famedly-sync-agent";

            copyToRoot = [
              self.packages.${system}.famedly-sync

              pkgs.cacert
              pkgs.openssl
            ];

            config = {
              WorkingDir = "/opt/famedly-sync";
              EntryPoint = "/bin/famedly-sync";

              Env = [
                "FAMEDLY_SYNC_CONFIG=/opt/famedly-sync/config.yaml"
              ];
            };
          };
        };
      }
    );
}
