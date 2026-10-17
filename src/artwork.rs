//! Fitting the phone's cover art into the car's media message.
//!
//! The car reads the song and its cover from one command-channel message, and
//! the body of that message has a ceiling: 40 KB on the channel, with the
//! reference implementation keeping the cover under 30 KB so the rest of it
//! fits comfortably. The phone sends what the app has, and a 1200px JPEG of a
//! CD sleeve is ordinary — several hundred kilobytes of it. Sent as it arrives
//! that is a message the car refuses, and a refused length takes the channel
//! down with it, so a cover that will not fit is re-encoded here.
//!
//! Only a cover that is too large is touched at all. One that already fits is
//! handed on as the phone sent it, byte for byte: re-encoding a picture that was
//! going to be shown anyway costs the battery, and a second generation of JPEG
//! artefacts, for nothing.
//!
//! The ladder is six encodes at most: 400px at quality 90, 80, 70 and 60, then
//! 350px at 60, then 300px at 60. Quality is not started over at each size,
//! because a 350px picture at 90 is larger than a 400px one at 60 — that is the
//! ladder going back up. Each size is resampled once and encoded four times, so
//! the order matters: the picture is only scaled down when the quality has run
//! out.
//!
//! What comes out is a baseline JPEG with 4:2:0 chroma and no metadata of any
//! kind. The panel is small enough that the colour detail 4:2:0 gives up is not
//! visible on it, and dropping the metadata is the point rather than a side
//! effect: the phone's files carry EXIF, sometimes kilobytes of it. The one
//! thing in there that matters is the orientation tag, because a photo taken on
//! its side is stored on its side and meant to be turned by the tag — so it is
//! read, applied to the pixels, and only then does the metadata go.
//!
//! A picture smaller than the first size is never stretched: a 200px cover is
//! encoded at 200px, at the same four qualities, and the two smaller sizes are
//! skipped because they would be the same picture again.
//!
//! None of this runs on the CarLife side. A cover is a few megapixels of
//! decoding and up to six passes of encoding — tens of milliseconds on this
//! board, long enough to be felt in the video if it happened where the streams
//! are written. `crate::session` does the work in its own task and the CarLife
//! session only ever sees the result.

use std::fmt;
use std::io::Cursor;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_core::result::DecodingResult;
use zune_jpeg::JpegDecoder;
use zune_png::PngDecoder;

/// The longest edges a cover is brought down to, in the order they are tried.
const LONG_EDGES: [u32; 3] = [400, 350, 300];

/// The qualities tried at the first size. Every smaller size is tried at the
/// last of them only.
const QUALITIES: [u8; 4] = [90, 80, 70, 60];

/// The largest a cover's sides may be before it is not decoded at all.
///
/// 2048 is four megapixels, twelve megabytes of RGB: the most this board is
/// willing to hold for a picture it is about to shrink. Nothing that is a cover
/// is that large, and a file that claims to be is not one to spend the memory on
/// finding out.
const MAX_EDGE: u32 = 2048;

/// What became of one cover on its way to the car.
#[derive(Debug, Clone)]
pub enum Fitted {
    /// Already small enough, and handed on as the phone sent it.
    AsIs(Vec<u8>),
    /// Re-encoded into something the car's message can carry.
    Shrunk {
        bytes: Vec<u8>,
        /// The format it arrived in, which is not the one it leaves in.
        kind: Kind,
        /// Its size once it was made small enough.
        size: (u32, u32),
        quality: u8,
        /// How many encodes it took, which says how far down the ladder it fell.
        tries: u8,
        /// The EXIF orientation that was applied, and 1 when there was none.
        orientation: u8,
    },
    /// No re-encoding got it under the budget, so the car is not told about it.
    GaveUp {
        why: Why,
        /// The smallest attempt, when there was one worth reporting.
        best: Option<Attempt>,
        tries: u8,
    },
}

/// One encoding that was too large, kept to say how close it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub bytes: usize,
    pub size: (u32, u32),
    pub quality: u8,
}

/// The formats the phone's artwork arrives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
}

