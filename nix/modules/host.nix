{
  config,
  lib,
  pkgs,
  inputs,
  ...
}:
with lib; let
  cfg = config.vitro.server;
  listenHost = if cfg.proxy.listenHost != null then cfg.proxy.listenHost else cfg.bridge.address;

  hostConfig = builtins.toJSON {
    bridge = {
      inherit (cfg.bridge) name address subnet;
    };
    proxy = {
      inherit (cfg.proxy) httpPort gitCredentialPort controlPort logFile;
    };
    user = {
      inherit (cfg.user) name uid authorizedKeys;
    };
    vm = {
      inherit (cfg.vm) vcpu mem varSize;
    };
  };

  proxyConfig = builtins.toJSON {
    cells = [];
    egress = {
      reads = {
        methods = cfg.egress.reads.methods;
        allowed = cfg.egress.reads.allowed;
        denied = cfg.egress.reads.denied;
      };
      writes = {
        methods = cfg.egress.writes.methods;
        allowed = cfg.egress.writes.allowed;
        denied = cfg.egress.writes.denied;
      };
      credentials = cfg.egress.credentials;
      passthrough = cfg.egress.passthrough;
    };
    httpPort = cfg.proxy.httpPort;
    gitCredentialPort = cfg.proxy.gitCredentialPort;
    controlPort = cfg.proxy.controlPort;
    logFile = cfg.proxy.logFile;
    bindAddress = listenHost;
    sweepTimeout = cfg.sweep.timeout;
    sweepInterval = cfg.sweep.interval;
  };
