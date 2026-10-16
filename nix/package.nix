{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  glib,
  gtk4,
  libadwaita,
  webkitgtk_6_0,
  poppler,
  cairo,
  pango,
  gdk-pixbuf,
  graphene,
  gsettings-desktop-schemas,
  gst_all_1,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "flux-preview";
  inherit (cargoToml.package) version;

  src = lib.cleanSource ./..;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
  ];

  buildInputs = [
    glib
    gtk4
    libadwaita
    webkitgtk_6_0
    poppler
    cairo
    pango
    gdk-pixbuf
    graphene
    gsettings-desktop-schemas
  ]
  ++ (with gst_all_1; [
    gstreamer
    gst-plugins-base
    gst-plugins-good
    gst-plugins-bad
    gst-plugins-ugly
    gst-libav
  ]);

  # Wrap only the daemon (GST_PLUGIN_SYSTEM_PATH_1_0, XDG_DATA_DIRS, GIO modules). The client
  # is std-only and runs on every space press: a shell wrapper would add its startup to that,
  # and it inherits nothing it needs. It execs the wrapped daemon sitting next to it.
  dontWrapGApps = true;
  postFixup = ''
    wrapGApp $out/bin/flux-preview-daemon
  '';

  meta = {
    description = "QuickLook-style previewer for flux: press space, see the file";
    mainProgram = "flux-preview";
    platforms = lib.platforms.linux;
  };
}