impl Kind {
    /// Which of the two a file is, by its own opening bytes, and `None` when it
    /// is neither.
    ///
    /// LIVI guesses between the two when it renders the phone's artwork and
    /// assumes PNG for anything that is not a JPEG; guessing is not worth it
    /// here. A file this cannot name is one this cannot read.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else {
            None
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        })
    }
}

/// Why a cover was left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Not a picture this knows how to read.
    Format,
    /// A picture too large to be worth decoding.
    Pixels,
    /// A picture that would not decode.
    Decode,
    /// Every size and quality was still too large.
    TooBig,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "format",
            Self::Pixels => "pixels",
            Self::Decode => "decode",
            Self::TooBig => "too-big",
        })
    }
}

/// A cover made small enough to travel in the car's media message.
///
/// `budget` is the most the picture itself may take, which is what the message
/// has left after the song's fields — see `crate::carlife::proto::MAX_ART`.
pub fn fit(bytes: &[u8], budget: usize) -> Fitted {
    if bytes.len() <= budget {
        return Fitted::AsIs(bytes.to_vec());
    }

    let Some(kind) = Kind::sniff(bytes) else {
        return Fitted::GaveUp {
            why: Why::Format,
            best: None,
            tries: 0,
        };
    };

    let picture = match decode(bytes, kind) {
        Ok(picture) => picture,
        Err(why) => {
            return Fitted::GaveUp {
                why,
                best: None,
                tries: 0,
            }
        }
    };

    // Only a JPEG is looked for the tag. PNG has an `eXIf` chunk that can carry
    // one, but the phone's artwork does not write it, and the tag is only ever
    // meant for the camera's own photos.
    let orientation = match kind {
        Kind::Jpeg => orientation(bytes),
        Kind::Png => 1,
    };
    let Picture {
        mut rgb,
        mut width,
        mut height,
    } = picture;
    if orientation != 1 {
        (rgb, width, height) = upright(rgb, width, height, orientation);
    }

    let mut tries = 0u8;
    let mut best: Option<Attempt> = None;
    let mut previous: Option<(u32, u32)> = None;

    for (step, &edge) in LONG_EDGES.iter().enumerate() {
        let (w, h) = fitted_size(width, height, edge);
        // A picture already smaller than the size being tried comes out the
        // same size again, and is not worth a second pass of encodes.
        if previous == Some((w, h)) {
            continue;
        }
        previous = Some((w, h));

        let resized;
        let pixels: &[u8] = if (w, h) == (width, height) {
            &rgb
        } else {
            resized = scale(&rgb, width, height, w, h);
            &resized
        };

        let qualities: &[u8] = if step == 0 {
            &QUALITIES
        } else {
            &QUALITIES[QUALITIES.len() - 1..]
        };
        for &quality in qualities {
            tries += 1;
            let Ok(encoded) = encode(pixels, w, h, quality) else {
                continue;
            };
            if encoded.len() <= budget {
                return Fitted::Shrunk {
                    bytes: encoded,
                    kind,
                    size: (w, h),
                    quality,
                    tries,
                    orientation,
                };
            }
            if best.is_none_or(|b| encoded.len() < b.bytes) {
                best = Some(Attempt {
                    bytes: encoded.len(),
                    size: (w, h),
                    quality,
                });
            }
        }
    }

    Fitted::GaveUp {
        why: Why::TooBig,
        best,
        tries,
    }
}

/// A picture in the one layout the rest of this works in: three bytes per pixel,
/// top row first.
struct Picture {
    rgb: Vec<u8>,
    width: u32,
    height: u32,
}

