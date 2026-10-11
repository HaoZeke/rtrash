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
          postInstall = pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
            for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
              ln -s rtrash $out/bin/$n
            done
            installShellCompletion --cmd rtrash \
              --bash <($out/bin/rtrash completions bash) \
              --zsh <($out/bin/rtrash completions zsh) \
              --fish <($out/bin/rtrash completions fish)
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
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
          ];
        };
      }
    );
}
