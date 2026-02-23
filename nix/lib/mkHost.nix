# vitro.lib.mkHost — creates a NixOS configuration for a vitro server.
#
# Usage in a host flake:
#   vitro.lib.mkHost { inherit vitro nixpkgs disko; } {
#     name = "myhost";
#     disk = ./disk.nix;
#     sshPubkey = "ssh-ed25519 ...";
#     config = { vitro.server.enable = true; };
#   }
{ vitro, nixpkgs, disko }:

{
  name,
  disk,
  sshPubkey,
  system ? "x86_64-linux",
  config ? {},
}: {
  nixosConfigurations.${name} = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      disko.nixosModules.disko
      vitro.nixosModules.server
      disk
      ({pkgs, ...}: {
        networking.hostName = name;
        system.stateVersion = "24.11";

        # virtio drivers for cloud/VM hosts
        imports = [(nixpkgs + "/nixos/modules/profiles/qemu-guest.nix")];

        # server networking
        networking.useDHCP = true;
        networking.firewall.allowedTCPPorts = [22];

        # essential packages
        environment.systemPackages = [
          vitro.packages.${system}.default
          pkgs.git
        ];

        # SSH access
        services.openssh.enable = true;
        users.users.root.openssh.authorizedKeys.keys = [sshPubkey];
      })
      config
    ];
  };
}
