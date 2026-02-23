{
  boot.loader.grub.enable = true;

  disko.devices.disk.main = {
    type = "disk";
    # use /dev/disk/by-id/... for stable device naming
    device = "/dev/sda";
    content = {
      type = "gpt";
      partitions = {
        bios = {
          size = "1M";
          type = "EF02";
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };
}
