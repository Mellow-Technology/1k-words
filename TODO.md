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
- **Download and listing.** The plugin interface has them; the app doesn't
  use them yet (restoring a library, adopting files already on a service).
- **Parallel and multipart uploads** for big libraries and long videos.
- **Plugins from users.** `.wasm` files in the app's `plugins` folder are
  loaded, but there's no way to install them or review what they can reach.
- **Thumbnails first, originals on demand.** Thumbnails (and other
  derivatives, `src/library/derivatives.rs`) are kept in the library,
  catalogued apart from the originals, and uploaded ahead of them. Still to
  do for a device short on space:
  - sync the catalog itself, so another device knows the library's photos;
  - download a library's derivatives, and its originals only when asked;
  - record which originals are on the device (say, a column on `photos`),
    and show and open photos whose originals aren't;
  - free up space by removing originals that are safely synced.
- **More derivatives**, such as viewer-sized previews and video posters.
  Each is a `DerivativeKind`, with its own folder.

## Object detection

Objects are found with YOLO11 (`src/detect.rs`), from each photo's
thumbnail, and recorded in the catalog (`src/library/objects.rs`). The
viewer's info sidebar lists them and toggles shimmering boxes around them
(`src/object_boxes.rs`); the gallery's search field finds them, suggesting
the kinds found so far as it's typed.

- **Ask before downloading the model.** It's 228 MB, fetched from Hugging
  Face the first time a library is opened. Offer a setting, or bundle a
  smaller model (YOLO11n is ~11 MB) once there's a trusted ONNX export.
- **Refresh search results** as the first scan finds more; a search shows
  what had been found when it ran.
- **Progress** for the first scan of a big library.
- **A smaller model.** The helper process takes ~440 MB while it runs,
  most of it YOLO11x's weights; YOLO11s or m would likely find much the
  same kinds of object in a fraction of that. Bumping `MODEL` rescans.

## Memory

- **The viewer's full image.** A 24 MP photo peaks above 300 MB:
  `decode::viewer_preview` clones the decoded image only to shrink it
  (`thumbnail_exact` takes `&self`), and the full texture is capped at
  `MAX_TEXTURE_SIZE` rather than what the display can show at the
  furthest zoom. Tiling would bound it altogether.
- **Concurrent raw decodes.** Developing a raw file takes a few hundred
  MB, and the gallery runs up to six full decodes at once; raw files
  should count for more than JPEGs against that limit.

## Albums

Albums are kept in each library's catalog (`src/library/albums.rs`), shown
nested in the sidebar, and filled by importing into them
(`src/album_dialogs.rs`), from the gallery's right-click menu, by dragging
the selection onto one, or from the viewer's albums menu, which also shows
the albums a photo is in.

- **Drop onto a whole sidebar row.** GPUI Kit's sidebar items take no
  drops, so an album's drop target is its row's suffix, stretched over the
  label (`navigation::album_item`); the icon and caret aren't covered. A
  drop hook upstream would be cleaner.
- **Box selection from a selected photo.** Dragging the selection moves it
  (onto an album), so a box can only start elsewhere.
- **Move albums** into other albums, by dragging them in the sidebar.
- **Keep albums safe.** Unlike the rest of the catalog, they can't be found
  again from the originals; syncing the catalog (see Sync) would cover them.
- **Show the photos of the albums inside** an album, optionally.

## Automated Updates

- Integrate [GPUI Updater](https://github.com/AprilNEA/gpui-updater) for automatic updates

## Turso Integration

-
-
-