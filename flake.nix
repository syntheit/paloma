{
  description = "Paloma — a native GTK4/libadwaita Telegram client";

  inputs = {
    # Pinned to the SAME nixpkgs rev as the tdlib-rs spike so `pkgs.tdlib` is
    # 1.8.65 — the version tdlib-rs 1.4's generated bindings expect.
    nixpkgs.url = "github:NixOS/nixpkgs/e8273b29fe1390ec8d4603f2477357555291432e";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Optional build-time API credentials. Export PALOMA_API_ID and
        # PALOMA_API_HASH before running `nix build --impure` to bake them into
        # the wrapper so the app boots straight to the phone-login screen.
        # When absent (pure builds / CI) the build succeeds and the app falls
        # back to ~/.config/paloma/credentials.toml at runtime.
        # See secrets.nix.example for the full workflow.
        apiCreds = {
          apiId = builtins.getEnv "PALOMA_API_ID";
          apiHash = builtins.getEnv "PALOMA_API_HASH";
        };

        # Pinned stable Rust toolchain via fenix (reproducible, works on aarch64 too).
        rustToolchain = fenix.packages.${system}.stable.toolchain;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Native build inputs needed at compile time.
        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          glib # provides glib-compile-schemas / glib-compile-resources
          desktop-file-utils
          appstream # validate metainfo
        ];

        # Libraries the app links against.
        buildInputs = with pkgs; [
          glib
          gtk4
          libadwaita
          # gtk4-rs -sys crates link these directly, so they must be present at
          # link time AND on the runtime library path (see shellHook / preFixup).
          pango
          cairo
          gdk-pixbuf
          librsvg # SVG pixbuf loader for the QR-code login page
          graphene
          harfbuzz
          # TDLib backend (via tdlib-rs with the `pkg-config` feature): tdjson.pc
          # lives in ${pkgs.tdlib}/lib/pkgconfig; openssl + zlib are its link deps.
          tdlib
          openssl
          zlib
          # GStreamer audio backend for voice-note playback. base+good carry the
          # ogg demuxer + opus/vorbis decoders; bad+libav round out coverage for
          # the audio/mpeg + audio/mp4 voice-note fallbacks TDLib may deliver.
          # wrapGAppsHook4 exports GST_PLUGIN_SYSTEM_PATH from these so the
          # `gstplay::Play` pipeline finds its plugins at runtime.
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
          gst_all_1.gst-plugins-bad
          gst_all_1.gst-libav
        ];

        # tdlib-rs's `pkg-config` feature probes for `tdjson.pc`. pkg-config in
        # nixpkgs already picks up buildInputs' pkgconfig dirs, but we set it
        # explicitly for robustness (inside `nix develop`, `cargo run`, crane).
        PKG_CONFIG_PATH = "${pkgs.tdlib}/lib/pkgconfig";

        # libtdjson.so is a shared object; the final binary must locate it at
        # runtime. Bake an rpath into the ELF so no LD_LIBRARY_PATH is needed for
        # the tdlib half. (The GUI libs are handled by the gapps wrapper below.)
        RUSTFLAGS = "-C link-arg=-Wl,-rpath,${pkgs.tdlib}/lib";

        # Cleaned source (Rust/TOML only) for the dependency layer — keeps the
        # crane cache warm across data/README/etc. edits.
        cleanSrc = craneLib.cleanCargoSource ./.;

        # Full source for the final build so postInstall can reach data/*.
        fullSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            ./data
          ];
        };

        commonArgs = {
          inherit
            nativeBuildInputs
            buildInputs
            PKG_CONFIG_PATH
            RUSTFLAGS
            ;
          strictDeps = true;
        };

        # Dependencies compiled against the cleaned source.
        cargoArtifacts = craneLib.buildDepsOnly (commonArgs // { src = cleanSrc; });

        paloma = craneLib.buildPackage (
          commonArgs
          // {
            src = fullSrc;
            inherit cargoArtifacts;

            # Install the desktop file, icon, metainfo and gschema, then compile
            # the schema so the installed app launches without GSETTINGS_SCHEMA_DIR.
            postInstall = ''
              install -Dm644 data/io.matv.Paloma.desktop \
                $out/share/applications/io.matv.Paloma.desktop
              install -Dm644 data/io.matv.Paloma.metainfo.xml \
                $out/share/metainfo/io.matv.Paloma.metainfo.xml
              install -Dm644 data/icons/hicolor/scalable/apps/io.matv.Paloma.svg \
                $out/share/icons/hicolor/scalable/apps/io.matv.Paloma.svg
              install -Dm644 data/icons/hicolor/scalable/actions/paloma-send-symbolic.svg \
                $out/share/icons/hicolor/scalable/actions/paloma-send-symbolic.svg
              install -Dm644 data/io.matv.Paloma.gschema.xml \
                $out/share/glib-2.0/schemas/io.matv.Paloma.gschema.xml
              glib-compile-schemas $out/share/glib-2.0/schemas
            '';

            # crane doesn't stamp the GUI libraries into the binary's RPATH, so
            # the wrapped app can't find libadwaita/gtk4/glib at runtime outside a
            # full GNOME session. Put them on the wrapper's LD_LIBRARY_PATH.
            #
            # If secrets.nix supplies non-empty API credentials, bake them into
            # the wrapper environment so the app boots straight to phone-login
            # without any per-user credentials.toml setup.
            preFixup =
              let
                hasApiCreds = apiCreds.apiId != "" && apiCreds.apiHash != "";
              in
              ''
                gappsWrapperArgs+=(
                  --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath buildInputs}"
                  ${pkgs.lib.optionalString hasApiCreds ''--set PALOMA_API_ID "${apiCreds.apiId}"''}
                  ${pkgs.lib.optionalString hasApiCreds ''--set PALOMA_API_HASH "${apiCreds.apiHash}"''}
                )
              '';

            meta = with pkgs.lib; {
              description = "Native GTK4/libadwaita Telegram client";
              homepage = "https://github.com/syntheit/paloma";
              license = licenses.gpl3Plus;
              mainProgram = "paloma";
              platforms = platforms.linux;
            };
          }
        );
      in
      {
        packages = {
          default = paloma;
          paloma = paloma;
          tdlib = pkgs.tdlib;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = paloma;
          name = "paloma";
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs PKG_CONFIG_PATH RUSTFLAGS;
          nativeBuildInputs = nativeBuildInputs ++ [
            rustToolchain
            fenix.packages.${system}.stable.rust-analyzer
            pkgs.clippy
          ];

          shellHook = ''
            export GSETTINGS_SCHEMA_DIR="$PWD/data"
            # `cargo run` launches the unwrapped binary; nix build's wrapGAppsHook4
            # handles this for the packaged app, but the devshell needs the GUI
            # libs (and libtdjson) on the runtime linker path explicitly.
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath buildInputs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            if [ -f data/io.matv.Paloma.gschema.xml ]; then
              glib-compile-schemas data 2>/dev/null || true
            fi
            echo "paloma devshell — run: cargo run   |   smoke: cargo run --bin tdlib-check"
          '';
        };
      }
    );
}
