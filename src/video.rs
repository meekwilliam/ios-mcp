use image::{ImageFormat, RgbImage};
use openh264::{
    OpenH264API,
    decoder::{Decoder, DecoderConfig, Flush},
    formats::YUVSource,
};

use crate::{Error, Result, picture::Picture};

/// a png in iphone points, a third of the pixels on most iphones, of the last of `frames`, which start at a key frame,
/// a lone key frame always comes out right, openh264 smears some of the phone's later frames so a longer run is only a
/// fallback for when the phone won't send a fresh key frame
pub fn picture(frames: &[Vec<u8>]) -> Result<Picture> {
    let rgb = points(frames)?;
    let mut png = Vec::new();
    rgb.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| Error::Other(e.to_string()))?;
    Ok(Picture {
        png,
        width: rgb.width(),
        height: rgb.height(),
    })
}

/// the last of `frames` scaled to iphone points, openh264 holds back one picture on anything but baseline video and
/// flushing mid-stream breaks it after a few dozen frames, so a fresh decoder takes the frames and flushes once at the
/// end, which is the use flushing is for
pub fn points(frames: &[Vec<u8>]) -> Result<RgbImage> {
    let failed = |e: openh264::Error| Error::Other(format!("Couldn't read the picture of the iPhone's screen: {e}"));
    let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    let mut decoder = Decoder::with_api_config(OpenH264API::from_source(), config).map_err(failed)?;
    for frame in frames {
        let _ = decoder.decode(frame);
    }
    let flushed = decoder.flush_remaining().map_err(failed)?;
    let last = flushed
        .last()
        .ok_or_else(|| Error::Other("The iPhone's screen picture was incomplete.".into()))?;
    Ok(to_points(last))
}

// averages each 3 x 3 block straight from the yuv planes, far less work than converting every pixel then shrinking
fn to_points(yuv: &impl YUVSource) -> RgbImage {
    let (full_width, full_height) = yuv.dimensions();
    let width = (full_width / 3).max(1);
    let height = (full_height / 3).max(1);
    let (ys, us, vs) = yuv.strides();
    let (y_plane, u_plane, v_plane) = (yuv.y(), yuv.u(), yuv.v());
    let mut out = RgbImage::new(width as u32, height as u32);
    for oy in 0..height {
        for ox in 0..width {
            let (x, y) = (ox * 3, oy * 3);
            let mut luma = 0u32;
            for dy in 0..3 {
                let row = (y + dy).min(full_height - 1) * ys;
                for dx in 0..3 {
                    luma += y_plane[row + (x + dx).min(full_width - 1)] as u32;
                }
            }
            let (cx, cy) = ((x + 1) / 2, (y + 1) / 2);
            let c = luma as f32 / 9.0 - 16.0;
            let d = u_plane[cy * us + cx] as f32 - 128.0;
            let e = v_plane[cy * vs + cx] as f32 - 128.0;
            let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
            out.put_pixel(
                ox as u32,
                oy as u32,
                image::Rgb([
                    clamp(1.164 * c + 1.596 * e),
                    clamp(1.164 * c - 0.392 * d - 0.813 * e),
                    clamp(1.164 * c + 2.017 * d),
                ]),
            );
        }
    }
    out
}
