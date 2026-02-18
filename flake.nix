{
  description = "ssh-agent-switcher";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    flakebox.url = "github:rustshop/flakebox";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      flakebox,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        projectName = "ssh-agent-switcher";

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [ ".#ci.${projectName}" ];
            just.rules.watch.enable = false;
          };
        };

        buildPaths = [
          "Cargo.toml"
          "Cargo.lock"
          "src"
          "tests"
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
        };

        multiBuild = (flakeboxLib.craneMultiBuild { }) (
          craneLib':
          let
            craneLib = (
              craneLib'.overrideArgs {
                pname = projectName;
                src = buildSrc;
                nativeBuildInputs = [ ];
              }
            );
          in
          rec {
            workspaceDeps = craneLib.buildWorkspaceDepsOnly { };

            workspace = craneLib.buildWorkspace {
              cargoArtifacts = workspaceDeps;
            };

            tests = craneLib.cargoNextest {
              cargoArtifacts = workspace;
              # Integration tests spawn the binary which needs HOME/XDG for defaults
              preCheck = ''
                export HOME="$TMPDIR/home"
                mkdir -p "$HOME"
              '';
            };

            clippy = craneLib.cargoClippy {
              cargoArtifacts = workspaceDeps;
            };

            ${projectName} = craneLib.buildPackage {
              cargoArtifacts = workspaceDeps;
            };
          }
        );
      in
      {
        packages.default = multiBuild.${projectName};

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells {
          packages = [
            pkgs.treefmt
          ];
        };
      }
    );
}
