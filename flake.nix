{
  inputs = {
    nixpkgs = {
      #url = "github:NixOS/nixpkgs/nixos-unstable";
      url = "github:NixOS/nixpkgs/nixos-26.05";
    };

    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      microvm,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # vsock port of the host HTTP proxy, shared with src/main.rs
      proxyPort = 3128;

      # vsock port where the host serves forwarded environment variables
      envPort = 3129;

      workDir = "/work";

      # Writable nix store layer; the file name is shared with src/main.rs
      storeImage = "nix-store-overlay.img";

      userName = "agentvm";

      # workaround: cannot mount workdir as user home, since claude code refuse tu trust homedir
      userHome = "${workDir}/.${userName}";

      # Tools available in the guest, both system-wide and on the session's PATH.
      # Takes the module's pkgs (not the outer one) so allowUnfreePredicate applies.
      guestPackages =
        pkgs: with pkgs; [
          devenv
          ripgrep
          jq
          yq-go
          eza
          file
          which
          tree
          gnused
          gnutar
          gnumake
          gawk
          zstd
          zip
          xz
          unzip
          socat
          claude-code
        ];

      # Define the MicroVM Guest System using nixosSystem + microvm module
      mkGuestVm =
        { vcpu, mem, storeSize }:
        nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            microvm.nixosModules.microvm
            ({ pkgs, lib, ... }: {
              system.stateVersion = "26.11";

              nixpkgs.config.allowUnfreePredicate = pkg: builtins.elem (lib.getName pkg) [ "claude-code" ];

              networking.hostName = "agentvm";

              users.users.${userName} = {
                isNormalUser = true;
                home = "${userHome}";
                group = "${userName}";
                uid = 1000;
                createHome = false;
              };

              users.groups.${userName} = {
                gid = 1000;
              };

              environment.systemPackages = guestPackages pkgs;

              nix.settings = {
                experimental-features = [ "nix-command" "flakes" ];
                trusted-users = [ userName ];
              };
              # The daemon downloads substitutes itself, so it needs the proxy too
              systemd.services.nix-daemon.environment = {
                HTTP_PROXY = "http://127.0.0.1:${toString proxyPort}";
                HTTPS_PROXY = "http://127.0.0.1:${toString proxyPort}";
              };

              # The session runs directly on the virtio console, which cloud-hypervisor
              # attaches to the user's terminal; the VM powers off when it exits.
              boot.kernelModules = [ "virtio_console" ];
              systemd.services."serial-getty@hvc0".enable = false;
              systemd.services.agentvm-session = {
                description = "agentvm session on the host terminal";
                wantedBy = [ "multi-user.target" ];
                after = [
                  "systemd-user-sessions.service"
                  "vsock-proxy.service"
                ];
                # The guest has no network interface; all traffic goes through
                # the host's HTTP proxy, tunnelled over vsock.
                environment = rec {
                  TERM = "xterm-256color";
                  # Separate from the project dir: claude only remembers folder trust
                  # for non-home directories, and ~/.claude must not clash with the
                  # project's own .claude/
                  HOME = "${userHome}";
                  HTTP_PROXY = "http://127.0.0.1:${toString proxyPort}";
                  HTTPS_PROXY = HTTP_PROXY;
                  http_proxy = HTTP_PROXY;
                  https_proxy = HTTP_PROXY;
                };
                path = guestPackages pkgs;
                script = ''
                  # Import host environment (e.g. ANTHROPIC_API_KEY) and claude arguments
                  eval "$(socat -u VSOCK-CONNECT:2:${toString envPort} -)"
                  mkdir -p "$HOME"
                  echo "🚀 Launching Claude Code..."
                  if [[ -f ./devenv.nix ]]; then
                    exec devenv shell claude "$@"
                  else
                    exec claude "$@"
                  fi
                '';
                serviceConfig = {
                  Type = "simple";
                  User = "${userName}";
                  WorkingDirectory = "${workDir}";
                  StandardInput = "tty-force";
                  StandardOutput = "tty";
                  StandardError = "tty";
                  TTYPath = "/dev/hvc0";
                  TTYReset = true;
                  TTYVHangup = true;
                  ExecStopPost = "+${pkgs.systemd}/bin/systemctl poweroff --no-block";
                };
              };

              # Guest -> host: HTTP proxy over vsock (host CID is 2)
              systemd.services.vsock-proxy = {
                description = "Forward localhost:${toString proxyPort} to host proxy over vsock";
                wantedBy = [ "multi-user.target" ];
                serviceConfig = {
                  ExecStart = "${pkgs.socat}/bin/socat TCP-LISTEN:${toString proxyPort},bind=127.0.0.1,fork,reuseaddr VSOCK-CONNECT:2:${toString proxyPort}";
                  Restart = "always";
                };
              };

              microvm = {
                hypervisor = "cloud-hypervisor";
                inherit vcpu mem;
                # virtiofsd is started by the agentvm CLI with the real directory;
                # the runner only connects to ./agentvm-virtiofs-work.sock.
                shares = [
                  {
                    tag = "work";
                    proto = "virtiofs";
                    source = "/tmp/placeholder";
                    mountPoint = "${workDir}";
                  }
                ];
                # devenv/nix must be able to add store paths: overlay a writable layer
                # on the read-only store image. The layer is persisted on an ext4 volume;
                # the agentvm CLI creates the image in the project (.agentvm/.cache/devenv)
                # and symlinks it to this name in the runner's working directory.
                writableStoreOverlay = "/nix/.rw-store";
                volumes = [
                  {
                    image = storeImage;
                    mountPoint = "/nix/.rw-store";
                    size = storeSize;
                  }
                ];
                # The runner exposes this as ./notify.vsock
                vsock.cid = 3;
                # virtio console on the user's terminal; kernel/boot log to a file
                cloud-hypervisor.extraArgs = [
                  "--console"
                  "tty"
                  "--serial"
                  "file=console.log"
                ];
              };
            })
          ];
        };

      # 2. Package and wrap the Rust CLI, baking in dependencies and the VM runner.
      #    Settings are fixed per build:
      #    pkgs.agentvm.config { vcpu = 4; mem = 4096; storeSize = 20480; forwardEnv = [ "ANTHROPIC_API_KEY" ]; }
      mkagentvm =
        {
          vcpu ? 2,
          mem ? 2048,
          # size of the persistent nix store image, in MiB
          storeSize ? 20480,
          forwardEnv ? [ ],
        }:
        let
          runner = (mkGuestVm { inherit vcpu mem storeSize; }).config.microvm.runner.cloud-hypervisor;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "agentvm";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            wrapProgram $out/bin/agentvm \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.virtiofsd pkgs.e2fsprogs ]} \
              --set AGENTVM_RUNNER "${runner}/bin/microvm-run" \
              --set AGENTVM_STORE_MIB ${toString storeSize} \
              --set AGENTVM_FORWARD_ENV ${pkgs.lib.escapeShellArg (toString forwardEnv)}
          '';

          passthru = {
            inherit runner;
            # Rebuild with different settings; unspecified ones keep their current value
            config = args: mkagentvm ({ inherit vcpu mem forwardEnv storeSize; } // args);
          };
          meta.mainProgram = "agentvm";
        };

      agentvmPkg = mkagentvm { };

    in
    {
      packages.${system} = {
        default = agentvmPkg;
        agentvm = agentvmPkg;
        microvmImage = agentvmPkg.runner;
      };

      apps.${system}.default = {
        type = "app";
        program = "${agentvmPkg}/bin/agentvm";
      };

      # NixOS module:
      #   imports = [ agentvm.nixosModules.default ];
      #   programs.agentvm = { enable = true; mem = 2; vcpu = 2; };
      nixosModules.default =
        { config, lib, ... }:
        let
          cfg = config.programs.agentvm;
        in
        {
          options.programs.agentvm = {
            enable = lib.mkEnableOption "agentvm, Claude Code in a throwaway microVM";

            vcpu = lib.mkOption {
              type = lib.types.ints.positive;
              default = 2;
              description = "Number of virtual CPUs of the guest.";
            };

            mem = lib.mkOption {
              type = lib.types.ints.positive;
              default = 2;
              description = "Guest memory in GB.";
            };

            storeSize = lib.mkOption {
              type = lib.types.ints.positive;
              default = 20;
              description = ''
                Size in GB of the persistent disk holding the guest's nix store layer.
                Only applies when a project's image is first created.
              '';
            };

            forwardEnv = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              example = [ "ANTHROPIC_API_KEY" ];
              description = "Host environment variables forwarded into the guest session.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [
              (self.packages.${system}.agentvm.config {
                inherit (cfg) vcpu forwardEnv;
                mem = cfg.mem * 1024;
                storeSize = cfg.storeSize * 1024;
              })
            ];
          };
        };

      # For NixOS / home-manager configs: nixpkgs.overlays = [ agentvm.overlays.default ];
      # then add pkgs.agentvm to environment.systemPackages or home.packages
      overlays.default = final: prev: {
        agentvm = self.packages.${final.stdenv.hostPlatform.system}.agentvm;
      };
    };
}
