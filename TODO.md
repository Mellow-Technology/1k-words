# TODO

## Upstream GPUI contributions

- **Cross-platform `surface` element.** It's macOS-only: the DirectX
  renderer's `draw_surfaces` is a stub and the wgpu renderer skips surfaces.
  Start with D3D11 textures on Windows, then DMA-BUF on Linux. Until then,
  Windows and Linux copy video frames into images (`src/video/mod.rs`).
- **Colour matrix from the frame's tags.** The surface shaders
  (`gpui-pre-apple/src/shaders.metal`, `gpui-pre-wgpu/src/shaders.wgsl`)
  hard-code BT.601 full range. Read the buffer's YCbCr matrix and range
  attachments and pass the matrix to the shader instead.
  `SurfaceConverter` in `src/video/macos.rs` works around this with an extra
  VideoToolbox pass for non-BT.601 video.
- **Accept video-range (`420v`) buffers.** The Metal renderer asserts
  full-range `420f`, which forces a conversion pass for most video.
- **Rounded corners on surfaces.** Playing gallery tiles show square corners.
- **Colour management.** GPUI's `CAMetalLayer` has no colour space, so stills
  and video are shown as device RGB. Display P3 photos look right on P3
  displays only, and nothing is matched to the display's profile.

## Distribution and licensing

- **LGPL-only FFmpeg build.** Homebrew's FFmpeg includes GPL components.
  Build and bundle FFmpeg without `--enable-gpl` (only the decoders we need),
  linked dynamically.
- **Bundle the native libraries.** FFmpeg and libheif (and its codec,
  libde265) currently come from Homebrew; a distributed app must ship them.
- **LGPL compliance.** rawler (LGPL-2.1) is statically linked Rust, so users
  must be able to relink against a modified version. libheif and FFmpeg are
  LGPL too.
- **Font licence.** Lato is under the SIL OFL (`assets/fonts/OFL.txt`); ship
  the licence with the app.

## Video and Live Photos

- **Hardware decoding on Windows and Linux** (D3D11VA, VA-API), ahead of the
  cross-platform surface work.
- **Audio** for Live Photos in the viewer, as Apple's Photos does. Needs an
  audio output library.
- **Pair by Apple content identifier**, not just the file name.
- **Zoom and pan while playing.** The viewer shows the video fitted, ignoring
  the photo's zoom.
- **HDR tone mapping in software.** The copy path (Windows, Linux, and the
  macOS fallback) decodes HDR video without tone mapping, so it looks
  washed out.

## Location and maps

Location is shown in the info sidebar (`src/location.rs`). Ideas for later:

- **Show in Maps.** A button in the Location section that opens Maps at the
  coordinates. No network use in the app itself.
- **Place names** (e.g. "St. Louis, MO") by reverse geocoding, e.g. with
  CoreLocation's geocoder on macOS. This sends coordinates to a service, so
  it should be opt-in and cached.
- _More map ideas to come._

## Sync

Targets are set up on the Sync page in Settings, and each library chooses
its targets on the Libraries page (`src/sync_settings.rs`).

- **Sync in the background**, after imports and on a timer, rather than only
  with Sync Now.
- **Choose a library's folder in a target.** It's `<name>-<start of id>`,
  fixed when the target's added.
- **Secure settings on Windows and Linux.** On macOS targets' settings are in
  the keychain; elsewhere they're in `sync-targets.json` as plain JSON. Use
  Credential Manager and the Secret Service.
- **Sign the app** so the keychain doesn't ask again after each update (items
  are tied to the binary's code signature).
- **Listing.** The plugin interface has it; the app doesn't use it yet
  (restoring a library, adopting files already on a service). Downloading
  is `library::download`.
- **Resume big uploads.** Files are uploaded several at once, and big ones
  in parts (`library::sync`), but a file whose upload in parts is stopped
  is given up on and starts again next time. S3 keeps the parts of an open
  upload, so they could be recorded and resumed.