fn decode(bytes: &[u8], kind: Kind) -> Result<Picture, Why> {
    match kind {
        Kind::Jpeg => decode_jpeg(bytes),
        Kind::Png => decode_png(bytes),
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<Picture, Why> {
    // Asked for RGB outright: the phone's covers are also greyscale and CMYK,
    // and the decoder knows how to bring either of those to three channels.
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|_| Why::Decode)?;

    let (width, height) = decoder.dimensions().ok_or(Why::Decode)?;
    let (width, height) = (width as u32, height as u32);
    if width > MAX_EDGE || height > MAX_EDGE {
        return Err(Why::Pixels);
    }

    let rgb = decoder.decode().map_err(|_| Why::Decode)?;
    if rgb.len() != (width as usize * height as usize * 3) {
        return Err(Why::Decode);
    }
    Ok(Picture { rgb, width, height })
}

fn decode_png(bytes: &[u8]) -> Result<Picture, Why> {
    // Sixteen bits per channel is more than a cover needs and more than the
    // encoder below can take, so the decoder is asked to keep the top eight.
    let options = DecoderOptions::default().png_set_strip_to_8bit(true);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|_| Why::Decode)?;

    let (width, height) = decoder.dimensions().ok_or(Why::Decode)?;
    let (width, height) = (width as u32, height as u32);
    if width > MAX_EDGE || height > MAX_EDGE {
        return Err(Why::Pixels);
    }

    let space = decoder.colorspace().ok_or(Why::Decode)?;
    let samples = match decoder.decode().map_err(|_| Why::Decode)? {
        DecodingResult::U8(samples) => samples,
        // Asking to strip to eight bits means this does not happen; a sixteen
        // bit cover is not worth a second path through here.
        _ => return Err(Why::Decode),
    };
    let channels = space.num_components();
    if channels == 0 || samples.len() != width as usize * height as usize * channels {
        return Err(Why::Decode);
    }

    Ok(Picture {
        rgb: to_rgb(&samples, channels),
        width,
        height,
    })
}

/// The decoder's samples as three bytes per pixel, with any alpha laid over
/// black.
///
/// The car's picture has no transparency in it, and a cover is drawn on a dark
/// panel: a logo with a transparent background belongs on black rather than on
/// whatever the decoder happened to leave behind it.
fn to_rgb(samples: &[u8], channels: usize) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(samples.len() / channels * 3);
    for pixel in samples.chunks_exact(channels) {
        let (r, g, b, a) = match channels {
            1 => (pixel[0], pixel[0], pixel[0], 255),
            2 => (pixel[0], pixel[0], pixel[0], pixel[1]),
            4 => (pixel[0], pixel[1], pixel[2], pixel[3]),
            _ => (pixel[0], pixel[1], pixel[2], 255),
        };
        let over = |v: u8| ((u32::from(v) * u32::from(a) + 127) / 255) as u8;
        rgb.extend_from_slice(&[over(r), over(g), over(b)]);
    }
    rgb
}

/// Which way up the phone says the picture goes, from the one EXIF tag that says
/// so.
///
/// 1 is "as stored", and so is a file with no EXIF at all — which is what a
/// reader that fails on the metadata, or finds none, is taken to mean. A value
/// outside 1–8 is not one the standard defines, and is ignored the same way.
fn orientation(bytes: &[u8]) -> u8 {
    let exif = exif::Reader::new().read_from_container(&mut Cursor::new(bytes));
    exif.ok()
        .and_then(|exif| {
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|field| field.value.get_uint(0))
        })
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1) as u8
}

/// The picture turned the way the tag says it is meant to be seen.
///
/// The eight cases are the standard's: the four that are a mirror or a half turn
/// keep the picture's shape, and the four that are a quarter turn swap its
/// sides. That swap is why this happens before the picture is made smaller — the
/// long edge the sizes are counted along is the one it is seen with, not the one
/// it is stored with.
fn upright(src: Vec<u8>, width: u32, height: u32, orientation: u8) -> (Vec<u8>, u32, u32) {
    let (out_w, out_h) = if orientation >= 5 {
        (height, width)
    } else {
        (width, height)
    };
    let mut out = vec![0u8; src.len()];
    for y in 0..out_h {
        for x in 0..out_w {
            let (sx, sy) = match orientation {
                2 => (width - 1 - x, y),
                3 => (width - 1 - x, height - 1 - y),
                4 => (x, height - 1 - y),
                5 => (y, x),
                6 => (y, height - 1 - x),
                7 => (width - 1 - y, height - 1 - x),
                8 => (width - 1 - y, x),
                _ => (x, y),
            };
            let from = ((sy * width + sx) * 3) as usize;
            let to = ((y * out_w + x) * 3) as usize;
            out[to..to + 3].copy_from_slice(&src[from..from + 3]);
        }
    }
    (out, out_w, out_h)
}

