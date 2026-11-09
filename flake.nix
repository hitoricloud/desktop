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
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ pkgs.rust-bin.stable.latest.default pkgs.pkg-config pkgs.clang pkgs.mold ] ++ libs;
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
      };
    };
}
