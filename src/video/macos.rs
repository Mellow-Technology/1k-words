//! Keeps hardware-decoded frames on the GPU on macOS.
//!
//! FFmpeg decodes with VideoToolbox into IOSurface-backed CVPixelBuffers.
//! GPUI's `surface` element draws those directly, but only in one format
//! (8-bit, full-range, bi-planar 4:2:0; anything else panics in its
//! renderer), and its shader always decodes colours with the BT.601 matrix.
//! It can't rotate frames either. VideoToolbox converts frames to that
//! encoding (tone mapping HDR to SDR) and rotates them on the GPU as needed.

use std::ptr::{NonNull, null_mut};

use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;
use image::{Rgb, RgbImage};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType, Type as _};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_P3_D65,
    kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_2100_HLG, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTPixelRotationSession, VTPixelTransferSession, VTSessionSetProperty,
    kVTPixelRotationPropertyKey_Rotation, kVTPixelTransferPropertyKey_DestinationColorPrimaries,
    kVTPixelTransferPropertyKey_DestinationTransferFunction,
    kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, kVTRotation_180, kVTRotation_CCW90,
    kVTRotation_CW90,
};

/// The only pixel format GPUI's `surface` element accepts.
const SURFACE_FORMAT: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

/// Asks FFmpeg to decode with VideoToolbox. If that isn't possible (an
/// unsupported codec, say), FFmpeg falls back to software decoding.
pub fn enable_hardware_decoding(context: &mut ffmpeg::codec::context::Context) {
    unsafe {
        let mut device: *mut ffi::AVBufferRef = null_mut();
        let status = ffi::av_hwdevice_ctx_create(
            &mut device,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            std::ptr::null(),
            null_mut(),
            0,
        );
        if status >= 0 {
            (*context.as_mut_ptr()).hw_device_ctx = device;
        }
    }
}

/// How a frame's colours have to change for GPUI to draw them correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColorConversion {
    /// Re-encode with the BT.601 matrix, which GPUI's shader assumes.
    matrix: bool,
    /// Tone map HDR (PQ or HLG) to SDR.
    tone_map: bool,
    /// Map BT.2020 colours into P3, the gamut of iPhone photos. The app
    /// doesn't colour manage, so this keeps HDR video in step with stills.
    gamut: bool,
}

impl ColorConversion {
    fn of(buffer: &CVPixelBuffer) -> Self {
        let tag = |key| attachment(buffer, key);
        let is = |value: &Option<CFRetained<CFString>>, expected: &CFString| {
            value.as_deref() == Some(expected)
        };
        let transfer = tag(unsafe { kCVImageBufferTransferFunctionKey });
        unsafe {
            Self {
                // Untagged frames are left alone: there's nothing to convert
                // from.
                matrix: tag(kCVImageBufferYCbCrMatrixKey)
                    .is_some_and(|matrix| *matrix != *kCVImageBufferYCbCrMatrix_ITU_R_601_4),
                tone_map: is(&transfer, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ)
                    || is(&transfer, kCVImageBufferTransferFunction_ITU_R_2100_HLG),
                gamut: is(
                    &tag(kCVImageBufferColorPrimariesKey),
                    kCVImageBufferColorPrimaries_ITU_R_2020,
                ),
            }
        }
    }

    fn is_needed(self) -> bool {
        self.matrix || self.tone_map || self.gamut
    }
}

pub struct SurfaceConverter {
    /// Clockwise degrees.
    rotation: u32,
    rotation_session: Option<CFRetained<VTPixelRotationSession>>,
    transfer_session: Option<(ColorConversion, CFRetained<VTPixelTransferSession>)>,
}

// SAFETY: the sessions are only used from the decode thread that owns the
// converter.
unsafe impl Send for SurfaceConverter {}

impl SurfaceConverter {
    pub fn new(rotation: u32) -> Self {
        Self {
            rotation,
            rotation_session: None,
            transfer_session: None,
        }
    }

    /// Returns the frame as a pixel buffer GPUI can draw, upright, or `None`
    /// if VideoToolbox couldn't convert it.
    pub fn convert(&mut self, frame: &ffmpeg::frame::Video) -> Option<CFRetained<CVPixelBuffer>> {
        // For VideoToolbox frames, FFmpeg stores the CVPixelBufferRef in
        // `data[3]`.
        let source = unsafe { ((*frame.as_ptr()).data[3] as *const CVPixelBuffer).as_ref()? };
        let mut current: CFRetained<CVPixelBuffer> = source.retain();

        // Convert to the surface format (e.g. from 10-bit HDR frames) and
        // colour encoding.
        let conversion = ColorConversion::of(&current);
        if CVPixelBufferGetPixelFormatType(&current) != SURFACE_FORMAT || conversion.is_needed() {
            let destination = create_pixel_buffer(
                CVPixelBufferGetWidth(&current),
                CVPixelBufferGetHeight(&current),
            )?;
            let session = self.transfer_session(conversion)?;
            if unsafe { session.transfer_image(&current, &destination) } != 0 {
                return None;
            }
            current = destination;
        }

        if self.rotation != 0 {
            let (width, height) = (
                CVPixelBufferGetWidth(&current),
                CVPixelBufferGetHeight(&current),
            );
            let (width, height) = if self.rotation == 180 {
                (width, height)
            } else {
                (height, width)
            };
            let destination = create_pixel_buffer(width, height)?;
            let session = self.rotation_session()?;
            if unsafe { session.rotate_image(&current, &destination) } != 0 {
                return None;
            }
            current = destination;
        }
        Some(current)
    }

