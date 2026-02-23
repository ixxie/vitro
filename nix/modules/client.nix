{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.vitro.client;
  registryDir = "${config.users.users.${cfg.user}.home}/.config/vitro";

in {
  options.vitro.client = {
    enable = mkEnableOption "vitro client (manages server registry and local DNS)";

    user = mkOption {
      type = types.str;
      description = "User to install vitro config for";
    };

    servers = mkOption {
      type = types.attrsOf types.str;
      default = {};
      description = "Server registry (name = SSH target, e.g. grove = \"root@1.2.3.4\")";
      example = {
        grove = "root@95.216.229.121";
      };
    };

    vmConfig = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Env base config directory for localhost (contains flake.nix exporting nixosModule)";
    };

    server = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Default server for vitro envs (overridden by repo config or --server flag)";
      example = "grove";
    };

    sync = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Files/directories to sync into remote envs (e.g. ~/.claude.json)";
      example = ["~/.claude.json" "~/.claude"];
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [pkgs.age];

    # Write server registry and deploy localhost vm-config
    system.activationScripts.vitro-client = let
      registry = concatStringsSep "\n" (mapAttrsToList (name: target: ''
        [${name}]
        target = "${target}"
      '') cfg.servers);
    in ''
      mkdir -p "${registryDir}"
      chown ${cfg.user}: "${registryDir}"
      cat > "${registryDir}/servers.toml" << 'REGISTRY'
      ${registry}
      REGISTRY
      chown ${cfg.user}: "${registryDir}/servers.toml"

      ${optionalString (cfg.vmConfig != null) ''
        rm -rf /var/lib/vitro/vm-config
        cp -rL "${cfg.vmConfig}" /var/lib/vitro/vm-config
        chmod -R a+rX /var/lib/vitro/vm-config
      ''}

      ${let
        hasConfig = cfg.sync != [] || cfg.server != null;
        syncLine = optionalString (cfg.sync != []) (let
          syncToml = concatStringsSep ", " (map (s: ''"${s}"'') cfg.sync);
        in "sync = [${syncToml}]");
        serverLine = optionalString (cfg.server != null) ''server = "${cfg.server}"'';
      in optionalString hasConfig ''
        cat > "${registryDir}/config.toml" << 'CLIENTCFG'
        ${serverLine}
        ${syncLine}
        CLIENTCFG
        chown ${cfg.user}: "${registryDir}/config.toml"
      ''}
    '';

    # Make /etc/hosts writable so `vitro tunnel` can add .env entries at runtime.
    # NixOS normally symlinks this to the nix store (read-only).
    # Setting a mode copies it instead, making it mutable until the next rebuild.
    environment.etc.hosts.mode = "0644";

    # Ensure runtime dir exists
    systemd.tmpfiles.rules = [
      "d /run/vitro 0755 root root -"
    ];

    # Sudo rules for tunnel management
    security.sudo.extraRules = [
      {
        users = [cfg.user];
        commands = [
          { command = "${pkgs.iproute2}/bin/ip addr add 127.* dev lo"; options = ["NOPASSWD"]; }
          { command = "${pkgs.iproute2}/bin/ip addr del 127.* dev lo"; options = ["NOPASSWD"]; }
          { command = "${pkgs.vitro}/bin/vitro util hosts *"; options = ["NOPASSWD"]; }
        ];
      }
    ];
  };
}
