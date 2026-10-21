# NixOS guest for programs launched "as virtual" from ironland-compositor's
# launcher (see `src/shell/vm.rs`): a minimal headless system that boots fast,
# runs an SSH server for the compositor's `waypipe ssh` to log in to, and has
# `waypipe` plus the one program the image is for installed (`packages`).
# Every program gets its own image, built by `services.ironland-compositor.vm
# .programs` (`nix/module.nix`); the compositor boots a snapshot of it per
# launch.
#
# Build one by hand with `scripts/build-vm-guest <package>`, which also
# creates the ssh key the compositor logs in with.
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
    packages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      example = lib.literalExpression "[ pkgs.firefox ]";
      description = ''
        The program this image is for (and anything it needs). A launcher
        entry started as virtual runs its `Exec=` command *in the guest*, so
        the program has to be installed here under the same command name. A
        `Terminal=true` entry also needs its terminal emulator (`terminal`
        under `[vm]`, else the top-level `terminal` setting) in the list.
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
      # sshd refuses a command whose path has a group-writable parent, which
      # rules out anything under /nix/store (mode 1775) and any symlink into
      # it, hence the plain root-owned script in /etc below.
      authorizedKeysCommand = "/etc/ssh/ironland-authorized-keys";
      authorizedKeysCommandUser = "root";
    };
    # A non-symlink `mode` makes NixOS install a real copy rather than a link.
    environment.etc."ssh/ironland-authorized-keys" = {
      mode = "0755";
      text = ''
        #!/bin/sh
        exec ${pkgs.coreutils}/bin/cat /run/credentials/@system/ironland.authorized_keys
      '';
    };

    # Lets the compositor set the clock of a guest restored from saved state
    # (persistent VMs): it would otherwise resume on the time it was saved at.
    services.qemuGuest.enable = true;

    environment.systemPackages = [ pkgs.waypipe ] ++ cfg.packages;
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
