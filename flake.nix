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
              pkgs.bash
              pkgs.coreutils
              pkgs.gitMinimal
              pkgs.gnused
              pkgs.openssh
              pkgs.rsync
            ];
            # Guest simulations need store tools and a usable login shell.
            # Use Bash's readonly BASHOPTS for the rejected-assignment fixture;
            # newer Dash accepts the nonnumeric OPTIND rejected by guest Dash.
            postPatch = ''
              substituteInPlace src/backend.rs \
                --replace-fail 'let mut guest = Command::new("/bin/sh");' \
                  'let mut guest = Command::new("${pkgs.bash}/bin/bash");' \
                --replace-fail 'guest.env_clear();' \
                  'guest.env_clear().env("SHELL", "${pkgs.bash}/bin/bash");' \
                --replace-fail 'guest.arg("-c").arg(ssh.get_args().last().unwrap());' \
                  'guest.arg("-c").arg(ssh.get_args().last().unwrap().to_string_lossy().replacen("/bin/sh -c", "${pkgs.bash}/bin/bash -c", 1));' \
                --replace-fail '"/usr/bin/env -0"' '"${pkgs.coreutils}/bin/env -0"' \
                --replace-fail '.env("SHELL", "/bin/bash")' '.env("SHELL", "${pkgs.bash}/bin/bash")' \
                --replace-fail '"/bin/cat; exit 37"' '"${pkgs.coreutils}/bin/cat; exit 37"' \
                --replace-fail 'OPTIND' 'BASHOPTS' \
                --replace-fail "The guest's dash shell treats BASHOPTS as numeric." \
                  "The fixture's Bash shell treats BASHOPTS as readonly."
              substituteInPlace src/commands/lifecycle.rs \
                --replace-fail '"/usr/bin/printenv PATH"' '"${pkgs.coreutils}/bin/printenv PATH"' \
                --replace-fail '.envs(ssh.get_envs().map(|(name, value)| (name, value.unwrap())))' \
                  '.env("SHELL", "${pkgs.bash}/bin/bash").envs(ssh.get_envs().map(|(name, value)| (name, value.unwrap())))' \
                --replace-fail '#!/bin/bash' '#!${pkgs.bash}/bin/bash' \
                --replace-fail 'exec /bin/cat --' 'exec ${pkgs.coreutils}/bin/cat --'
              substituteInPlace src/creation_hooks.rs \
                --replace-fail 'SHELL=/bin/bash /bin/sh -c' \
                  'SHELL=${pkgs.bash}/bin/bash ${pkgs.bash}/bin/bash -c' \
                --replace-fail 'r#"#!/bin/bash' 'r#"#!${pkgs.bash}/bin/bash' \
                --replace-fail '/guest-bin:/usr/bin:/bin' '/guest-bin:${pkgs.bash}/bin:/usr/bin:/bin'
              substituteInPlace src/ssh.rs \
                --replace-fail '/usr/bin/sed' '${pkgs.gnused}/bin/sed' \
                --replace-fail 'SHELL=/bin/bash /bin/sh -c' \
                  'SHELL=${pkgs.bash}/bin/bash ${pkgs.bash}/bin/bash -c'
              # The Linux sandbox has no /bin/mkdir for the limactl shim.
              substituteInPlace src/lima.rs \
                --replace-fail '/bin/mkdir' '${pkgs.coreutils}/bin/mkdir'
              # Shutdown fixtures clear PATH to test a missing SSH client.
              substituteInPlace src/vm.rs \
                --replace-fail '/bin/sleep' '${pkgs.coreutils}/bin/sleep' \
                --replace-fail '/bin/cat' '${pkgs.coreutils}/bin/cat' \
                --replace-fail '/bin/rm' '${pkgs.coreutils}/bin/rm'
            '';
            # CMake builds aws-lc-sys through Cargo, not the top-level project.
            dontUseCmakeConfigure = true;

            # The SSH and TLS unit tests bind loopback listeners.
            __darwinAllowLocalNetworking = true;
            # Private-storage preparation creates the Lima home on macOS; keep
            # it out of the unwritable /homeless-shelter.
            preCheck = ''
              export LIMA_HOME="$TMPDIR/lima"
            '';
            checkFlags =
              pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                # APFS rejects the invalid UTF-8 name before this test can
                # exercise coop's path validation. Keep it enabled on Linux.
                "--skip=commands::lifecycle::tests::check_reprovision_workspace_source_rejects_a_non_utf8_workspace_dir"
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                # These tests remain enabled in ordinary Linux cargo test runs.
                # The PID fixture renames sleep's argv[0], which breaks nixpkgs'
                # multicall coreutils. Its probes also require privileged sudo,
                # which is unavailable in the Nix build sandbox.
                "--skip=config::tests::is_running_true_for_live_firecracker_like_pid"
                "--skip=config::tests::probe_liveness_recognizes_running_firecracker"
                # This CLI test also invokes the privileged socket probe via sudo.
                "--skip=stop_retains_proxy_for_full_socket_queue_and_cleans_after_close"

                # Nix's Linux syscall filter rejects setxattr with ENOTSUP, so
                # the ACL fixtures fail even on ACL-capable filesystems.
                "--skip=vm::tests::pid_trampoline_normalizes_inherited_default_acl"
                "--skip=private_storage::tests::removes_access_and_inherited_acls"
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

              # Uninstall must refuse a store binary before purging any data,
              # even with --yes/--purge and through the macOS wrapper.
              uninstallCheckDir="$TMPDIR/coop-uninstall-check"
              mkdir -p "$uninstallCheckDir/data"
              printf 'data_dir = "%s/data"\n' "$uninstallCheckDir" > "$uninstallCheckDir/config.toml"
              printf 'keep me\n' > "$uninstallCheckDir/data/sentinel"
              for flags in "" "--yes" "--yes --purge" "--yes --keep-data"; do
                if "$out/bin/coop" --config "$uninstallCheckDir/config.toml" uninstall $flags \
                  < /dev/null > "$uninstallCheckDir/uninstall.log" 2>&1; then
                  echo "uninstall unexpectedly accepted a Nix-store binary" >&2
                  exit 1
                fi
                grep -F 'Cannot uninstall a Nix-managed binary' "$uninstallCheckDir/uninstall.log"
                grep -F 'nix profile remove coop' "$uninstallCheckDir/uninstall.log"
                if grep -F 'sudo coop uninstall' "$uninstallCheckDir/uninstall.log"; then
                  echo "uninstall suggested sudo for a Nix-store binary" >&2
                  exit 1
                fi
                test "$(cat "$uninstallCheckDir/data/sentinel")" = 'keep me'
                test -x "$out/bin/coop"
              done
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              runtimeCheckDir="$TMPDIR/coop-runtime-check"
              mkdir -p "$runtimeCheckDir/state"
              printf 'data_dir = "%s/state"\n' "$runtimeCheckDir" > "$runtimeCheckDir/config.toml"
              # Stop at key generation, before any VM or image is created.
              # Private-storage preparation rejects symlinked state, so block the
              # public-key path instead: ssh-keygen fails when saving its key.
              mkdir "$runtimeCheckDir/state/vm_key.pub"
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
