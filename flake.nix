{
  description = "Isolated VM environments for running Claude Code and Codex";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      # Match the native release targets: Lima needs Apple Silicon on macOS.
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      perSystem = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          darwinRuntimeInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.lima
            pkgs.coreutils
            pkgs.curl
            pkgs.gitMinimal
            pkgs.gnutar
            pkgs.openssh
            pkgs.rsync
          ];
          coop = rustPlatform.buildRustPackage {
            pname = "coop";
            inherit (manifest.workspace.package) version;
            src = pkgs.lib.cleanSource self;

            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" ];
            cargoTestFlags = [ "--workspace" ];
            # Port-collision tests release their reservations before probing.
            dontUseCargoParallelTests = true;
            strictDeps = true;

            nativeBuildInputs = [
              pkgs.cmake
              pkgs.makeBinaryWrapper
            ];
            nativeCheckInputs = [
              pkgs.gitMinimal
              pkgs.openssh
            ];
            # CMake builds aws-lc-sys through Cargo, not the top-level project.
            dontUseCmakeConfigure = true;

            # The SSH and TLS unit tests bind loopback listeners.
            __darwinAllowLocalNetworking = true;
            checkFlags =
              pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                # APFS rejects the invalid UTF-8 name before this test can
                # exercise coop's path validation. Keep it enabled on Linux.
                "--skip=commands::lifecycle::tests::check_reprovision_workspace_source_rejects_a_non_utf8_workspace_dir"
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                # These tests remain enabled in ordinary Linux cargo test runs.
                # The PID fixtures rename sleep's argv[0], which breaks nixpkgs'
                # multicall coreutils. Their probes also require privileged sudo,
                # which is unavailable in the Nix build sandbox.
                "--skip=config::tests::is_firecracker_process_true_for_firecracker_named_pid"
                "--skip=config::tests::is_running_true_for_live_firecracker_like_pid"

                # Nix's Linux syscall filter rejects setxattr with ENOTSUP, so
                # the default-ACL fixture fails even on ACL-capable filesystems.
                "--skip=vm::tests::pid_trampoline_normalizes_inherited_default_acl"
              ];

            # Nix owns upgrades of these immutable binaries. Keep the existing
            # dev-build guard against `coop update` and its background notifier.
            COOP_FORCE_BUILD_KIND = "dev";

            # Keep the real executable in bin/ so its sibling lookup still
            # finds coop-proxy after adding Lima and host tools to PATH.
            postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              wrapProgram "$out/bin/coop" \
                --prefix PATH : ${pkgs.lib.makeBinPath darwinRuntimeInputs}
            '';

            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              "$out/bin/coop" --version
              test -x "$out/bin/coop-proxy"
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              runtimeCheckDir="$TMPDIR/coop-runtime-check"
              mkdir -p "$runtimeCheckDir/state"
              printf 'data_dir = "%s/state"\n' "$runtimeCheckDir" > "$runtimeCheckDir/config.toml"
              # Stop at key generation, before any VM or image is created.
              # The dangling link makes ssh-keygen fail when saving its key.
              ln -s missing/key "$runtimeCheckDir/state/vm_key"
              if env PATH= LIMA_HOME="$runtimeCheckDir/lima" \
                "$out/bin/coop" --config "$runtimeCheckDir/config.toml" setup \
                > "$runtimeCheckDir/setup.log" 2>&1; then
                echo "setup unexpectedly passed the key-generation stop point" >&2
                exit 1
              fi
              cat "$runtimeCheckDir/setup.log"
              grep -F '  limactl: ' "$runtimeCheckDir/setup.log"
              grep -F 'ssh-keygen failed' "$runtimeCheckDir/setup.log"
            ''
            + ''
              runHook postInstallCheck
            '';

            meta = {
              inherit (manifest.package) description;
              homepage = manifest.workspace.package.repository;
              license = pkgs.lib.licenses.asl20;
              mainProgram = "coop";
              platforms = systems;
            };
          };
        in
        {
          inherit coop;
          shell = pkgs.mkShell {
            packages = [
              toolchain
              pkgs.cmake
              pkgs.gitMinimal
              pkgs.openssh
              pkgs.python3
            ]
            ++ darwinRuntimeInputs;
          };
          formatter = pkgs.nixfmt;
        }
      );
    in
    {
      packages = forAllSystems (system: {
        default = perSystem.${system}.coop;
        coop = perSystem.${system}.coop;
      });
      checks = forAllSystems (system: {
        coop = perSystem.${system}.coop;
      });
      devShells = forAllSystems (system: {
        default = perSystem.${system}.shell;
      });
      formatter = forAllSystems (system: perSystem.${system}.formatter);
    };
}
