# NixOS guest for programs launched "as virtual" from ironland-compositor's
# launcher (see `src/shell/vm.rs`): a minimal headless system that boots fast,
# runs an SSH server for the compositor's `waypipe ssh` to log in to, and has
# `waypipe` plus a few Wayland programs installed. The compositor boots a
# throw-away snapshot of the disk image per launched window.
#
# Build the image with `scripts/build-vm-guest`, which also creates the ssh key
# the compositor logs in with.
{ config, lib, pkgs, modulesPath, ... }:

let
  cfg = config.ironland.vmGuest;
in
{
  # virtio disk/net drivers in the initrd, so the image boots under QEMU.
  imports = [ (modulesPath + "/profiles/qemu-guest.nix") ];

  options.ironland.vmGuest = {
    user = lib.mkOption {
      type = lib.types.str;
      default = "user";
      description = "Account the compositor's `waypipe ssh` logs in as (`vm.user`).";
    };
    authorizedKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Public keys allowed to log in as the guest user. Password login is
        disabled entirely, so with none the guest is unreachable.
      '';
    };
    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      example = lib.literalExpression "[ pkgs.firefox ]";
      description = ''
        Programs to install in the guest. A launcher entry started as
        virtual runs its `Exec=` command *in the guest*, so the program has to
        be installed here under the same command name.
      '';
    };
  };

  config = {
    system.stateVersion = lib.trivial.release;
    networking.hostName = "ironland-vm";

    users.users.${cfg.user} = {
      isNormalUser = true;
      openssh.authorizedKeys.keys = cfg.authorizedKeys;
    };

    services.openssh = {
      enable = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "no";
      };
      # The compositor hands the login key(s) over at boot as a systemd
      # credential (an SMBIOS OEM string, see `authorized_keys_args` in
      # `src/shell/vm.rs`), taken from the user's ssh-agent or ~/.ssh, so the
      # image carries no key and one build serves every user. Root-only file,
      # hence the root-run command; `authorizedKeys` above still works too.
      authorizedKeysCommand = "${pkgs.coreutils}/bin/cat /run/credentials/@system/ironland.authorized_keys";
      authorizedKeysCommandUser = "root";
    };

    # Lets the compositor set the clock of a guest restored from saved state
    # (persistent VMs): it would otherwise resume on the time it was saved at.
    services.qemuGuest.enable = true;

    environment.systemPackages = [ pkgs.waypipe pkgs.foot ] ++ cfg.extraPackages;
    fonts.packages = [ pkgs.dejavu_fonts ];
    # Mesa's software rasterizer, for programs that want GL.
    hardware.graphics.enable = true;

    # Boot as fast as possible and log to the serial port the compositor
    # captures (`serial.log` in the VM's scratch directory).
    boot.kernelParams = [ "console=ttyS0" ];
    boot.loader.timeout = 0;
    documentation.enable = false;

    # Room on the root filesystem for what the guest writes while it runs
    # (the compositor boots a throw-away snapshot, so none of it persists).
    image.modules.qemu.virtualisation.diskSize = lib.mkDefault 6144;
  };
}
