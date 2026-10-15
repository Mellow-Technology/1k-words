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

## Automated Updates

- Integrate [GPUI Updater](https://github.com/AprilNEA/gpui-updater) for automatic updates

## Turso Integration

-
-
-