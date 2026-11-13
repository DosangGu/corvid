use std::time::Duration;

use anyhow::{Result, bail, ensure};
use corvid_deck::{COLUMNS, KEY_COUNT};
use hidapi::{HidApi, HidDevice, HidError};
use image::{
    DynamicImage, ExtendedColorType,
    codecs::{bmp::BmpEncoder, jpeg::JpegEncoder},
};

const ELGATO_VENDOR_ID: u16 = 0x0fd9;
pub const KEY_IMAGE_SIZE: u32 = 72;
const CLASSIC_REPORT_SIZE: usize = 1024;
const CLASSIC_IMAGE_HEADER_SIZE: usize = 8;
const ORIGINAL_REPORT_SIZE: usize = 8191;
const ORIGINAL_IMAGE_HEADER_SIZE: usize = 16;
// A 54-byte BMP header followed by 24-bit pixels. Each row is already aligned.
const ORIGINAL_BMP_SIZE: usize = 54 + (KEY_IMAGE_SIZE as usize).pow(2) * 3;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum Model {
    Original = 0x0060,
    OriginalV2 = 0x006d,
    Mk2 = 0x0080,
    Mk2Scissor = 0x00a5,
    Mk2Module = 0x00b9,
}

impl Model {
    fn from_product_id(product_id: u16) -> Option<Self> {
        [
            Self::Original,
            Self::OriginalV2,
            Self::Mk2,
            Self::Mk2Scissor,
            Self::Mk2Module,
        ]
        .into_iter()
        .find(|&model| model as u16 == product_id)
    }
}

pub fn supported_devices(api: &HidApi) -> Vec<(Model, String)> {
    let mut devices: Vec<_> = api
        .device_list()
        .filter(|device| device.vendor_id() == ELGATO_VENDOR_ID)
        .filter_map(|device| {
            let model = Model::from_product_id(device.product_id())?;
            let serial = device.serial_number().filter(|serial| !serial.is_empty())?;
            Some((model, serial.to_owned()))
        })
        .collect();
    devices.sort_unstable_by(|(left_model, left_serial), (right_model, right_serial)| {
        left_serial
            .cmp(right_serial)
            .then(left_model.cmp(right_model))
    });
    devices.dedup();
    devices
}

pub struct StreamDeck {
    model: Model,
    device: HidDevice,
}

impl StreamDeck {
    pub fn connect(api: &HidApi, model: Model, serial: &str) -> Result<Self> {
        let device = api.open_serial(ELGATO_VENDOR_ID, model as u16, serial)?;
        Ok(Self { model, device })
    }

    pub fn read_keys(&self, timeout: Duration) -> Result<Option<[bool; KEY_COUNT]>> {
        let mut report = [0; 512];
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let len = self.device.read_timeout(&mut report, timeout_ms)?;
        if len == 0 {
            return Ok(None);
        }
        decode_keys(self.model, &report[..len]).map(Some)
    }

    pub fn write_key_images(
        &self,
        images: impl Iterator<Item = Result<(usize, DynamicImage)>>,
    ) -> Result<()> {
        let encoded = images
            .map(|image| {
                let (key, image) = image?;
                ensure!(key < KEY_COUNT, "invalid Stream Deck key index {key}");
                Ok((key, encode_image(self.model, image)?))
            })
            .collect::<Result<Vec<_>>>()?;

        for (key, image) in encoded {
            let send = |report: &[u8]| self.write_report(report);
            match self.model {
                Model::Original => write_original_image(key, &image, send)?,
                _ => write_classic_image(key, &image, send)?,
            }
        }
        Ok(())
    }

    fn write_report(&self, report: &[u8]) -> Result<()> {
        let sent = self.device.write(report)?;
        if sent < report.len() {
            return Err(HidError::IncompleteSendError {
                sent,
                all: report.len(),
            }
            .into());
        }
        Ok(())
    }
}

// The 2017 Original uses right-to-left key indices within each row.
fn original_key_index(key: usize) -> usize {
    let row_start = key / COLUMNS * COLUMNS;
    row_start + (COLUMNS - 1 - key % COLUMNS)
}

fn decode_keys(model: Model, report: &[u8]) -> Result<[bool; KEY_COUNT]> {
    if model == Model::Original {
        let Some([0x01, keys @ ..]) = report.get(..1 + KEY_COUNT) else {
            bail!("invalid Stream Deck Original key report");
        };
        return Ok(std::array::from_fn(|key| {
            keys[original_key_index(key)] != 0
        }));
    }

    // Elgato Main HID protocol: report ID, command, little-endian payload size.
    let Some([0x01, 0x00, len_lo, len_hi, keys @ ..]) = report.get(..4 + KEY_COUNT) else {
        bail!("invalid Stream Deck Classic key report");
    };
    ensure!(
        usize::from(u16::from_le_bytes([*len_lo, *len_hi])) == KEY_COUNT
            && keys.iter().all(|&level| level <= 1),
        "invalid Stream Deck Classic key payload"
    );
    Ok(std::array::from_fn(|key| keys[key] == 1))
}

