{
  description = "Hitori VPN — native GPUI client";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = { url = "github:oxalica/rust-overlay"; inputs.nixpkgs.follows = "nixpkgs"; };
  };
  outputs = { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
      libs = with pkgs; [ wayland libxkbcommon vulkan-loader fontconfig freetype libx11 libxcb libxcursor libxi libxrandr openssl zlib ];
      mingw = pkgs.pkgsCross.mingwW64;
    in {
      devShells.${system} = {
        default = pkgs.mkShell {
          packages = [ pkgs.rust-bin.stable.latest.default pkgs.pkg-config pkgs.clang pkgs.mold ] ++ libs;
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
        };
        # Windows and macOS builds from Linux: scripts/build-windows.sh, scripts/build-macos.sh.
        cross = pkgs.mkShell {
          packages = [
            (pkgs.rust-bin.stable.latest.default.override { targets = [ "x86_64-pc-windows-gnu" "aarch64-apple-darwin" "x86_64-apple-darwin" ]; })
            mingw.stdenv.cc
            mingw.buildPackages.binutils
            pkgs.cargo-zigbuild
            pkgs.zig
            pkgs.llvmPackages.libclang
            pkgs.clang
            pkgs.pkg-config
            pkgs.wineWow64Packages.stable
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "x86_64-w64-mingw32-gcc";
          CC_x86_64_pc_windows_gnu = "x86_64-w64-mingw32-gcc";
          AR_x86_64_pc_windows_gnu = "x86_64-w64-mingw32-ar";
          WINDRES_x86_64_pc_windows_gnu = "x86_64-w64-mingw32-windres";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
        };
      };
    };
}