in {
  imports = [
    ./options.nix
  ];

  config = mkIf cfg.enable {
    systemd.network.wait-online.enable = false;

    # Bridge network
    systemd.network = {
      enable = true;
      netdevs."10-cellbr" = {
        netdevConfig = {
          Name = cfg.bridge.name;
          Kind = "bridge";
        };
      };
      networks."10-cellbr" = {
        matchConfig.Name = cfg.bridge.name;
        networkConfig = {
          Address = "${cfg.bridge.address}/24";
          DHCPServer = false;
          ConfigureWithoutCarrier = true;
          DNS = [cfg.bridge.address];
          Domains = ["~cell"];
        };
        linkConfig.RequiredForOnline = "no";
      };
      # Attach VM tap devices to bridge
      networks."11-microvm" = {
        matchConfig.Name = "vm-*";
        networkConfig.Bridge = cfg.bridge.name;
      };
    };

    # NAT for proxy outbound
    networking.nat = {
      enable = true;
      internalInterfaces = [cfg.bridge.name];
      externalInterface = lib.mkIf (cfg.nat.interface != "auto") cfg.nat.interface;
    };

    boot.kernel.sysctl."net.ipv4.ip_forward" = 1;

    # nftables: cells can ONLY reach the proxy
    networking.nftables = {
      enable = true;
      tables.vitro = {
        family = "inet";
        content = ''
          chain forward {
            type filter hook forward priority 0; policy drop;

            ct state established,related accept

            # cells -> proxy HTTP
            iifname "${cfg.bridge.name}" ip daddr ${cfg.bridge.address} tcp dport ${toString cfg.proxy.httpPort} accept
            # cells -> proxy git-credential
            iifname "${cfg.bridge.name}" ip daddr ${cfg.bridge.address} tcp dport ${toString cfg.proxy.gitCredentialPort} accept
            # cells -> control API (for flow rule updates)
            iifname "${cfg.bridge.name}" ip daddr ${cfg.bridge.address} tcp dport ${toString cfg.proxy.controlPort} accept
            # cells -> host SSH
            iifname "${cfg.bridge.name}" ip daddr ${cfg.bridge.address} tcp dport 22 accept

            # host -> cells (host is trusted)
            ip saddr ${cfg.bridge.address} oifname "${cfg.bridge.name}" accept

            # DROP everything else from cells
            iifname "${cfg.bridge.name}" drop

            # proxy (host) -> internet
            ${if cfg.nat.interface == "auto"
              then "oifname != \"${cfg.bridge.name}\" accept"
              else "oifname \"${cfg.nat.interface}\" accept"}
          }

          chain input {
            type filter hook input priority 0; policy accept;
            iifname "${cfg.bridge.name}" ip daddr ${cfg.bridge.address} tcp dport { ${toString cfg.proxy.httpPort}, ${toString cfg.proxy.gitCredentialPort}, ${toString cfg.proxy.controlPort} } accept
          }
        '';
      };
    };

    networking.firewall.trustedInterfaces = [cfg.bridge.name];

    # DNS for VMs — branch names resolve to VM IPs
    services.dnsmasq = {
      enable = true;
      settings = {
        interface = cfg.bridge.name;
        bind-interfaces = true;
        listen-address = cfg.bridge.address;
        server = ["1.1.1.1" "1.0.0.1"];
        no-resolv = true;
        no-dhcp-interface = cfg.bridge.name;
        addn-hosts = "/var/lib/vitro/dns-hosts";
      };
    };

    # Generate a host SSH key for server-side VM access.
    # The public key is included in the host config so VMs authorize it.
    systemd.services.vitro-hostkey = {
      description = "Generate vitro host SSH key";
      wantedBy = ["multi-user.target"];
      before = ["vitro-services.service"];
      serviceConfig.Type = "oneshot";
      serviceConfig.RemainAfterExit = true;
      script = ''
        KEY=/var/lib/vitro/ssh/id_ed25519
        if [ ! -f "$KEY" ]; then
          mkdir -p /var/lib/vitro/ssh
          ${pkgs.openssh}/bin/ssh-keygen -t ed25519 -f "$KEY" -N "" -C "vitro-host"
        fi
        # symlink into root's .ssh so all server-side SSH commands use it
        mkdir -p /root/.ssh
        ln -sf "$KEY" /root/.ssh/id_ed25519
        ln -sf "$KEY.pub" /root/.ssh/id_ed25519.pub
      '';
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/vitro 0755 root root -"
      "d /var/lib/vitro/ca 0755 root root -"
      "d /var/lib/vitro/cells 0755 root root -"
      "d /var/lib/vitro/copyfiles 0755 root root -"
      "d /var/log/vitro 0755 root root -"
      "f /var/lib/vitro/dns-hosts 0666 root root -"
      "f /var/lib/vitro/ip-pool.json 0644 root root -"
    ];

    # Host config JSON — read by mkCell at nix eval time
    environment.etc."vitro/host-config.json".text = hostConfig;

    # Allow git operations on cell repos (owned by uid 1000, accessed by root via SSH/vitro-services).
    # Scoped to the cell tree — host code never touches git outside this prefix.
    programs.git.enable = true;
    programs.git.config.safe.directory = ["/var/lib/vitro/cells/*"];


    # Proxy config
    environment.etc."vitro/proxy-config.json".text = proxyConfig;

    # Stage copyFiles into the shared directory
    systemd.services.vitro-copyfiles = mkIf (cfg.vm.copyFiles != {}) {
      description = "Stage files for vitro VMs";
      wantedBy = ["multi-user.target"];
      serviceConfig.Type = "oneshot";
      script = let
        copies = lib.concatStringsSep "\n" (lib.mapAttrsToList (src: dst: ''
            mkdir -p "$(dirname "/var/lib/vitro/copyfiles/${dst}")"
            cp -f "${src}" "/var/lib/vitro/copyfiles/${dst}"
          '')
          cfg.vm.copyFiles);
      in
        copies;
    };

    # MITM proxy (mitmproxy in regular mode — redsocks in guest handles transparency)
    systemd.services.vitro-mitmproxy = {
      description = "Vitro MITM Proxy";
      wantedBy = ["multi-user.target"];
      after = ["network.target"];
      serviceConfig = {
        ExecStart = "${pkgs.mitmproxy}/bin/mitmdump --listen-host ${listenHost} --listen-port ${toString cfg.proxy.httpPort} --set confdir=/var/lib/vitro/ca -s ${./proxy}/vitro_addon.py";
        Restart = "always";
        RestartSec = 5;
        EnvironmentFile = ["-/var/lib/vitro/secrets.env"];
        ReadWritePaths = ["/var/log/vitro" "/var/lib/vitro/ca" "/var/lib/vitro"];
      };
    };

    # Sync the public CA cert from mitmproxy's combined key+cert file so
    # guests always trust the same key mitmproxy actually signs with
    systemd.services.vitro-ca-sync = {
      description = "Sync mitmproxy CA public cert";
      wantedBy = ["multi-user.target"];
      after = ["vitro-mitmproxy.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        CA=/var/lib/vitro/ca/mitmproxy-ca.pem
        for i in $(seq 1 30); do
          [ -f "$CA" ] && break
          sleep 1
        done
        [ -f "$CA" ] || exit 1
        ${pkgs.openssl}/bin/openssl x509 -in "$CA" -out /var/lib/vitro/ca/mitmproxy-ca-cert.pem
      '';
    };

    # Vitro services (git credentials + control API)
    systemd.services.vitro-services = {
      description = "Vitro Git Credentials + Control API";
      wantedBy = ["multi-user.target"];
      after = ["network.target"];
      path = [pkgs.git pkgs.sudo pkgs.util-linux pkgs.systemd pkgs.openssh pkgs.curl pkgs.nix pkgs.age];
      serviceConfig = {
        ExecStart = "${pkgs.vitro}/bin/vitro server proxy --config /etc/vitro/proxy-config.json";
        Restart = "always";
        RestartSec = 5;
        EnvironmentFile = ["-/var/lib/vitro/secrets.env"];
      };
    };

    # Optional GC timer for stopped cells
    systemd.services.vitro-gc = mkIf cfg.gc.enable {
      description = "Vitro garbage collection";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.vitro}/bin/vitro server gc --older-than ${cfg.gc.olderThan}";
      };
      path = [pkgs.vitro];
    };

    systemd.timers.vitro-gc = mkIf cfg.gc.enable {
      description = "Periodic vitro garbage collection";
      wantedBy = ["timers.target"];
      timerConfig = {
        OnCalendar = cfg.gc.interval;
        Persistent = true;
      };
    };

    # Scoped sudo for VM management
    security.sudo.extraRules = [
      {
        users = ["root"];
        commands = [
          {
            command = "/run/current-system/sw/bin/systemctl start microvm@*";
            options = ["NOPASSWD"];
          }
          {
            command = "/run/current-system/sw/bin/systemctl stop microvm@*";
            options = ["NOPASSWD"];
          }
        ];
      }
    ];
  };
}