- **Plugins from users.** `.wasm` files in the app's `plugins` folder are
  loaded, but there's no way to install them or review what they can reach.
  Plugins built against the first interface (`onekwords:sync@0.1.0`) no
  longer load; the interface is `onekwords:plugins@0.2.0` now.
- **Thumbnails first, originals on demand.** Thumbnails (and other
  derivatives, `src/library/derivatives.rs`) are kept in the library,
  catalogued apart from the originals, and uploaded ahead of them. Still to
  do for a device short on space:
  - sync the catalog itself, so another device knows the library's photos;
  - record which originals are on the device (say, a column on `photos`),
    and show and open photos whose originals aren't;
  - free up space by removing originals that are safely synced.
- **More derivatives**, such as viewer-sized previews and video posters.
  Each is a `DerivativeKind`, with its own folder.

## Object detection

Objects are found from each photo's thumbnail by the detector chosen in
Settings (`src/detection_settings.rs`): YOLO11 on this device
(`src/detect.rs`), or a detection plugin, such as the built-in one for
OpenAI-compatible vision models (`plugins/openai-vision`). Each detector's
findings are recorded apart in the catalog (`src/library/objects.rs`). The
viewer's info sidebar lists them and toggles shimmering boxes around them
(`src/object_boxes.rs`); the gallery's search field finds them, suggesting
the kinds found so far as it's typed.

- **Ask before downloading the model.** It's 228 MB, fetched from Hugging
  Face the first time a library is opened. Offer a setting, or bundle a
  smaller model (YOLO11n is ~11 MB) once there's a trusted ONNX export.
- **Refresh search results** as the first scan finds more; a search shows
  what had been found when it ran.
- **Progress** for the first scan of a big library.
- **Several photos at once for remote detectors.** Photos are looked at one
  at a time, which suits the built-in model, but a vision model on a
  service takes seconds each. Like uploads, several could go at once.
- **Say what a remote detector will cost.** Choosing one has it look at
  every photo, once each; with a paid API, say how many before starting.
- **Pick the photo size for a detector.** Plugins get the 560-pixel-wide
  thumbnail; some models would do better with more, or as well with less.
- **A smaller model.** The helper process takes ~440 MB while it runs,
  most of it YOLO11x's weights; YOLO11s or m would likely find much the
  same kinds of object in a fraction of that. Bumping `MODEL` rescans.

## Memory

- **Tiled full-resolution images.** The viewer loads a photo scaled to
  the window, and its full resolution only once it's zoomed in past that
  (`PhotoViewer::load_full_if_needed`), when a 24 MP photo takes ~100 MB
  in memory and as much in the GPU atlas. Loading just the tiles in view
  would bound it at any size.

## Albums

Albums are kept in each library's catalog (`src/library/albums.rs`), shown
nested in the sidebar (`navigation::AlbumTree`), and filled by importing
into them (`src/album_dialogs.rs`), from the gallery's right-click menu, by
dragging the selection onto one, or from the viewer's albums menu, which
also shows the albums a photo is in. They're moved by dragging them onto
one another, and can show the photos of the albums in them. A copy is kept
in the library's `Catalog` folder and restored into a catalog with none.

- **Rebuild a lost catalog from the originals.** Albums are restored from
  their copy, but only photos in the catalog can go back in them; a catalog
  that's lost altogether comes back empty, so its albums do too (the copy
  is kept as `Catalog/albums-<time>.json`). Cataloguing the originals
  already in a library's folder would fix both.
- **Sync the albums' copy** with the photos (see Sync); it's rewritten as
  albums change, so it needs uploading again each time.
- **Drop onto the top level.** An album goes back to the top level from
  its menu; there's nowhere to drop it.

## Automated Updates

- Integrate [GPUI Updater](https://github.com/AprilNEA/gpui-updater) for automatic updates