    fn transfer_session(&mut self, conversion: ColorConversion) -> Option<&VTPixelTransferSession> {
        if self
            .transfer_session
            .as_ref()
            .is_none_or(|(current, _)| *current != conversion)
        {
            self.transfer_session = None;
            let mut session: *mut VTPixelTransferSession = null_mut();
            let status =
                unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
            if status != 0 {
                return None;
            }
            let session = unsafe { CFRetained::from_raw(NonNull::new(session)?) };
            // Unset properties keep the source's encoding.
            let mut properties = vec![unsafe {
                (
                    kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                )
            }];
            if conversion.tone_map {
                properties.push(unsafe {
                    (
                        kVTPixelTransferPropertyKey_DestinationTransferFunction,
                        kCVImageBufferTransferFunction_ITU_R_709_2,
                    )
                });
            }
            if conversion.gamut {
                properties.push(unsafe {
                    (
                        kVTPixelTransferPropertyKey_DestinationColorPrimaries,
                        kCVImageBufferColorPrimaries_P3_D65,
                    )
                });
            }
            for (key, value) in properties {
                if unsafe { VTSessionSetProperty(&session, key, Some(value)) } != 0 {
                    return None;
                }
            }
            self.transfer_session = Some((conversion, session));
        }
        self.transfer_session
            .as_ref()
            .map(|(_, session)| &**session)
    }

    fn rotation_session(&mut self) -> Option<&VTPixelRotationSession> {
        if self.rotation_session.is_none() {
            let mut session: *mut VTPixelRotationSession = null_mut();
            let status =
                unsafe { VTPixelRotationSession::create(None, NonNull::from(&mut session)) };
            if status != 0 {
                return None;
            }
            let session = unsafe { CFRetained::from_raw(NonNull::new(session)?) };
            let rotation: &CFString = unsafe {
                match self.rotation {
                    90 => kVTRotation_CW90,
                    180 => kVTRotation_180,
                    _ => kVTRotation_CCW90,
                }
            };
            let status = unsafe {
                VTSessionSetProperty(
                    &session,
                    kVTPixelRotationPropertyKey_Rotation,
                    Some(rotation),
                )
            };
            if status != 0 {
                return None;
            }
            self.rotation_session = Some(session);
        }
        self.rotation_session.as_deref()
    }
}

/// A colour tag on a pixel buffer, e.g. its YCbCr matrix.
fn attachment(buffer: &CVPixelBuffer, key: &CFString) -> Option<CFRetained<CFString>> {
    unsafe { buffer.attachment(key, null_mut()) }?
        .downcast::<CFString>()
        .ok()
}

/// An IOSurface-backed pixel buffer in the surface format, which Metal (and
/// so GPUI) can read without copying.
fn create_pixel_buffer(width: usize, height: usize) -> Option<CFRetained<CVPixelBuffer>> {
    let io_surface_properties = CFDictionary::<CFString, CFType>::empty();
    let keys = unsafe {
        [
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let attributes = CFDictionary::<CFString, CFType>::from_slices(
        &keys,
        &[&io_surface_properties, CFBoolean::new(true)],
    );
    let mut buffer: *mut CVPixelBuffer = null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            SURFACE_FORMAT,
            Some(attributes.as_opaque()),
            NonNull::from(&mut buffer),
        )
    };
    if status != kCVReturnSuccess {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new(buffer)?) })
}

/// The colour GPUI's surface shader draws at the centre of `buffer`, using
/// the same YCbCr-to-RGB conversion (BT.601, full range).
/// The surface's pixels, as GPUI's `surface_fragment` shader draws them.
pub fn surface_image(buffer: &CVPixelBuffer) -> RgbImage {
    unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    let plane = |plane: usize| unsafe {
        let row = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
        let height = CVPixelBufferGetHeightOfPlane(buffer, plane);
        let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane) as *const u8;
        (std::slice::from_raw_parts(base, row * height), row)
    };
    let ((luma, luma_row), (chroma_plane, chroma_row)) = (plane(0), plane(1));
    let width = CVPixelBufferGetWidthOfPlane(buffer, 0) as u32;
    let height = CVPixelBufferGetHeightOfPlane(buffer, 0) as u32;
    let image = RgbImage::from_fn(width, height, |x, y| {
        let (x, y) = (x as usize, y as usize);
        let chroma = (y / 2) * chroma_row + (x / 2) * 2;
        Rgb(surface_rgb(
            luma[y * luma_row + x],
            chroma_plane[chroma],
            chroma_plane[chroma + 1],
        ))
    });
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    image
}

/// From GPUI's `surface_fragment` shader.
fn surface_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let [y, cb, cr] = [y, cb, cr].map(|v| f32::from(v) / 255.);
    [
        y + 1.402 * (cr - 0.5),
        y - 0.3441 * (cb - 0.5) - 0.7141 * (cr - 0.5),
        y + 1.772 * (cb - 0.5),
    ]
    .map(|v| (v.clamp(0., 1.) * 255.).round() as u8)
}

#[cfg(test)]
pub fn surface_centre_colour(buffer: &CVPixelBuffer) -> [u8; 3] {
    let sample = |plane: usize, bytes_per_pixel: usize| unsafe {
        let (width, height) = (
            CVPixelBufferGetWidthOfPlane(buffer, plane),
            CVPixelBufferGetHeightOfPlane(buffer, plane),
        );
        let row = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
        let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane) as *const u8;
        let pixel = base.add(height / 2 * row + width / 2 * bytes_per_pixel);
        std::slice::from_raw_parts(pixel, bytes_per_pixel).to_vec()
    };
    unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    let (y, cb_cr) = (sample(0, 1), sample(1, 2));
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };

    surface_rgb(y[0], cb_cr[0], cb_cr[1])
}
