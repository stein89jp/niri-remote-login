//! Bounded CF_DIB/CF_DIBV5 <-> PNG conversion for the private clipboard.
//! Every size and dimension is checked before any allocation that depends
//! on peer-controlled values.

/// Upper bound for an encoded PNG, a packed DIB, and decoded RGBA pixels.
pub(crate) const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// A packed DIB is its pixels plus a header, masks and at most 256 colours.
pub(crate) const MAX_DIB_BYTES: usize = MAX_IMAGE_BYTES + 4096;
const MAX_PIXELS: usize = MAX_IMAGE_BYTES / 4;
const MAX_DIMENSION: u32 = 32768;
pub(crate) const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Straight-alpha RGBA8 pixels, top row first.
#[derive(Debug, PartialEq)]
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

fn pixel_count(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    let count = (width as usize).checked_mul(height as usize)?;
    (count <= MAX_PIXELS).then_some(count)
}

fn u16_le(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}
fn u32_le(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}
fn u32_be(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// One BI_BITFIELDS channel: its mask position and width.
#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    bits: u32,
}
impl Channel {
    fn new(mask: u32) -> Self {
        Self {
            mask,
            shift: if mask == 0 { 0 } else { mask.trailing_zeros() },
            bits: mask.count_ones(),
        }
    }
    fn get(self, value: u32) -> u8 {
        if self.bits == 0 {
            return 0;
        }
        let raw = (value & self.mask) >> self.shift;
        if self.bits >= 8 {
            (raw >> (self.bits - 8)) as u8
        } else {
            let max = (1u32 << self.bits) - 1;
            ((raw * 255 + max / 2) / max) as u8
        }
    }
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

/// Decodes a packed CF_DIB or CF_DIBV5: BITMAPINFOHEADER through
/// BITMAPV5HEADER, bottom-up or top-down rows, 1/4/8-bit palettes and
/// 16/24/32-bit BI_RGB or BI_BITFIELDS pixels. Compressed DIBs are refused.
pub(crate) fn decode_dib(data: &[u8]) -> Option<Rgba> {
    if data.len() > MAX_DIB_BYTES {
        return None;
    }
    let header = u32_le(data, 0)? as usize;
    if !matches!(header, 40 | 52 | 56 | 108 | 124) || data.len() < header {
        return None;
    }
    let width = i32::from_le_bytes(data[4..8].try_into().ok()?);
    let raw_height = i32::from_le_bytes(data[8..12].try_into().ok()?);
    let planes = u16_le(data, 12)?;
    let bits = u16_le(data, 14)? as u32;
    let compression = u32_le(data, 16)?;
    let colors_used = u32_le(data, 32)? as usize;
    if planes != 1 || width <= 0 || raw_height == 0 || raw_height == i32::MIN {
        return None;
    }
    let top_down = raw_height < 0;
    let (width, height) = (width as u32, raw_height.unsigned_abs());
    let count = pixel_count(width, height)?;
    let bitfields = matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS);
    if !(compression == BI_RGB || bitfields && matches!(bits, 16 | 32)) {
        return None;
    }
    let mut offset = header;
    // Masks always start right after the 40-byte core; larger headers carry
    // them inside, the 40-byte header has them appended.
    let masks = if bitfields {
        let alpha_present = compression == BI_ALPHABITFIELDS || header >= 56;
        if header == 40 {
            offset += if compression == BI_ALPHABITFIELDS {
                16
            } else {
                12
            };
        }
        Some([
            u32_le(data, 40)?,
            u32_le(data, 44)?,
            u32_le(data, 48)?,
            if alpha_present { u32_le(data, 52)? } else { 0 },
        ])
    } else {
        None
    };
    let palette = match bits {
        1 | 4 | 8 => {
            let entries = if colors_used == 0 {
                1 << bits
            } else {
                colors_used
            };
            if entries > 1 << bits {
                return None;
            }
            let table = data.get(offset..offset + entries * 4)?;
            offset += entries * 4;
            table
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| [c[2], c[1], c[0]])
                .collect()
        }
        16 | 24 | 32 => {
            // An optional optimisation palette may precede true-colour pixels.
            if colors_used > 256 {
                return None;
            }
            offset += colors_used * 4;
            Vec::new()
        }
        _ => return None,
    };
    let stride = (width as usize * bits as usize).div_ceil(32) * 4;
    let size = stride.checked_mul(height as usize)?;
    // Some writers append BI_BITFIELDS masks after a V4/V5 header too.
    if bitfields && header > 40 && data.len() == offset + 12 + size {
        offset += 12;
    }
    let rows = data.get(offset..offset.checked_add(size)?)?;
    let channels = match (bits, masks) {
        (_, Some(masks)) => Some(masks.map(Channel::new)),
        (16, None) => Some([0x7c00, 0x03e0, 0x001f, 0].map(Channel::new)),
        _ => None,
    };
    let mut pixels = vec![0u8; count * 4];
    let mut alpha_seen = false;
    let alpha_source = bits == 32 && masks.is_none_or(|m| m[3] != 0);
    for y in 0..height as usize {
        let source = if top_down { y } else { height as usize - 1 - y };
        let row = &rows[source * stride..source * stride + stride];
        let out = &mut pixels[y * width as usize * 4..(y + 1) * width as usize * 4];
        for (x, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let rgba = match bits {
                32 => {
                    let value = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().unwrap());
                    match channels {
                        Some([r, g, b, a]) => {
                            [r.get(value), g.get(value), b.get(value), a.get(value)]
                        }
                        None => [row[x * 4 + 2], row[x * 4 + 1], row[x * 4], row[x * 4 + 3]],
                    }
                }
                24 => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255],
                16 => {
                    let value = u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]) as u32;
                    let [r, g, b, a] = channels?;
                    let alpha = if a.bits == 0 { 255 } else { a.get(value) };
                    [r.get(value), g.get(value), b.get(value), alpha]
                }
                _ => {
                    let bit = x * bits as usize;
                    let byte = row[bit / 8];
                    let index = (byte >> (8 - bits as usize - bit % 8)) & ((1 << bits) - 1) as u8;
                    let [r, g, b] = *palette.get(index as usize)?;
                    [r, g, b, 255]
                }
            };
            alpha_seen |= bits == 32 && rgba[3] != 0;
            *pixel = rgba;
        }
    }
    // 32-bit BI_RGB leaves the fourth byte reserved; Windows commonly fills
    // it with zero. Only an alpha channel with any content is honoured.
    if !alpha_source || !alpha_seen {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Some(Rgba {
        width,
        height,
        pixels,
    })
}

