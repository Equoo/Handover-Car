{
  description = "RC";

  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" ];
      perSystem = { config, self', inputs', pkgs, system, ... }: {

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ 
            pkgconf
            glib.dev
            gst_all_1.gstreamer.dev
            glib
            gst_all_1.gstreamer
            gst
            pcre2
            sysprof
            libsysprof-capture
            libunwind
            elfutils
            zstd
            util-linux
            libselinux
            libsepol
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad
            gst_all_1.gst-plugins-ugly
            orc
            wayland
            wayland-protocols
            libxkbcommon
            mesa
            libGL
            libglvnd
            egl-wayland
            systemd
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.mesa
            pkgs.libglvnd
            pkgs.egl-wayland
          ];

        };

      };

    };
}

