# vitro.lib.mkEnv — creates a NixOS microVM configuration for an env.
#
# Used by the per-env wrapper flake generated at runtime by `vitro create`.
# The wrapper flake passes inputs and env-specific parameters.
{ vitro, nixpkgs, microvm }:

{
  name,
  ip,
  envDir,
  repo ? "env",
  hostConfig ? {},
  modules ? [],
  persist ? [],
  system ? "x86_64-linux",
}: let
  lib = nixpkgs.lib;
  bridge = hostConfig.bridge or { address = "192.168.83.1"; name = "cellbr"; subnet = "192.168.83.0/24"; };
  proxy = hostConfig.proxy or { httpPort = 8080; gitCredentialPort = 8081; controlPort = 8082; logFile = "/var/log/vitro/proxy.log"; };
  user = hostConfig.user or { name = "agent"; uid = 1000; authorizedKeys = []; };
  vm = hostConfig.vm or { vcpu = 4; mem = 4096; varSize = 4096; };

  env = { inherit ip name envDir repo; };

  persistShares = lib.imap0 (idx: p:
    let stripped = lib.removePrefix "/" p.path;
    in {
      tag = "persist-${toString idx}";
      source = "${envDir}/persist/${stripped}";
      mountPoint = p.path;
      proto = "virtiofs";
    }
  ) persist;
in {
  nixosConfigurations.${name} = nixpkgs.lib.nixosSystem {
    inherit system;
    specialArgs = {
      inherit env;
      vitroHost = { inherit bridge proxy user vm; };
    };
    modules = [
      microvm.nixosModules.microvm
      (vitro + "/nix/modules/guest/base.nix")
      {
        networking.hostName = "env";
        microvm = {
          vcpu = vm.vcpu;
          mem = vm.mem;
          hypervisor = "qemu";
          interfaces = [
            {
              type = "tap";
              id = "vm-${name}";
              mac = let
                hash = builtins.hashString "md5" name;
                b1 = builtins.substring 0 2 hash;
                b2 = builtins.substring 2 2 hash;
                b3 = builtins.substring 4 2 hash;
                b4 = builtins.substring 6 2 hash;
              in "02:ce:${b1}:${b2}:${b3}:${b4}";
            }
          ];
          volumes = [
            {
              image = "var.img";
              mountPoint = "/var";
              size = vm.varSize;
            }
          ];
          shares = [
            {
              tag = "ro-store";
              source = "/nix/store";
              mountPoint = "/nix/.ro-store";
              proto = "virtiofs";
            }
            {
              tag = "env";
              source = "${envDir}/repo";
              mountPoint = "/${repo}";
              proto = "virtiofs";
            }
            {
              tag = "vitro-ca";
              source = "/var/lib/vitro/ca";
              mountPoint = "/etc/vitro/ca";
              proto = "virtiofs";
            }
            {
              tag = "vitro-copyfiles";
              source = "/var/lib/vitro/copyfiles";
              mountPoint = "/etc/vitro/copyfiles";
              proto = "virtiofs";
            }
          ] ++ persistShares;
          writableStoreOverlay = "/var/.rw-store";
        };
        systemd.network.networks."10-lan" = {
          matchConfig.Type = "ether";
          networkConfig = {
            Address = "${ip}/24";
            Gateway = bridge.address;
            DNS = bridge.address;
          };
        };
        # create mount points for persist shares inside the VM
        systemd.tmpfiles.rules = map (p: "d ${p.path} 0755 root root -") persist;
      }
    ]
    ++ modules;
  };
}
