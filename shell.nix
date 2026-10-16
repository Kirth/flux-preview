{ pkgs ? import <nixpkgs> { } }:

let
  gstPlugins = with pkgs.gst_all_1; [
    gstreamer
    gst-plugins-base
    gst-plugins-good
    gst-plugins-bad
    gst-plugins-ugly
    gst-libav
  ];
in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    cargo
    rustc
    rustfmt
    clippy
    rust-analyzer
    pkg-config
    glib # glib-compile-resources (build.rs)
    gettext # msgfmt (make translations)
    gnumake
    git
    desktop-file-utils
  ];

  buildInputs = with pkgs; [
    glib
    gtk4
    libadwaita
    vte-gtk4
    webkitgtk_6_0 # HTML previews
    poppler
    cairo
    pango
    gdk-pixbuf
    graphene
    fontconfig
    freetype
    xz
    bzip2
    zstd
    openssl
    gsettings-desktop-schemas
    adwaita-icon-theme
    hicolor-icon-theme
    glib-networking
    gvfs
  ] ++ gstPlugins;

  # External tools flux shells out to at runtime
  packages = with pkgs; [
    p7zip
    binutils # ar
    file
    exiftool
    ffmpeg
    icoutils # icotool, wrestool
    util-linux # lsblk
    udisks
    xdg-utils
    e2fsprogs
    exfatprogs
    dosfstools # fatlabel
    btrfs-progs
    xfsprogs
    python3
  ];

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

  shellHook = ''
    export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.adwaita-icon-theme}/share:${pkgs.hicolor-icon-theme}/share:$XDG_DATA_DIRS"
    export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules:${pkgs.gvfs}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}"
    export GST_PLUGIN_SYSTEM_PATH_1_0="${pkgs.lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins}"
  '';
}
