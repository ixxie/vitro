{
  config,
  lib,
  pkgs,
  vitroHost,
  env,
  ...
}: let
  inherit (vitroHost) bridge proxy user;
  workspace = "/${env.repo}";
in {
  system.stateVersion = "24.11";

  # Networking
  systemd.network.enable = true;
  networking.useNetworkd = true;

  # System-wide proxy
  networking.proxy = {
    httpProxy = "http://${bridge.address}:${toString proxy.httpPort}";
    httpsProxy = "http://${bridge.address}:${toString proxy.httpPort}";
    noProxy = "localhost,127.0.0.1,${bridge.address}";
  };

  # Transparent proxy — redsocks catches apps that ignore proxy env vars
  services.redsocks = {
    enable = true;
    redsocks = [
      {
        port = 12345;
        proxy = "${bridge.address}:${toString proxy.httpPort}";
        type = "http-relay";
        redirectCondition = "--dport 80";
        doNotRedirect = [
          "-d 127.0.0.0/8"
          "-d ${bridge.address}"
        ];
      }
      {
        port = 12346;
        proxy = "${bridge.address}:${toString proxy.httpPort}";
        type = "http-connect";
        redirectCondition = "--dport 443";
        doNotRedirect = [
          "-d 127.0.0.0/8"
          "-d ${bridge.address}"
        ];
      }
    ];
  };

  # Egress firewall
  networking.firewall = {
    enable = true;
    allowedTCPPorts = [22];
    trustedInterfaces = ["enp+"];
    extraCommands = ''
      # Transparent proxy: redirect outbound TCP to redsocks.
      # The ! -d ${bridge.address} exclusion is what prevents the redirect
      # loop: redsocks' own upstream connection IS to the bridge address,
      # so it's never re-redirected.
      iptables -t nat -F OUTPUT
      iptables -t nat -A OUTPUT -p tcp --dport 80 ! -d ${bridge.address} \
        -j REDIRECT --to-port 12345
      iptables -t nat -A OUTPUT -p tcp --dport 443 ! -d ${bridge.address} \
        -j REDIRECT --to-port 12346

      # Filter rules: default-drop egress, allow only loopback, established,
      # and connections to the proxy bridge.
      iptables -P OUTPUT DROP
      iptables -A OUTPUT -o lo -j ACCEPT
      iptables -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
      iptables -A OUTPUT -d ${bridge.address} -j ACCEPT

      # Diagnostic snapshot so the (sudo-less) agent can verify the rules.
      iptables-save > /tmp/vitro-iptables.txt 2>&1 || true
      chmod 644 /tmp/vitro-iptables.txt 2>/dev/null || true
    '';
    extraStopCommands = ''
      iptables -P OUTPUT ACCEPT
      iptables -F OUTPUT
      iptables -t nat -F OUTPUT
    '';
  };

  # SSH
  services.openssh = {
    enable = true;
    hostKeys = [
      {
        path = "/var/ssh-keys/ssh_host_ed25519_key";
        type = "ed25519";
      }
    ];
    settings = {
      ClientAliveInterval = 30;
      ClientAliveCountMax = 3;
    };
  };

  systemd.services.ssh-key-setup = {
    description = "Copy SSH keys from cell mount";
    wantedBy = ["sshd.service"];
    before = ["sshd.service"];
    serviceConfig.Type = "oneshot";
    script = ''
      mkdir -p /var/ssh-keys
      if [ -f ${workspace}/keys/ssh_host_ed25519_key ]; then
        cp ${workspace}/keys/ssh_host_ed25519_key /var/ssh-keys/
        chmod 600 /var/ssh-keys/ssh_host_ed25519_key
      fi

      USER_HOME="/home/${user.name}"
      mkdir -p "$USER_HOME/.ssh"
      # Validate every line before copying — the file is sourced from the
      # mounted repo, so a PR adding garbage (or an extra key) here would
      # otherwise grant SSH access into the cell. ssh-keygen -l -f - parses
      # one public-key line per stdin invocation.
      if [ -f ${workspace}/keys/authorized_keys ] && [ -s ${workspace}/keys/authorized_keys ]; then
        ak_ok=1
        while IFS= read -r line || [ -n "$line" ]; do
          case "$line" in
            ""|\#*) continue ;;
          esac
          if ! printf '%s\n' "$line" | ${pkgs.openssh}/bin/ssh-keygen -l -f - >/dev/null 2>&1; then
            ak_ok=0
            break
          fi
        done < ${workspace}/keys/authorized_keys
        if [ "$ak_ok" = 1 ]; then
          cp ${workspace}/keys/authorized_keys "$USER_HOME/.ssh/authorized_keys"
        else
          echo "ssh-key-setup: refusing to install authorized_keys — invalid public-key line(s)" >&2
        fi
      fi
      chmod 700 "$USER_HOME/.ssh"
      chmod 600 "$USER_HOME/.ssh/authorized_keys" 2>/dev/null || true
      chown -R ${user.name}:users "$USER_HOME/.ssh"
    '';
  };

  # User — no sudo
  users.users.${user.name} = {
    isNormalUser = true;
    uid = user.uid;
    group = "users";
    home = "/home/${user.name}";
    initialHashedPassword = "";
    openssh.authorizedKeys.keys = user.authorizedKeys;
  };

  # vitro run state directory
  systemd.tmpfiles.rules = [
    "d /var/lib/vitro 0755 ${user.name} users -"
    "d /tmp/vitro 0755 ${user.name} users -"
  ];

  services.getty.autologinUser = user.name;

  environment = {
    enableAllTerminfo = true;

    systemPackages = with pkgs; [
      git
      curl
      jq
    ];

    variables = {
      SSL_CERT_FILE = "/etc/ssl/vitro-ca-bundle.crt";
      NIX_SSL_CERT_FILE = "/etc/ssl/vitro-ca-bundle.crt";
      CURL_CA_BUNDLE = "/etc/ssl/vitro-ca-bundle.crt";
      # node/bun ignore SSL_CERT_FILE; this is the env var they read.
      NODE_EXTRA_CA_CERTS = "/etc/ssl/vitro-ca-bundle.crt";
    };

    etc."gitconfig".text = ''
      [safe]
        directory = *
    '';

  };

  # Netfilter modules — needed for the transparent-proxy REDIRECT rules
  # in the firewall. The microvm kernel doesn't auto-load these, so without
  # them the iptables rules install but don't fire (packets bypass NAT).
  boot.kernelModules = [
    "nf_nat"
    "nf_nat_redirect"
    "nf_conntrack"
    "xt_REDIRECT"
    "xt_owner"
  ];

  # Kernel hardening
  boot.kernel.sysctl = {
    "kernel.dmesg_restrict" = 1;
    "kernel.sysrq" = 0;
    "kernel.yama.ptrace_scope" = 2;
    "kernel.kptr_restrict" = 2;
    # Allow REDIRECT targets to route to loopback from non-loopback contexts.
    "net.ipv4.conf.all.route_localnet" = 1;
  };

  boot.tmp = {
    useTmpfs = true;
    tmpfsSize = "1G";
  };

  nix.settings.experimental-features = ["nix-command" "flakes"];

  # Trust the mitmproxy CA
  systemd.services.vitro-ca-trust = {
    description = "Install mitmproxy CA certificate";
    wantedBy = ["multi-user.target"];
    after = ["local-fs.target"];
    before = ["nix-daemon.service" "redsocks.service"];
    serviceConfig.Type = "oneshot";
    script = ''
      BUNDLE=/etc/ssl/vitro-ca-bundle.crt
      CA=/etc/vitro/ca/mitmproxy-ca-cert.pem

      for i in $(seq 1 30); do
        [ -f "$CA" ] && break
        sleep 1
      done

      cp /etc/ssl/certs/ca-certificates.crt "$BUNDLE" 2>/dev/null || touch "$BUNDLE"
      if [ -f "$CA" ]; then
        cat "$CA" >> "$BUNDLE"
      fi
    '';
  };

  systemd.services.nix-daemon.environment = {
    NIX_SSL_CERT_FILE = lib.mkForce "/etc/ssl/vitro-ca-bundle.crt";
    CURL_CA_BUNDLE = lib.mkForce "/etc/ssl/vitro-ca-bundle.crt";
    SSL_CERT_FILE = "/etc/ssl/vitro-ca-bundle.crt";
  };

  i18n.defaultLocale = "en_US.UTF-8";
  time.timeZone = "UTC";
}
