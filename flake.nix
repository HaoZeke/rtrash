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
            for n in rtrash trash trash-put trash-empty trash-list trash-restore trash-rm; do
              if [ "$n" != "rtrash" ]; then
                ln -s rtrash $out/bin/$n
              fi
              installShellCompletion --cmd $n \
                --bash <($out/bin/rtrash completions bash) \
                --zsh <($out/bin/rtrash completions zsh) \
                --fish <($out/bin/rtrash completions fish)
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
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
          ];
        };
      }
    );
}
