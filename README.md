<p align="center"><img src="assets/app-icon/icon.svg" width="128" alt="1k Words"></p>

<h1 align="center">1k Words</h1>

<p align="center">A picture is worth 1000 words, make sure you can find the right one</p>

For photographers who'd rather be shooting than sorting. Brackets stack themselves, search finds what's in your photos, and syncing just works, so you can focus on the stories your pictures tell.

1k Words is a native photo library app for macOS, written in Rust on GPUI, through gpui-kit. It's free and open source under the [LGPL-3.0](COPYING.LESSER).

## Features

### Stacks

1k Words reads each photo's exposure details and keeps the frames of one moment together, as one tile in the grid ([`src/photos.rs`](src/photos.rs)). Frames are only stacked when they come from the same camera at the same dimensions.

- **Exposure brackets.** Shots seconds apart at different exposures become one stack, with the middle exposure as its cover. Every frame's exposure must differ from the others', so back-to-back brackets are split where the exposures start over.
- **Bursts.** Frames shot moments apart at the same exposure are kept together, from any camera, not only an iPhone. The burst ends where the exposure changes or the shooting pauses.
- **RAW+JPEG.** A raw file and the JPEG saved beside it are one photo, marked RAW+.
- **Live Photos.** A photo and its video are one photo, paired by name or, when renamed, by Apple's content identifier. The video plays when you hover over it, HDR video included.

### Libraries

- A library is a plain folder you can open in Finder, on your Mac or an external drive, with its originals filed by the day they were taken. Keep as many as you like, side by side in the sidebar.
- Originals are read-only on disk, so another app or a stray drag in Finder can't change or delete them by accident ([`src/library/protect.rs`](src/library/protect.rs)).
- Each library has a catalog of its photos and their metadata. Everything in it apart from albums can be read again from the originals.
- Libraries can be moved or deleted from Settings; a deleted library goes to the Trash.

### Importing

- Cards, USB drives and cameras that mount as disks appear in the sidebar, as in Apple's Photos, and can be ejected from there ([`src/devices`](src/devices/mod.rs)).
- Import copies photos from a device or a folder into a library, optionally into an album ([`src/library/import.rs`](src/library/import.rs)).
- Photos already in the library (by content) are skipped.
- Each file is copied into a staging folder and moved into `Originals/YYYY/MM/DD` once it's complete, so a failed or interrupted import never leaves a half-copied file among the originals.

### Viewing

