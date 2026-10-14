use core::num::NonZeroUsize;

use ironrdp_core::{Encode as _, WriteCursor, cast_int, cast_length, invalid_field_err};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::rdp6::{
    ABgrChannels, ARgbChannels, BgrAChannels, BitmapEncodeError, BitmapStreamEncoder, RgbAChannels,
};
use ironrdp_pdu::bitmap::{self, BitmapData, BitmapUpdateData, Compression};
use ironrdp_pdu::geometry::InclusiveRectangle;

use crate::BitmapUpdate;

// PERF: we could also remove the need for this buffer
#[derive(Clone)]
pub(crate) struct BitmapEncoder {
    buffer: Vec<u8>,
}

impl BitmapEncoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; usize::from(u16::MAX)],
        }
    }

    pub(crate) fn encode(
        &mut self,
        bitmap: &BitmapUpdate,
        output: &mut [u8],
    ) -> Result<usize, BitmapEncodeError> {
        // FIXME: support non-multiple of 4 widths.
        //
        // It’s not clear how to achieve that yet, but generally, server uses multiple of 4-widths,
        // and client has surface capabilities, so this path is unlikely.
        if !bitmap.width.get().is_multiple_of(4) {
            return Err(BitmapEncodeError::Encode(invalid_field_err!(
                "bitmap",
                "Width must be a multiple of 4"
            )));
        }

        let bytes_per_pixel = u16::from(bitmap.format.bytes_per_pixel());
        let row_len = bitmap.width.get() * bytes_per_pixel;
        let chunk_height = u16::MAX / row_len;

        let mut cursor = WriteCursor::new(output);
        let stride = bitmap.stride.get();
        let chunks = bitmap.data.chunks(stride * usize::from(chunk_height));

        let total = cast_int!("chunks length lower bound", chunks.size_hint().0)
            .map_err(BitmapEncodeError::Encode)?;
        BitmapUpdateData::encode_header(total, &mut cursor).map_err(BitmapEncodeError::Encode)?;

        for (i, chunk) in chunks.enumerate() {
            // A subregion shares its parent's stride but its final row ends at
            // the subregion's right edge. Count that short final row as well.
            let height = cast_int!("bitmap height", chunk.len().div_ceil(stride))
                .map_err(BitmapEncodeError::Encode)?;
            let i: u16 = cast_int!("chunk idx", i).map_err(BitmapEncodeError::Encode)?;
            let top = bitmap.y + i * chunk_height;

            let encoder = BitmapStreamEncoder::new(
                NonZeroUsize::from(bitmap.width).get(),
                usize::from(height),
            );

            let len = {
                let pixels = chunk
                    .chunks(stride)
                    .map(|row| &row[..usize::from(row_len)])
                    .rev()
                    .flat_map(|row| row.chunks(usize::from(bytes_per_pixel)));

                Self::encode_iter(encoder, bitmap.format, pixels, self.buffer.as_mut_slice())?
            };

            let data = BitmapData {
                rectangle: InclusiveRectangle {
                    left: bitmap.x,
                    top,
                    right: bitmap.x + bitmap.width.get() - 1,
                    bottom: top + height - 1,
                },
                width: u16::from(bitmap.width),
                height,
                bits_per_pixel: u16::from(bitmap.format.bytes_per_pixel()) * 8,
                compression_flags: Compression::BITMAP_COMPRESSION,
                compressed_data_header: Some(bitmap::CompressedDataHeader {
                    main_body_size: cast_length!("main body size", len)
                        .map_err(BitmapEncodeError::Encode)?,
                    scan_width: u16::from(bitmap.width),
                    uncompressed_size: height * row_len,
                }),
                bitmap_data: &self.buffer[..len],
            };

            data.encode(&mut cursor)
                .map_err(BitmapEncodeError::Encode)?;
        }

        Ok(cursor.pos())
    }

    fn encode_iter<'a, P>(
        mut encoder: BitmapStreamEncoder,
        format: PixelFormat,
        src: P,
        dst: &mut [u8],
    ) -> Result<usize, BitmapEncodeError>
    where
        P: Iterator<Item = &'a [u8]> + Clone,
    {
        let written = match format {
            PixelFormat::ARgb32 | PixelFormat::XRgb32 => {
                encoder.encode_pixels_stream::<_, ARgbChannels>(src, dst, true)?
            }
            PixelFormat::RgbA32 | PixelFormat::RgbX32 => {
                encoder.encode_pixels_stream::<_, RgbAChannels>(src, dst, true)?
            }
            PixelFormat::ABgr32 | PixelFormat::XBgr32 => {
                encoder.encode_pixels_stream::<_, ABgrChannels>(src, dst, true)?
            }
            PixelFormat::BgrA32 | PixelFormat::BgrX32 => {
                encoder.encode_pixels_stream::<_, BgrAChannels>(src, dst, true)?
            }
        };

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::num::NonZeroU16;
    use ironrdp_graphics::rdp6::BitmapStreamDecoder;

    #[test]
    fn shared_stride_subregions_keep_their_final_wire_row() {
        let (width, height, stride) = (256usize, 400usize, 1024usize);
        let mut pixels = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let start = y * stride + x * 4;
                pixels[start..start + 4].copy_from_slice(&[
                    (x % 251) as u8,
                    (y % 251) as u8,
                    ((x + y) % 251) as u8,
                    255,
                ]);
            }
        }
        let parent = BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width as u16).unwrap(),
            height: NonZeroU16::new(height as u16).unwrap(),
            format: PixelFormat::BgrX32,
            data: pixels.clone().into(),
            stride: NonZeroUsize::new(stride).unwrap(),
        };
        // 300 rows also spans multiple 64KiB classic Bitmap Data rectangles.
        for rows in [1u16, 64, 256, 300] {
            let region = parent
                .sub(
                    64,
                    3,
                    NonZeroU16::new(64).unwrap(),
                    NonZeroU16::new(rows).unwrap(),
                )
                .unwrap();
            assert_eq!(region.data.len(), (usize::from(rows) - 1) * stride + 64 * 4);
            let mut encoded = vec![0u8; 512 * 1024];
            let length = BitmapEncoder::new().encode(&region, &mut encoded).unwrap();
            let pdu: BitmapUpdateData<'_> = ironrdp_core::decode(&encoded[..length]).unwrap();
            let mut actual = vec![0u8; usize::from(rows) * 64 * 3];
            let mut covered_rows = 0usize;
            for rectangle in pdu.rectangles {
                assert!(rectangle.height > 0);
                assert_eq!(rectangle.rectangle.left, 64);
                assert_eq!(rectangle.rectangle.right, 127);
                assert_eq!(
                    rectangle.rectangle.bottom,
                    rectangle.rectangle.top + rectangle.height - 1
                );
                let mut decoded = Vec::new();
                BitmapStreamDecoder::default()
                    .decode_bitmap_stream_to_rgb24(
                        rectangle.bitmap_data,
                        &mut decoded,
                        64,
                        usize::from(rectangle.height),
                    )
                    .unwrap();
                for row in 0..usize::from(rectangle.height) {
                    let target_row = usize::from(rectangle.rectangle.bottom) - row - 3;
                    actual[target_row * 64 * 3..(target_row + 1) * 64 * 3]
                        .copy_from_slice(&decoded[row * 64 * 3..(row + 1) * 64 * 3]);
                }
                covered_rows += usize::from(rectangle.height);
            }
            assert_eq!(covered_rows, usize::from(rows));
            let mut expected = Vec::new();
            for y in 3..usize::from(rows) + 3 {
                for x in 64..128 {
                    let source = &pixels[y * stride + x * 4..];
                    expected.extend_from_slice(&[source[2], source[1], source[0]]);
                }
            }
            assert_eq!(
                actual, expected,
                "classic bitmap wire data lost or reordered a row"
            );
        }
    }
}