fn encode_image(model: Model, image: DynamicImage) -> Result<Vec<u8>> {
    ensure!(
        image.width() == KEY_IMAGE_SIZE && image.height() == KEY_IMAGE_SIZE,
        "invalid Stream Deck key image size"
    );
    let pixels = image.rotate180().into_rgb8();
    let mut bytes = Vec::new();
    let (width, height) = pixels.dimensions();
    match model {
        Model::Original => BmpEncoder::new(&mut bytes).encode(
            pixels.as_raw(),
            width,
            height,
            ExtendedColorType::Rgb8,
        )?,
        _ => JpegEncoder::new_with_quality(&mut bytes, 90).encode(
            pixels.as_raw(),
            width,
            height,
            ExtendedColorType::Rgb8,
        )?,
    }
    Ok(bytes)
}

// https://docs.elgato.com/streamdeck/hid/stream-deck-classic/#update-key-image
fn write_classic_image(
    key: usize,
    image: &[u8],
    mut send: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let chunks = image.chunks(CLASSIC_REPORT_SIZE - CLASSIC_IMAGE_HEADER_SIZE);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let mut report = [0; CLASSIC_REPORT_SIZE];
        let last = u8::from(index + 1 == count);
        report[..4].copy_from_slice(&[0x02, 0x07, key as u8, last]);
        report[4..6].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
        report[6..CLASSIC_IMAGE_HEADER_SIZE].copy_from_slice(&(index as u16).to_le_bytes());
        report[CLASSIC_IMAGE_HEADER_SIZE..][..chunk.len()].copy_from_slice(chunk);
        send(&report)?;
    }
    Ok(())
}

// The 2017 Original sends the fixed-size 24-bit BMP as two equal pages.
// https://github.com/abcminiuser/python-elgato-streamdeck/blob/master/src/StreamDeck/Devices/StreamDeckOriginal.py
fn write_original_image(
    key: usize,
    image: &[u8],
    mut send: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    ensure!(
        image.len() == ORIGINAL_BMP_SIZE,
        "invalid Stream Deck Original BMP length"
    );
    let wire_key = (original_key_index(key) + 1) as u8;
    for (index, chunk) in image.chunks(ORIGINAL_BMP_SIZE / 2).enumerate() {
        let mut report = [0; ORIGINAL_REPORT_SIZE];
        let page = (index + 1) as u8;
        let last = u8::from(page == 2);
        report[..6].copy_from_slice(&[0x02, 0x01, page, 0, last, wire_key]);
        report[ORIGINAL_IMAGE_HEADER_SIZE..][..chunk.len()].copy_from_slice(chunk);
        send(&report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_key_report_preserves_order_and_ignores_padding() {
        let mut report = [0; 64];
        report[..4].copy_from_slice(&[1, 0, 15, 0]);
        report[4] = 1;
        report[18] = 1;
        report[19] = 0xff;

        let keys = decode_keys(Model::Mk2, &report).unwrap();
        assert!(keys[0]);
        assert!(keys[14]);
        assert!(keys[1..14].iter().all(|pressed| !pressed));
    }

    #[test]
    fn rejects_malformed_classic_key_reports() {
        let mut report = [0; 19];
        report[..4].copy_from_slice(&[1, 0, 15, 0]);
        assert!(decode_keys(Model::Mk2, &report[..18]).is_err());
        for (offset, value) in [(0, 2), (1, 1), (2, 14), (3, 1), (18, 2)] {
            let mut invalid = report;
            invalid[offset] = value;
            assert!(decode_keys(Model::Mk2, &invalid).is_err());
        }
    }

    #[test]
    fn original_key_report_reverses_each_row() {
        let mut report = [0; 16];
        report[0] = 1;
        report[1] = 1;
        report[15] = 1;

        let keys = decode_keys(Model::Original, &report).unwrap();
        assert!(keys[4]);
        assert!(keys[10]);
        assert_eq!(keys.into_iter().filter(|&pressed| pressed).count(), 2);
        assert!(decode_keys(Model::Original, &report[..15]).is_err());
    }

    #[test]
    fn classic_image_reports_encode_length_page_and_last_flag() {
        let image = vec![0xab; 1018];
        let mut reports = Vec::new();
        write_classic_image(14, &image, |report| {
            reports.push(report.to_vec());
            Ok(())
        })
        .unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].len(), 1024);
        assert_eq!(reports[1].len(), 1024);
        assert_eq!(&reports[0][..8], &[2, 7, 14, 0, 0xf8, 3, 0, 0]);
        assert_eq!(&reports[1][..8], &[2, 7, 14, 1, 2, 0, 1, 0]);
        assert_eq!(&reports[0][8..], &image[..1016]);
        assert_eq!(&reports[1][8..10], &image[1016..]);
        assert!(reports[1][10..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn original_sends_a_24_bit_bitmap_in_two_pages_to_the_mirrored_key() {
        let tile = DynamicImage::new_rgb8(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE);
        let image = encode_image(Model::Original, tile).unwrap();
        let mut reports = Vec::new();
        write_original_image(14, &image, |report| {
            reports.push(report.to_vec());
            Ok(())
        })
        .unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].len(), 8191);
        assert_eq!(reports[1].len(), 8191);
        assert_eq!(&reports[0][..6], &[2, 1, 1, 0, 0, 11]);
        assert_eq!(&reports[1][..6], &[2, 1, 2, 0, 1, 11]);
        assert_eq!(&reports[0][16..18], b"BM");
        assert_eq!(&reports[0][44..46], &[24, 0]);
        let half = image.len() / 2;
        assert_eq!(&reports[0][16..16 + half], &image[..half]);
        assert_eq!(&reports[1][16..16 + half], &image[half..]);
        assert!(reports[1][16 + half..].iter().all(|&byte| byte == 0));
    }
}