- The grid is a masonry layout that only draws the photos in view, so a big library scrolls as easily as a small one.
- **Quick preview.** Press Space to see the selected photo over the grid, with its main exposure details and every frame of a bracket or burst along the bottom ([`src/quick_preview.rs`](src/quick_preview.rs)).
- **Viewer.** A zoomable view of one photo, a sidebar with its details (file, camera, lens, exposure and location), and a filmstrip of every frame for brackets and bursts.
- Photos show their embedded preview straight away, then the full image. Zoomed in past the fitted size, the photo is decoded at full resolution and only the tiles in view are drawn ([`src/tiles.rs`](src/tiles.rs)).
- Formats: camera raw (through [rawler](https://github.com/dnglab/dnglab)), HEIC and HEIF (through libheif), JPEG, PNG, WebP, GIF, BMP and TIFF, and Live Photo videos (through FFmpeg).
- On macOS, Live Photo video is hardware decoded with VideoToolbox and stays on the GPU ([`src/video`](src/video/mod.rs)).
- While photos upload, the Dock icon's bottom crop marks become a progress bar ([`src/dock_icon.rs`](src/dock_icon.rs)).

### Search by what's in them

Objects such as people, dogs and cars are found in each photo's thumbnail, by the detector chosen in Settings ([`src/detect.rs`](src/detect.rs)):

- **On this device:** YOLO11, run by ONNX Runtime (with Core ML on macOS) in a helper process. The model is downloaded the first time it's needed and checked against its known hash. Only the model is downloaded: photos never leave the device.
- **Through a plugin:** such as the built-in one for vision models behind an OpenAI-compatible API ([`plugins/openai-vision`](plugins/openai-vision)): OpenAI, services that copy its API, or a server on your own computer or network, such as Ollama or LM Studio.

Each detector's findings are kept apart in the catalog. The gallery's search field finds them, suggesting the kinds found so far as you type, and the viewer's info sidebar lists them and draws boxes around them.

### Albums

- Albums nest like folders, and one can show the photos of the albums inside it. A photo can be in any number of albums.
- Add photos by importing into an album, from the gallery's right-click menu, by dragging the selection onto one, or from the viewer's albums menu.
- A copy of the albums is kept in the library's `Catalog` folder, so they travel with it, and they're restored from it into a catalog that has none.

### Sync and backup

- **Sync targets.** Back a library up to any S3-compatible storage (AWS, Cloudflare R2, Backblaze B2, MinIO, Wasabi and others) through the built-in S3 plugin ([`plugins/s3`](plugins/s3)). Targets are set up on the Sync page in Settings, and each library chooses its targets on the Libraries page. A library can sync to several targets, and targets can be shared by libraries, each in its own folder ([`src/library/sync.rs`](src/library/sync.rs)).
- Big files are uploaded in parts. On macOS, targets' settings, credentials included, are kept in the keychain.
- **Catalog sync between Macs.** Set a catalog server (a turso-sync server) in Settings, and each open library's catalog syncs shortly after startup, every minute, after imports and with Sync Now ([`src/catalog_sync.rs`](src/catalog_sync.rs)). Each device can change the catalog while offline. If an album was deleted on one device and changed on another, you're asked which you meant.
- **Thumbnails first, originals on demand.** Thumbnails, previews and Live Photo posters are uploaded ahead of the originals. Another device downloads all of them, so it can show the whole library, and fetches an original only when it's opened ([`src/library/download.rs`](src/library/download.rs)). Photos whose originals aren't on the device are marked in the grid and viewer.
- **Pins.** Keep a photo, or an album, on this device, and its originals stay here ([`src/library/on_device.rs`](src/library/on_device.rs)). Originals that were only opened join a pool kept to a budget, evicting those wanted longest ago.

### Compared with Apple Photos

Only the differences, as far as we know. Apple Photos is a lovely place for iPhone pictures; 1k Words is for when your photos come off a memory card.

| | 1k Words | Apple Photos |
| --- | --- | --- |
| Exposure brackets | One tile per bracket, from any camera, with the middle exposure on top | One photo per frame |
| Bursts from a camera | One tile per burst, found by timing and exposure, from any camera | Grouped only when the iPhone marked them as a burst |
| Where originals live | Plain folders by day, such as `Originals/2026/10/15`, on any drive | Inside the Photos library package |
| Several libraries | Side by side in the sidebar, each on its own drive | One at a time, chosen as Photos opens |
| Backup and sync | Your own S3-compatible bucket, through sandboxed plugins | iCloud Photos |
| Source code | Open source, under the LGPL-3.0 | Closed |

## Status

1k Words is macOS first, for Macs with Apple silicon running macOS 13 or later. Releases are on [GitHub](https://github.com/Mellow-Technology/1k-words/releases). They aren't notarized by Apple yet (KW-50), so after moving the app to Applications, lift macOS's quarantine once before opening it:

```sh
xattr -dr com.apple.quarantine "/Applications/1k Words.app"
```

Still to come (see [`TODO.md`](TODO.md) for the full list):

- **Free Up Space.** Removing originals that are safely synced, to keep a library within a budget set in Settings, is built but doesn't remove anything yet: it waits until the catalog itself is known to be synced ([`src/library/free_space.rs`](src/library/free_space.rs), KW-45).
- **Release builds.** The macOS release build (KW-48), with an LGPL-only FFmpeg (KW-6) and the native libraries bundled. Windows and Linux release builds aren't done yet (KW-49).
- **Other platforms.** Removable devices in the sidebar are macOS only. Elsewhere, Live Photo video is decoded in software, and sync targets' settings are kept as plain JSON rather than in a secure store.
- **Sync.** Syncing files in the background rather than only with Sync Now, resuming big uploads, and a way to install and review plugins from users.
- **Object detection.** Asking before downloading the model, progress for the first scan of a big library, and refreshing search results as the scan finds more.
- **Albums.** Syncing the albums' copy with the photos, and rebuilding a lost catalog from the originals.
- **Automatic updates.**

## Building from source

You'll need [Rust](https://rustup.rs) and [Homebrew](https://brew.sh). rustup picks up the toolchain, and the `wasm32-wasip2` target the plugins are built for, from [`rust-toolchain.toml`](rust-toolchain.toml).

```sh
brew install ffmpeg libheif pkg-config
git clone https://github.com/Mellow-Technology/1k-words
cd 1k-words
cargo run --release
```

FFmpeg decodes Live Photo videos and libheif decodes HEIC photos. [`build.rs`](build.rs) builds the built-in plugins in [`plugins`](plugins) into WebAssembly components, which the app embeds.

### The app, with its own libraries

Homebrew's FFmpeg and libheif are built with GPL components (libheif links the x265 encoder), so the app ships its own, LGPL only and with just what it uses:

- [`scripts/build-ffmpeg.sh`](scripts/build-ffmpeg.sh) builds FFmpeg 9.0.2 into `target/ffmpeg/9.0.2`.
- [`scripts/build-libheif.sh`](scripts/build-libheif.sh) builds libheif 1.23.7 with libde265 1.1.3, decoding only, into `target/libheif/1.23.7`. It needs CMake (`brew install cmake`).

[`scripts/bundle-macos.sh`](scripts/bundle-macos.sh) builds both if they aren't there, builds the app against them, and makes `target/bundle/1k Words.app`, with the libraries in its `Frameworks` folder and their licences in `Resources/Licences`. It checks the app loads nothing from outside the bundle but macOS's, and signs it ad hoc (or with `SIGN_IDENTITY`):

```sh
scripts/bundle-macos.sh
open "target/bundle/1k Words.app"
```

The app and its libraries support macOS 13 and later ([`scripts/macos-target.sh`](scripts/macos-target.sh)). Development builds use Homebrew's libraries, unless `FFMPEG_DIR` and `PKG_CONFIG_PATH` point at these.

### Releases

Pushing a version tag that matches `Cargo.toml`'s version, such as `v0.1.0`, runs [`.github/workflows/release.yml`](.github/workflows/release.yml). It builds the libraries, runs the tests against them, bundles the app and publishes it as a GitHub release, zipped with its SHA-256. A tag with a hyphen, such as `v0.2.0-beta.1`, is a prerelease.

```sh
git tag v0.1.0
git push github v0.1.0
```

With a Developer ID certificate in the repository's secrets, the app is signed with the hardened runtime ([`scripts/entitlements.plist`](scripts/entitlements.plist)) and notarized; the workflow's header lists the secrets. Without them it's signed ad hoc, and the release notes say how to open it anyway.

## A library's folder

```text
1K Words/
  .1k-words-library
  README.txt
  Originals/2026/10/15/IMG_0001.HEIC
  Originals/2026/10/15/IMG_0001.MOV
  Thumbnails/2026/10/15/IMG_0001.HEIC.jpg
  Previews/2026/10/15/IMG_0001.HEIC.jpg
  Posters/2026/10/15/IMG_0001.MOV.jpg
  Catalog/
```

- `.1k-words-library` marks the folder as a library, with its id.
- `Originals` holds the photos as imported, filed by the day they were taken, read-only.
- `Thumbnails`, `Previews` and `Posters` are derivatives, laid out like the originals ([`src/library/derivatives.rs`](src/library/derivatives.rs)). They can always be made again from the originals.
- `Catalog` holds a copy of the library's albums.

The catalog itself lives in the app's config directory, as `libraries/<id>.db`, and `libraries.json` there lists the libraries. What's only true on this device (which originals are here, when photos were last opened, what's pinned) is kept beside it in `libraries/<id>.device.db`, and is never synced.

## Plugins

Plugins are WebAssembly components, run with Wasmtime, that extend the app ([`src/plugins`](src/plugins/mod.rs)). Their interface is [`wit/plugins.wit`](wit/plugins.wit) (`onekwords:plugins@0.2.0`). There are two kinds:

- **Sync plugins** copy a library's files to and from a service. They only move files: what's been synced where is recorded in the library's catalog.
- **Detection plugins** find objects in photos, such as with a vision model on a service.

Plugins are sandboxed: they get no file system, environment or sockets. They read and write files through handles the app gives them, and make HTTP requests through the app, only to the hosts their settings name.

The built-in plugins are [`plugins/s3`](plugins/s3) and [`plugins/openai-vision`](plugins/openai-vision). More can be added as `.wasm` files in the app's `plugins` folder, though there's no way to install them from the app yet.

## The website

The marketing site is in [`site`](site): a Next.js static export built with [gui-mellow](https://github.com/Mellow-Technology/gui-mellow), published to GitHub Pages by [`.github/workflows/pages.yml`](.github/workflows/pages.yml). See [`site/README.md`](site/README.md) for running it and adding screenshots.

## Licence

1k Words is licensed under the GNU Lesser General Public License, version 3 only (LGPL-3.0-only). See [`COPYING.LESSER`](COPYING.LESSER) and [`COPYING`](COPYING).
