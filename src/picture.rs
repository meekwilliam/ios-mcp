use image::{ImageFormat, RgbImage, imageops};
use openh264::{
    decoder::{DecodedYUV, Decoder},
    formats::YUVSource,
};

use crate::{Error, Result};

pub struct Picture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// a key frame as a png in iphone points, a third of the pixels on most iphones, so the agent's coordinates are points
pub fn from_key_frame(frame: &[u8]) -> Result<Picture> {
    let failed = |e: openh264::Error| Error::Other(format!("Couldn't read the picture of the iPhone's screen: {e}"));
    let mut decoder = Decoder::new().map_err(failed)?;
    let rgb = {
        let mut decoded = decoder.decode(frame).map_err(failed)?.map(|yuv| to_rgb(&yuv));
        if decoded.is_none() {
            decoded = decoder.flush_remaining().map_err(failed)?.first().map(to_rgb);
        }
        decoded.ok_or_else(|| Error::Other("The iPhone's screen picture was incomplete.".into()))?
    };
    let width = (rgb.width() / 3).max(1);
    let height = (rgb.height() / 3).max(1);
    let small = imageops::resize(&rgb, width, height, imageops::FilterType::Triangle);
    let mut png = Vec::new();
    small
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| Error::Other(e.to_string()))?;
    Ok(Picture { png, width, height })
}

fn to_rgb(yuv: &DecodedYUV<'_>) -> RgbImage {
    let (width, height) = yuv.dimensions();
    let mut rgb = vec![0; width * height * 3];
    yuv.write_rgb8(&mut rgb);
    RgbImage::from_raw(width as u32, height as u32, rgb).expect("sized to fit")
}
