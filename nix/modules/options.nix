{lib, ...}:
with lib; {
  options.vitro.server = {
    enable = mkEnableOption "Vitro server (sandboxed microVMs)";

    nat.interface = mkOption {
      type = types.str;
      default = "auto";
      description = "External network interface for NAT. Use \"auto\" to masquerade on all interfaces.";
    };

    bridge = {
      name = mkOption {
        type = types.str;
        default = "vitrobr";
      };
      address = mkOption {
        type = types.str;
        default = "192.168.83.1";
      };
      subnet = mkOption {
        type = types.str;
        default = "192.168.83.0/24";
      };
    };

    proxy = {
      httpPort = mkOption {
        type = types.port;
        default = 8080;
      };
      gitCredentialPort = mkOption {
        type = types.port;
        default = 8081;
      };
      logFile = mkOption {
        type = types.str;
        default = "/var/log/vitro/proxy.log";
      };
      controlPort = mkOption {
        type = types.port;
        default = 8082;
        description = "Control API port (localhost only)";
      };
      listenHost = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Override the proxy's listen address. Defaults to bridge.address.
          Set to "127.0.0.1" or "0.0.0.0" in nixosTest contexts where the
          bridge is not present.
        '';
      };
    };


    egress = {
      reads = {
        methods = mkOption {
          type = types.listOf types.str;
          default = ["GET" "HEAD" "OPTIONS"];
          description = "HTTP methods classified as reads";
        };
        allowed = mkOption {
          type = types.either types.str (types.listOf types.str);
          default = "*";
          description = "Allowed domains for read methods. Use \"*\" for all.";
        };
        denied = mkOption {
          type = types.listOf types.str;
          default = [];
          description = "Denied domains for read methods (overrides allowed)";
        };
      };
      writes = {
        methods = mkOption {
          type = types.listOf types.str;
          default = ["POST" "PUT" "PATCH" "DELETE"];
          description = "HTTP methods classified as writes";
        };
        allowed = mkOption {
          type = types.listOf types.str;
          default = [
            "github.com"
            "*.github.com"
            "*.githubusercontent.com"
            "registry.npmjs.org"
            "*.npmjs.org"
            "pypi.org"
            "*.pypi.org"
            "files.pythonhosted.org"
            "cache.nixos.org"
            "*.cachix.org"
          ];
          description = "Allowed domains for write methods";
        };
        denied = mkOption {
          type = types.either types.str (types.listOf types.str);
          default = "*";
          description = "Denied domains for write methods. Use \"*\" for all.";
        };
      };
      passthrough = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Domains to pass through without TLS interception (e.g. for OAuth)";
        example = ["claude.ai" "*.anthropic.com"];
      };
      credentials = mkOption {
        type = types.listOf (types.submodule {
          options = {
            host = mkOption {type = types.str;};
            header = mkOption {type = types.str;};
            envVar = mkOption {type = types.str;};
          };
        });
        default = [
          {
            host = "api.github.com";
            header = "Authorization";
            envVar = "GITHUB_TOKEN_HEADER";
          }
        ];
      };
    };

    vm = {
      vcpu = mkOption {
        type = types.int;
        default = 4;
      };
      mem = mkOption {
        type = types.int;
        default = 4096;
      };
      varSize = mkOption {
        type = types.int;
        default = 4096;
      };
      copyFiles = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Host files to copy into guest on boot (key = host path, value = guest path)";
      };
      mounts = mkOption {
        type = types.attrsOf (types.submodule {
          options = {
            mountPoint = mkOption {type = types.str;};
            readOnly = mkOption {
              type = types.bool;
              default = false;
            };
          };
        });
        default = {};
        description = "Host paths to mount into vm VMs (key = source path)";
      };
    };

    user = {
      name = mkOption {
        type = types.str;
        default = "agent";
      };
      uid = mkOption {
        type = types.int;
        default = 1000;
      };
      authorizedKeys = mkOption {
        type = types.listOf types.str;
        default = [];
      };
    };

    sweep = {
      timeout = mkOption {
        type = types.int;
        default = 21600;
        description = "Server-side sweep timeout in seconds. Stops VMs where the current op exceeds this (default: 6h).";
      };
      interval = mkOption {
        type = types.int;
        default = 300;
        description = "How often to run the sweep check in seconds (default: 5m).";
      };
    };

    gc = {
      enable = mkEnableOption "Automatic garbage collection of stopped envs";
      interval = mkOption {
        type = types.str;
        default = "daily";
        description = "systemd calendar expression for GC runs (e.g. \"daily\", \"hourly\", \"*-*-* 03:00:00\").";
      };
      olderThan = mkOption {
        type = types.str;
        default = "7d";
        description = "Delete stopped envs older than this duration (e.g. \"7d\", \"24h\").";
      };
    };
  };
}