/// The size a picture takes with its long edge brought down to `edge`.
///
/// A picture that is already smaller is left at its own size: a cover is never
/// stretched to fill the panel.
fn fitted_size(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= edge {
        return (width, height);
    }
    let along = |v: u32| ((u64::from(v) * u64::from(edge) / u64::from(long)) as u32).max(1);
    if width >= height {
        (edge, along(height))
    } else {
        (along(width), edge)
    }
}

/// The picture at another size, each pixel the average of the ones it covers.
///
/// A box filter rather than a sample of the nearest source pixel: a cover
/// arrives several times larger than it is going, and picking one pixel out of
/// each block of nine is what leaves a photograph shimmering where it should be
/// smooth.
fn scale(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 3) as usize];
    for y in 0..dh {
        let top = (u64::from(y) * u64::from(sh) / u64::from(dh)) as u32;
        let bottom = (((u64::from(y + 1) * u64::from(sh)) / u64::from(dh)) as u32)
            .max(top + 1)
            .min(sh);
        for x in 0..dw {
            let left = (u64::from(x) * u64::from(sw) / u64::from(dw)) as u32;
            let right = (((u64::from(x + 1) * u64::from(sw)) / u64::from(dw)) as u32)
                .max(left + 1)
                .min(sw);
            let mut sum = [0u32; 3];
            let mut count = 0u32;
            for sy in top..bottom {
                for sx in left..right {
                    let at = ((sy * sw + sx) * 3) as usize;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(src[at + channel]);
                    }
                    count += 1;
                }
            }
            let to = ((y * dw + x) * 3) as usize;
            for (channel, total) in sum.iter().enumerate() {
                out[to + channel] = ((total + count / 2) / count) as u8;
            }
        }
    }
    out
}