/// Encodes a bottom-up 32-bit BI_RGB CF_DIB, the format every Windows
/// application reads and from which Windows synthesises CF_DIBV5/CF_BITMAP.
pub(crate) fn encode_dib(image: &Rgba) -> Vec<u8> {
    let row = image.width as usize * 4;
    let mut out = Vec::with_capacity(40 + row * image.height as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&((row * image.height as usize) as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for line in image.pixels.chunks_exact(row).rev() {
        for p in line.as_chunks::<4>().0 {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}
static CRC_TABLE: [u32; 256] = crc_table();
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for part in parts {
        for &byte in *part {
            crc = CRC_TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = (
        (p - a as i16).abs(),
        (p - b as i16).abs(),
        (p - c as i16).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Encodes 8-bit RGB (opaque input) or RGBA with per-row adaptive filters.
pub(crate) fn encode_png(image: &Rgba) -> Vec<u8> {
    let opaque = image.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
    let channels = if opaque { 3 } else { 4 };
    let row = image.width as usize * channels;
    let mut raw = Vec::with_capacity((row + 1) * image.height as usize);
    let mut previous = vec![0u8; row];
    let mut current = Vec::with_capacity(row);
    let mut candidate = vec![0u8; row];
    let mut best = vec![0u8; row];
    for line in image.pixels.chunks_exact(image.width as usize * 4) {
        current.clear();
        for p in line.as_chunks::<4>().0 {
            current.extend_from_slice(&p[..channels]);
        }
        // The minimum-sum-of-absolute-differences heuristic from libpng.
        let mut best_filter = 0u8;
        let mut best_score = u64::MAX;
        for filter in 0..5u8 {
            for i in 0..row {
                let left = if i >= channels {
                    current[i - channels]
                } else {
                    0
                };
                let up = previous[i];
                let corner = if i >= channels {
                    previous[i - channels]
                } else {
                    0
                };
                let predicted = match filter {
                    0 => 0,
                    1 => left,
                    2 => up,
                    3 => ((left as u16 + up as u16) / 2) as u8,
                    _ => paeth(left, up, corner),
                };
                candidate[i] = current[i].wrapping_sub(predicted);
            }
            let score = candidate
                .iter()
                .map(|&v| (v as i8).unsigned_abs() as u64)
                .sum();
            if score < best_score {
                best_score = score;
                best_filter = filter;
                std::mem::swap(&mut best, &mut candidate);
            }
        }
        raw.push(best_filter);
        raw.extend_from_slice(&best);
        std::mem::swap(&mut previous, &mut current);
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 4);
    let mut out = Vec::with_capacity(compressed.len() + 64);
    out.extend_from_slice(PNG_SIGNATURE);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&image.width.to_be_bytes());
    header.extend_from_slice(&image.height.to_be_bytes());
    header.extend_from_slice(&[8, if opaque { 2 } else { 6 }, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &compressed);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Signature and IHDR dimensions only: enough to bound a PNG passed through.
pub(crate) fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() > MAX_IMAGE_BYTES || !data.starts_with(PNG_SIGNATURE) {
        return None;
    }
    if u32_be(data, 8)? != 13 || data.get(12..16)? != b"IHDR" {
        return None;
    }
    let (width, height) = (u32_be(data, 16)?, u32_be(data, 20)?);
    pixel_count(width, height)?;
    Some((width, height))
}

struct PngHeader {
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
    interlaced: bool,
}
impl PngHeader {
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        }
    }
    fn bits_per_pixel(&self) -> usize {
        self.channels() * self.depth as usize
    }
    fn row_bytes(&self, width: usize) -> usize {
        (width * self.bits_per_pixel()).div_ceil(8)
    }
}

/// Adam7 origin and step for each pass: (x0, y0, dx, dy).
const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn pass_size(header: &PngHeader, pass: (usize, usize, usize, usize)) -> (usize, usize) {
    let (x0, y0, dx, dy) = pass;
    let (w, h) = (header.width as usize, header.height as usize);
    ((w + dx - 1 - x0.min(w)) / dx, (h + dy - 1 - y0.min(h)) / dy)
}

fn unfilter(data: &mut [u8], width_bytes: usize, rows: usize, bpp: usize) -> Option<()> {
    let stride = width_bytes + 1;
    for y in 0..rows {
        let (before, rest) = data.split_at_mut(y * stride);
        let previous = (y > 0).then(|| &before[before.len() - width_bytes..]);
        let (filter, line) = rest[..stride].split_first_mut()?;
        for i in 0..width_bytes {
            let left = if i >= bpp { line[i - bpp] } else { 0 };
            let up = previous.map_or(0, |p| p[i]);
            let corner = if i >= bpp {
                previous.map_or(0, |p| p[i - bpp])
            } else {
                0
            };
            line[i] = line[i].wrapping_add(match *filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((left as u16 + up as u16) / 2) as u8,
                4 => paeth(left, up, corner),
                _ => return None,
            });
        }
    }
    Some(())
}

fn sample(line: &[u8], index: usize, depth: u8) -> u16 {
    match depth {
        8 => line[index] as u16,
        16 => u16::from_be_bytes([line[index * 2], line[index * 2 + 1]]),
        _ => {
            let bit = index * depth as usize;
            let shift = 8 - depth as usize - bit % 8;
            ((line[bit / 8] >> shift) & ((1u16 << depth) - 1) as u8) as u16
        }
    }
}

/// Decodes every standard PNG colour type and depth, with or without
/// Adam7 interlacing, into RGBA8. Ancillary chunks other than tRNS are
/// ignored; unknown critical chunks and bad CRCs are refused.
pub(crate) fn decode_png(data: &[u8]) -> Option<Rgba> {
    png_dimensions(data)?;
    let mut position = 8;
    let mut header = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut transparency: Option<&[u8]> = None;
    let mut compressed = Vec::new();
    loop {
        let length = u32_be(data, position)? as usize;
        let kind: [u8; 4] = data.get(position + 4..position + 8)?.try_into().ok()?;
        let body = data.get(position + 8..(position + 8).checked_add(length)?)?;
        let crc = u32_be(data, position + 8 + length)?;
        if crc32(&[&kind, body]) != crc {
            return None;
        }
        position += 12 + length;
        match &kind {
            b"IHDR" if header.is_none() && length == 13 => {
                let (depth, color) = (body[8], body[9]);
                let valid = match color {
                    0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                    3 => matches!(depth, 1 | 2 | 4 | 8),
                    2 | 4 | 6 => matches!(depth, 8 | 16),
                    _ => false,
                };
                if !valid || body[10] != 0 || body[11] != 0 || body[12] > 1 {
                    return None;
                }
                header = Some(PngHeader {
                    width: u32_be(body, 0)?,
                    height: u32_be(body, 4)?,
                    depth,
                    color,
                    interlaced: body[12] == 1,
                });
            }
            _ if header.is_none() => return None,
            b"PLTE" => {
                if !length.is_multiple_of(3) || length > 768 || !palette.is_empty() {
                    return None;
                }
                palette = body.as_chunks::<3>().0.to_vec();
            }
            b"tRNS" => transparency = Some(body),
            b"IDAT" => compressed.extend_from_slice(body),
            b"IEND" => break,
            _ if kind[0] & 0x20 == 0 => return None,
            _ => {}
        }
    }
    let header = header?;
    let count = pixel_count(header.width, header.height)?;
    if header.color == 3 && palette.is_empty() {
        return None;
    }
    let passes: &[_] = if header.interlaced {
        &ADAM7
    } else {
        &[(0, 0, 1, 1)]
    };
    let mut expected = 0usize;
    for &pass in passes {
        let (w, h) = pass_size(&header, pass);
        if w > 0 && h > 0 {
            expected = expected.checked_add((header.row_bytes(w) + 1).checked_mul(h)?)?;
        }
    }
    let mut raw =
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&compressed, expected).ok()?;
    if raw.len() != expected {
        return None;
    }
    let bpp = header.bits_per_pixel().div_ceil(8);
    let channels = header.channels();
    let max = ((1u32 << header.depth) - 1) as u16;
    let scale = |value: u16| -> u8 {
        match header.depth {
            16 => (value >> 8) as u8,
            8 => value as u8,
            _ => (value as u32 * 255 / max as u32) as u8,
        }
    };
    let key = |index: usize| -> Option<u16> {
        let t = transparency?;
        Some(u16::from_be_bytes([
            *t.get(index * 2)?,
            *t.get(index * 2 + 1)?,
        ]))
    };
    let (gray_key, rgb_key) = match header.color {
        0 => (key(0), None),
        2 => (
            None,
            key(0).zip(key(1)).zip(key(2)).map(|((r, g), b)| [r, g, b]),
        ),
        _ => (None, None),
    };
    let mut pixels = vec![0u8; count * 4];
    let mut offset = 0;
    for &pass in passes {
        let (w, h) = pass_size(&header, pass);
        if w == 0 || h == 0 {
            continue;
        }
        let width_bytes = header.row_bytes(w);
        let size = (width_bytes + 1) * h;
        let block = &mut raw[offset..offset + size];
        offset += size;
        unfilter(block, width_bytes, h, bpp)?;
        let (x0, y0, dx, dy) = pass;
        for (row, line) in block.chunks_exact(width_bytes + 1).enumerate() {
            let line = &line[1..];
            let y = y0 + row * dy;
            for column in 0..w {
                let x = x0 + column * dx;
                let s = |c: usize| sample(line, column * channels + c, header.depth);
                let rgba = match header.color {
                    0 => {
                        let v = s(0);
                        let g = scale(v);
                        [g, g, g, if gray_key == Some(v) { 0 } else { 255 }]
                    }
                    2 => {
                        let v = [s(0), s(1), s(2)];
                        let a = if rgb_key == Some(v) { 0 } else { 255 };
                        [scale(v[0]), scale(v[1]), scale(v[2]), a]
                    }
                    3 => {
                        let index = s(0) as usize;
                        let [r, g, b] = *palette.get(index)?;
                        let a = transparency
                            .and_then(|t| t.get(index))
                            .copied()
                            .unwrap_or(255);
                        [r, g, b, a]
                    }
                    4 => {
                        let g = scale(s(0));
                        [g, g, g, scale(s(1))]
                    }
                    _ => [scale(s(0)), scale(s(1)), scale(s(2)), scale(s(3))],
                };
                let at = (y * header.width as usize + x) * 4;
                pixels[at..at + 4].copy_from_slice(&rgba);
            }
        }
    }
    Some(Rgba {
        width: header.width,
        height: header.height,
        pixels,
    })
}

pub(crate) fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    decode_dib(dib).map(|image| encode_png(&image))
}

pub(crate) fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    decode_png(png).map(|image| encode_dib(&image))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32, alpha: bool) -> Rgba {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let a = if alpha {
                    ((x + y) * 37 % 255 + 1) as u8
                } else {
                    255
                };
                pixels.extend_from_slice(&[(x * 9) as u8, (y * 13) as u8, (x ^ y) as u8, a]);
            }
        }
        Rgba {
            width,
            height,
            pixels,
        }
    }

    fn dib_header(width: i32, height: i32, bits: u16, compression: u32, size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&compression.to_le_bytes());
        out.resize(size as usize, 0);
        out
    }

    #[test]
    fn png_roundtrips_odd_widths_with_and_without_alpha() {
        for (width, height) in [(1, 1), (3, 2), (17, 5), (33, 31)] {
            for alpha in [false, true] {
                let image = gradient(width, height, alpha);
                let png = encode_png(&image);
                assert_eq!(png_dimensions(&png), Some((width, height)));
                assert_eq!(decode_png(&png).unwrap(), image);
            }
        }
    }

    #[test]
    fn dib_roundtrips_through_png_both_ways() {
        for (width, height) in [(1, 1), (3, 3), (5, 2)] {
            let image = gradient(width, height, true);
            let dib = encode_dib(&image);
            assert_eq!(dib.len(), 40 + (width * height * 4) as usize);
            assert_eq!(decode_dib(&dib).unwrap(), image);
            let png = dib_to_png(&dib).unwrap();
            assert_eq!(decode_png(&png).unwrap(), image);
            assert_eq!(png_to_dib(&png).unwrap(), dib);
        }
    }

    #[test]
    fn bottom_up_and_top_down_24_bit_rows_with_odd_width_padding() {
        // 3x2 pixels: 9 bytes of BGR plus 3 padding bytes per row.
        let rows = [
            [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 0, 0, 0],
            [10, 11, 12, 13, 14, 15, 16, 17, 18, 0, 0, 0],
        ];
        let expect_top = [[3, 2, 1, 255], [6, 5, 4, 255], [9, 8, 7, 255]];
        let expect_bottom = [[12, 11, 10, 255], [15, 14, 13, 255], [18, 17, 16, 255]];
        let mut bottom_up = dib_header(3, 2, 24, BI_RGB, 40);
        bottom_up.extend_from_slice(&rows[1]);
        bottom_up.extend_from_slice(&rows[0]);
        let mut top_down = dib_header(3, -2, 24, BI_RGB, 40);
        top_down.extend_from_slice(&rows[0]);
        top_down.extend_from_slice(&rows[1]);
        for dib in [bottom_up, top_down] {
            let image = decode_dib(&dib).unwrap();
            assert_eq!((image.width, image.height), (3, 2));
            assert_eq!(image.pixels[..12], *expect_top.as_flattened());
            assert_eq!(image.pixels[12..], *expect_bottom.as_flattened());
        }
    }

    #[test]
    fn bitfields_masks_after_info_header_and_inside_v5_header() {
        let pixel = 0x11_22_33_44u32; // A=0x11? No: masks decide.
        // 40-byte header, BI_BITFIELDS with R=0xff, G=0xff00, B=0xff0000.
        let mut info = dib_header(1, 1, 32, BI_BITFIELDS, 40);
        for mask in [0xffu32, 0xff00, 0xff0000] {
            info.extend_from_slice(&mask.to_le_bytes());
        }
        info.extend_from_slice(&pixel.to_le_bytes());
        assert_eq!(decode_dib(&info).unwrap().pixels, [0x44, 0x33, 0x22, 255]);
        // V5 header with an alpha mask: channels come from the header.
        let mut v5 = dib_header(1, 1, 32, BI_BITFIELDS, 124);
        for (index, mask) in [0x00ff0000u32, 0xff00, 0xff, 0xff000000].iter().enumerate() {
            v5[40 + index * 4..44 + index * 4].copy_from_slice(&mask.to_le_bytes());
        }
        let mut with_alpha = v5.clone();
        with_alpha.extend_from_slice(&pixel.to_le_bytes());
        assert_eq!(
            decode_dib(&with_alpha).unwrap().pixels,
            [0x22, 0x33, 0x44, 0x11]
        );
        // The same V5 with redundant masks appended before the pixels.
        let mut redundant = v5;
        redundant.extend_from_slice(&[0xee; 12]);
        redundant.extend_from_slice(&pixel.to_le_bytes());
        assert_eq!(
            decode_dib(&redundant).unwrap().pixels,
            [0x22, 0x33, 0x44, 0x11]
        );
        // RGB565.
        let mut rgb565 = dib_header(1, 1, 16, BI_BITFIELDS, 40);
        for mask in [0xf800u32, 0x07e0, 0x001f] {
            rgb565.extend_from_slice(&mask.to_le_bytes());
        }
        rgb565.extend_from_slice(&[0xff, 0xff, 0, 0]);
        assert_eq!(decode_dib(&rgb565).unwrap().pixels, [255, 255, 255, 255]);
    }

    #[test]
    fn zero_reserved_byte_is_opaque_and_palettes_decode() {
        let mut dib = dib_header(2, 1, 32, BI_RGB, 40);
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        assert_eq!(
            decode_dib(&dib).unwrap().pixels,
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
        let mut paletted = dib_header(3, 1, 1, BI_RGB, 40);
        paletted.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        paletted.extend_from_slice(&[0b1010_0000, 0, 0, 0]);
        assert_eq!(
            decode_dib(&paletted).unwrap().pixels,
            [255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn malformed_and_oversize_dibs_are_refused() {
        let good = encode_dib(&gradient(4, 4, false));
        assert!(decode_dib(&good).is_some());
        let mut cases = vec![
            Vec::new(),
            good[..39].to_vec(),
            good[..good.len() - 1].to_vec(),
            dib_header(0, 1, 32, BI_RGB, 40),
            dib_header(1, 0, 32, BI_RGB, 40),
            dib_header(-1, 1, 32, BI_RGB, 40),
            dib_header(1, i32::MIN, 32, BI_RGB, 40),
            dib_header(1, 1, 32, 1 /* BI_RLE8 */, 40),
            dib_header(1, 1, 7, BI_RGB, 40),
            dib_header(1, 1, 32, BI_RGB, 12),
            dib_header(40000, 1, 32, BI_RGB, 40),
            // 4096 x 4096 x 4 = 64 MiB of pixels, above the decode bound.
            dib_header(4096, 4096, 32, BI_RGB, 40),
            vec![0; MAX_DIB_BYTES + 1],
        ];
        // A palette index outside a two-entry table.
        let mut index = dib_header(1, 1, 8, BI_RGB, 40);
        index[32..36].copy_from_slice(&2u32.to_le_bytes());
        index.extend_from_slice(&[0; 8]);
        index.extend_from_slice(&[5, 0, 0, 0]);
        cases.push(index);
        for case in cases {
            assert!(decode_dib(&case).is_none());
        }
    }

    #[test]
    fn malformed_and_oversize_pngs_are_refused() {
        let good = encode_png(&gradient(6, 3, true));
        assert!(decode_png(&good).is_some());
        let mut bad_crc = good.clone();
        let last = bad_crc.len() - 20;
        bad_crc[last] ^= 1;
        let mut huge = good.clone();
        huge[16..20].copy_from_slice(&5000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&5000u32.to_be_bytes());
        let mut critical = PNG_SIGNATURE.to_vec();
        chunk(&mut critical, b"IHDR", &good[16..29]);
        chunk(&mut critical, b"ZZZZ", &[]);
        for case in [
            Vec::new(),
            good[..good.len() - 12].to_vec(),
            bad_crc,
            huge,
            critical,
            b"GIF89a".to_vec(),
        ] {
            assert!(decode_png(&case).is_none());
        }
        assert!(png_dimensions(&vec![0; MAX_IMAGE_BYTES + 1]).is_none());
    }

    /// Builds a PNG from already-filtered scanlines for decoder coverage of
    /// colour types the encoder never writes.
    fn raw_png(
        width: u32,
        height: u32,
        depth: u8,
        color: u8,
        interlace: u8,
        extra: &[(&[u8; 4], &[u8])],
        raw: &[u8],
    ) -> Vec<u8> {
        let mut out = PNG_SIGNATURE.to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[depth, color, 0, 0, interlace]);
        chunk(&mut out, b"IHDR", &header);
        for (kind, body) in extra {
            chunk(&mut out, kind, body);
        }
        chunk(
            &mut out,
            b"IDAT",
            &miniz_oxide::deflate::compress_to_vec_zlib(raw, 6),
        );
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn decodes_palette_gray_sixteen_bit_and_interlaced_pngs() {
        // 2-bit palette with tRNS, one row of 3 pixels: indices 0, 1, 2.
        let palette = raw_png(
            3,
            1,
            2,
            3,
            0,
            &[(b"PLTE", &[1, 2, 3, 4, 5, 6, 7, 8, 9]), (b"tRNS", &[0])],
            &[0, 0b0001_1000],
        );
        assert_eq!(
            decode_png(&palette).unwrap().pixels,
            [1, 2, 3, 0, 4, 5, 6, 255, 7, 8, 9, 255]
        );
        // 16-bit gray+alpha.
        let gray = raw_png(1, 1, 16, 4, 0, &[], &[0, 0x80, 0x01, 0xff, 0xff]);
        assert_eq!(decode_png(&gray).unwrap().pixels, [0x80, 0x80, 0x80, 0xff]);
        // 1-bit gray with a transparent key of 0.
        let key = raw_png(2, 1, 1, 0, 0, &[(b"tRNS", &[0, 0])], &[0, 0b0100_0000]);
        assert_eq!(
            decode_png(&key).unwrap().pixels,
            [0, 0, 0, 0, 255, 255, 255, 255]
        );
        // Adam7 3x3 8-bit gray: passes 1, 4 (x=2), 5 (y=2), 6, 7.
        let image: Vec<u8> = (0..9).map(|v| v * 20).collect();
        let at = |x: usize, y: usize| image[y * 3 + x];
        let mut raw = vec![0, at(0, 0)];
        raw.extend_from_slice(&[0, at(2, 0)]);
        raw.extend_from_slice(&[0, at(0, 2), at(2, 2)]);
        raw.extend_from_slice(&[0, at(1, 0), 0, at(1, 2)]);
        raw.extend_from_slice(&[0, at(0, 1), at(1, 1), at(2, 1)]);
        let interlaced = raw_png(3, 3, 8, 0, 1, &[], &raw);
        let decoded = decode_png(&interlaced).unwrap();
        let gray: Vec<u8> = decoded
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0])
            .collect();
        assert_eq!(gray, image);
        // Missing palette, invalid depth and trailing image data are refused.
        assert!(decode_png(&raw_png(1, 1, 8, 3, 0, &[], &[0, 0])).is_none());
        assert!(decode_png(&raw_png(1, 1, 3, 0, 0, &[], &[0, 0])).is_none());
        assert!(decode_png(&raw_png(1, 1, 8, 0, 0, &[], &[0, 0, 0])).is_none());
        assert!(decode_png(&raw_png(1, 1, 8, 0, 0, &[], &[9, 0])).is_none());
    }
}
