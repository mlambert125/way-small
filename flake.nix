{
  description = "FAE-Rust development environment flake";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix?rev=27bc56a43b7ead695d1a1d598653a4c53ff32e5d"; # - 12/5 (4)
    };
  };

  outputs = {
    nixpkgs,
    flake-utils,
    fenix,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {inherit system;};
      rust = fenix.packages.${system}.complete.toolchain;
      rust-analyzer = fenix.packages.${system}.complete.rust-analyzer;
      clippy = fenix.packages.${system}.complete.clippy;
      rustfmt = fenix.packages.${system}.complete.rustfmt;
      # Linked or dlopen'd by the compositor as it stands. libxkbcommon is the
      # only one the binary links; the rest are opened at runtime, so they have
      # to be on LD_LIBRARY_PATH rather than just present at build time.
      runtimeLibraries = with pkgs; [
        libxkbcommon # keymaps and modifier state, via the xkbcommon crate
        wayland # libwayland-client, opened by winit's wayland backend
        libGL # libEGL and libGLESv2, opened by glutin and glow
      ];
      # Not used yet: what the DRM backend will need to drive displays and
      # input directly. See "DRM Backend" in docs/architecture.md.
      drmBackendLibraries = with pkgs; [
        libgbm # EGL on a gbm device, in place of a host window surface
        libinput # input events with no host compositor to get them from
        libdisplay-info # EDID parsing, for output modes and identity
        seatd # opening drm and input devices without running as root
        udev # device discovery and hotplug
      ];
      libPackages = runtimeLibraries ++ drmBackendLibraries;
    in {
      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs;
          [
            rust
            rust-analyzer
            rustfmt
            clippy
            nixd
            alejandra
            # Clients and tools to test the compositor against.
            weston
            wayland-utils
            foot
          ]
          ++ libPackages;
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libPackages;

        env = {
          WINIT_UNIX_BACKEND = "wayland";
        };
      };
    });
}