/// The picture as a baseline JPEG with 4:2:0 chroma and nothing else in it.
///
/// Baseline and not progressive because the car's decoder is the one that has to
/// read it, and 4:2:0 because the panel cannot show what the other sampling
/// modes keep. No EXIF, no colour profile, no comment: the encoder writes none
/// of them unless it is handed one.
fn encode(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(rgb, width as u16, height as u16, ColorType::Rgb)
        .map_err(|_| ())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zune_core::bit_depth::BitDepth;

    /// The most a cover may take, as the CarLife side counts it.
    const BUDGET: usize = 30 * 1024;

    /// A picture that is smooth everywhere and so compresses to almost nothing:
    /// what it is here to prove is the size it comes out at, not the bytes.
    fn smooth(width: u32, height: u32) -> Vec<u8> {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                rgb.push((x * 255 / width) as u8);
                rgb.push((y * 255 / height) as u8);
                rgb.push(((x + y) * 255 / (width + height)) as u8);
            }
        }
        rgb
    }

    /// A picture with no structure at all, which is the one thing that will not
    /// compress however small it is made.
    fn noise(width: u32, height: u32) -> Vec<u8> {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        let mut state = 0x1234_5678u32;
        for _ in 0..(width * height * 3) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            rgb.push((state >> 24) as u8);
        }
        rgb
    }

    fn jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
        encode(rgb, width, height, quality).expect("a picture to encode")
    }

    /// A TIFF block with one IFD entry in it: the orientation, and nothing else.
    fn tiff(orientation: u16) -> Vec<u8> {
        let mut block = Vec::new();
        block.extend_from_slice(b"II\x2a\x00");
        block.extend_from_slice(&8u32.to_le_bytes());
        block.extend_from_slice(&1u16.to_le_bytes());
        block.extend_from_slice(&0x0112u16.to_le_bytes());
        block.extend_from_slice(&3u16.to_le_bytes());
        block.extend_from_slice(&1u32.to_le_bytes());
        block.extend_from_slice(&orientation.to_le_bytes());
        block.extend_from_slice(&[0, 0]);
        block.extend_from_slice(&0u32.to_le_bytes());
        block
    }

    fn jpeg_with_orientation(rgb: &[u8], width: u32, height: u32, which: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = Encoder::new(&mut out, 100);
        encoder
            .add_exif_metadata(&tiff(which))
            .expect("room for the metadata");
        encoder
            .encode(rgb, width as u16, height as u16, ColorType::Rgb)
            .expect("a picture to encode");
        out
    }

    /// The markers of a JPEG, up to the scan, as (marker, payload).
    fn segments(bytes: &[u8]) -> Vec<(u8, &[u8])> {
        let mut found = Vec::new();
        let mut at = 2;
        while at + 4 <= bytes.len() {
            assert_eq!(bytes[at], 0xff, "a marker where one was expected");
            let marker = bytes[at + 1];
            if marker == 0xda {
                break;
            }
            let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
            found.push((marker, &bytes[at + 4..at + 2 + len]));
            at += 2 + len;
        }
        found
    }

    /// What a start-of-frame segment says: its size, and the sampling factors of
    /// the first component.
    fn frame(payload: &[u8]) -> (u32, u32, u8) {
        let height = u32::from(u16::from_be_bytes([payload[1], payload[2]]));
        let width = u32::from(u16::from_be_bytes([payload[3], payload[4]]));
        (width, height, payload[6 + 1])
    }

    /// A cover that is already small enough is handed on as the phone sent it,
    /// and never looked inside — these four bytes are not even a whole JPEG.
    #[test]
    fn a_cover_that_fits_is_handed_on_untouched() {
        let cover = vec![0xff, 0xd8, 0xff, 0xe0];
        match fit(&cover, BUDGET) {
            Fitted::AsIs(bytes) => assert_eq!(bytes, cover),
            other => panic!("expected the phone's own bytes, got {other:?}"),
        }
    }

    /// A cover too large for the message comes out as a baseline JPEG with 4:2:0
    /// chroma, no metadata, and a long edge of no more than 400.
    #[test]
    fn a_cover_too_large_comes_out_small_enough_to_send() {
        let cover = jpeg(&smooth(1200, 1200), 1200, 1200, 100);
        assert!(cover.len() > BUDGET, "a cover worth shrinking");

        let Fitted::Shrunk {
            bytes,
            kind,
            size,
            tries,
            orientation,
            ..
        } = fit(&cover, BUDGET)
        else {
            panic!("a cover that should have been shrunk");
        };

        assert_eq!(kind, Kind::Jpeg);
        assert!(bytes.len() <= BUDGET, "{} bytes", bytes.len());
        // A picture that fits at the first size is not taken any further down.
        assert_eq!(size, (400, 400));
        assert_eq!(tries, 1);
        assert_eq!(orientation, 1);

        let segments = segments(&bytes);
        assert!(
            segments.iter().any(|(marker, _)| *marker == 0xc0),
            "a baseline frame"
        );
        assert!(
            !segments.iter().any(|(marker, _)| *marker == 0xc2),
            "no progressive frame"
        );
        // APP1 is EXIF, APP2 is the colour profile, and a comment is neither.
        for marker in [0xe1, 0xe2, 0xfe] {
            assert!(
                !segments.iter().any(|(found, _)| *found == marker),
                "no {marker:#04x} segment"
            );
        }

        let (width, height, sampling) = frame(
            segments
                .iter()
                .find(|(marker, _)| *marker == 0xc0)
                .map(|(_, payload)| *payload)
                .expect("a frame"),
        );
        assert_eq!((width, height), size);
        assert_eq!(sampling, 0x22, "4:2:0 luma sampling");
    }

    /// A picture that will not fit at any size is given up on rather than sent,
    /// and the six encodes are all the ladder has.
    #[test]
    fn a_cover_that_will_not_fit_is_given_up_on() {
        let cover = jpeg(&noise(1200, 1200), 1200, 1200, 100);
        // A third of the real budget, so that the answer does not turn on how
        // many bytes the encoder leaves a picture of pure noise: every rung is
        // far above this one.
        let budget = BUDGET / 3;
        let Fitted::GaveUp { why, best, tries } = fit(&cover, budget) else {
            panic!("noise should not fit in a third of 30 KB");
        };
        assert_eq!(why, Why::TooBig);
        // Four qualities at 400, then one each at 350 and 300.
        assert_eq!(tries, 6);

        let best = best.expect("an attempt to report");
        assert_eq!(best.size, (300, 300));
        assert_eq!(best.quality, 60);
        assert!(best.bytes > budget);
    }

    /// A picture stored on its side is turned before it is made smaller, so the
    /// long edge the ladder counts along is the one it is seen with.
    #[test]
    fn a_cover_is_turned_upright_before_it_is_made_smaller() {
        let cover = jpeg_with_orientation(&smooth(1200, 600), 1200, 600, 6);
        assert!(cover.len() > BUDGET, "a cover worth shrinking");
        assert_eq!(orientation(&cover), 6);

        let Fitted::Shrunk {
            size, orientation, ..
        } = fit(&cover, BUDGET)
        else {
            panic!("a cover that should have been shrunk");
        };

        assert_eq!(orientation, 6);
        // Turned, it is 600 wide and 1200 tall, so 400 along the long edge is
        // 200 across.
        assert_eq!(size, (200, 400));
    }

    /// A cover the phone sent as PNG goes the same way, and comes out as the
    /// same kind of JPEG.
    #[test]
    fn a_png_cover_is_shrunk_like_a_jpeg() {
        let cover = png(&smooth(1200, 1200), 1200, 1200, ColorSpace::RGB);
        assert!(cover.len() > BUDGET, "a cover worth shrinking");

        let Fitted::Shrunk { bytes, kind, .. } = fit(&cover, BUDGET) else {
            panic!("a cover that should have been shrunk");
        };
        assert_eq!(kind, Kind::Png);
        assert!(bytes.len() <= BUDGET);
        assert!(
            bytes.starts_with(&[0xff, 0xd8, 0xff]),
            "a JPEG, whatever it was"
        );
    }

    /// A file that is not a picture, or is one too large to be worth decoding, is
    /// refused before anything is allocated for it.
    #[test]
    fn a_cover_that_is_not_one_is_refused() {
        let not_a_picture = vec![0x42; 40 * 1024];
        assert!(matches!(
            fit(&not_a_picture, BUDGET),
            Fitted::GaveUp {
                why: Why::Format,
                best: None,
                ..
            }
        ));

        // A budget of nothing, so that the file is looked at rather than handed
        // on: what it says about its size is the point of it.
        let too_many_pixels = png_claiming(&noise(8, 8), 8, 8, ColorSpace::RGB, 4000, 4000);
        assert!(matches!(
            fit(&too_many_pixels, 1),
            Fitted::GaveUp {
                why: Why::Pixels,
                ..
            }
        ));
    }

    /// A picture with transparency in it is laid over black, which is what the
    /// car's panel is behind it.
    #[test]
    fn transparency_is_laid_over_black() {
        assert_eq!(to_rgb(&[255, 0, 0, 0], 4), vec![0, 0, 0]);
        assert_eq!(to_rgb(&[0, 255, 0, 255], 4), vec![0, 255, 0]);
        assert_eq!(to_rgb(&[0, 0, 255, 128], 4), vec![0, 0, 128]);
        // Grey, which has no colour of its own to keep.
        assert_eq!(to_rgb(&[10, 20], 1), vec![10, 10, 10, 20, 20, 20]);
    }

    fn png(samples: &[u8], width: u32, height: u32, space: ColorSpace) -> Vec<u8> {
        let mut out = Vec::new();
        let options = zune_core::options::EncoderOptions::new(
            width as usize,
            height as usize,
            space,
            BitDepth::Eight,
        );
        zune_png::PngEncoder::new(samples, options)
            .encode(&mut out)
            .expect("a picture to encode");
        out
    }

    /// A PNG whose header claims a size the file does not have, which is the
    /// only way to be asked to decode a picture this large without allocating it.
    fn png_claiming(
        samples: &[u8],
        width: u32,
        height: u32,
        space: ColorSpace,
        claim_w: u32,
        claim_h: u32,
    ) -> Vec<u8> {
        let mut out = png(samples, width, height, space);
        out[16..20].copy_from_slice(&claim_w.to_be_bytes());
        out[20..24].copy_from_slice(&claim_h.to_be_bytes());
        let crc = crc32(&out[12..29]);
        out[29..33].copy_from_slice(&crc.to_be_bytes());
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ 0xedb8_8320
                };
            }
        }
        !crc
    }
}
