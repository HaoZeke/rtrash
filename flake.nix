{
  description = "Fast freedesktop.org trash tool with an rm-compatible interface";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;
        rtrash = craneLib.buildPackage {
          src = craneLib.path ./.;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.installShellFiles ];
          # The tests drive git, bash completion, script(1) for a terminal
          # and the release script, which makes its zip with zip or python.
          nativeCheckInputs = [
            pkgs.git
            pkgs.bashInteractive
            pkgs.util-linux
            pkgs.zip
          ];
          # The tests use debug-only RTRASH_TEST_* seams, as upstream CI
          # runs them, so the check phase builds the dev profile.
          checkPhaseCargoCommand = "cargo test --locked";
          postInstall = pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
            installShellCompletion --cmd rtrash \
              --bash <($out/bin/rtrash completions bash) \
              --zsh <($out/bin/rtrash completions zsh) \
              --fish <($out/bin/rtrash completions fish)

            for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
              ln -s rtrash "$out/bin/$n"
              ln -s rtrash.bash \
                "$out/share/bash-completion/completions/$n.bash"
              ln -s rtrash.fish \
                "$out/share/fish/vendor_completions.d/$n.fish"
            done

            $out/bin/rtrash man > rtrash.1
            installManPage rtrash.1
          '';
        };
      in
      {
        checks = {
          inherit rtrash;
        };

        packages.default = rtrash;

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = [ pkgs.rust-analyzer ];
        };
      }
    );
}
