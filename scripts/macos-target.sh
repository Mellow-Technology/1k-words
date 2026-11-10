# The oldest macOS the app supports, for the scripts that build what it
# ships: its native libraries (build-ffmpeg.sh, build-libheif.sh) and the app
# itself (bundle-macos.sh), whose Info.plist says the same. Sourced, not run.
#
# macOS 13 for VideoToolbox's VTPixelRotationSession, which rotates Live
# Photo frames on the GPU (src/video/macos.rs). Without this, compilers
# default to the macOS they run on, and the app wouldn't open on anything
# older than the Mac that built it.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
