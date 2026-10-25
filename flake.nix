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

        devShells.default = pkgs.mkShellNoCC rec {

          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [
            rustup

            glib.dev
            gst_all_1.gstreamer.dev
            gst_all_1.gst-plugins-base.dev
            systemd.dev

            wayland
            libxkbcommon
            libglvnd
            egl-wayland
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad
          ];

          RUSTC_VERSION = "stable";

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs);
        };

      };

    };
}
