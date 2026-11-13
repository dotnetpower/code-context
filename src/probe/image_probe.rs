use super::{ImageProbeArgs, clamp_json_budget, clamp_utf8_bytes};
use crate::document::inflate_zlib_bounded;
use crate::inline_budget::INLINE_BUDGET;
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

/// Cap raw uncompressed PNG scanline buffer to 64 MiB so giant textures stay header-only safe.
const MAX_PNG_RAW_SCANLINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_FILES: usize = 128;

#[derive(Clone, Debug, Serialize)]
pub struct ImageInfo {
    pub path: String,
    pub format: &'static str,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    pub color_space: String,
    pub has_alpha: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_box: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub svg_elements: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ExposureStats>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ExposureStats {
    pub mean_lum: f64,
    pub p05_lum: u8,
    pub p50_lum: u8,
    pub p95_lum: u8,
    pub dark_pct: f64,
    pub bright_pct: f64,
    pub transparent_pct: f64,
}

fn parse_expected_dimensions(raw: &str) -> Result<(u32, u32)> {
    let lower = raw.trim().to_ascii_lowercase();
    let Some((w_s, h_s)) = lower.split_once('x') else {
        return Err(invalid(format!(
            "--expect must have form WIDTHxHEIGHT (e.g. 256x256), got `{raw}`"
        ))
        .into());
    };
    let w = w_s
        .trim()
        .parse::<u32>()
        .map_err(|_| invalid(format!("invalid width in --expect `{raw}`")))?;
    let h = h_s
        .trim()
        .parse::<u32>()
        .map_err(|_| invalid(format!("invalid height in --expect `{raw}`")))?;
    if w == 0 || h == 0 {
        return Err(invalid("--expect dimensions must be > 0").into());
    }
    Ok((w, h))
}

pub fn looks_like_image_magic(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xFF\xD8\xFF")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
    {
        return true;
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return true;
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
}

pub fn inspect_image_bytes(
    label: &str,
    data: &[u8],
    expected: Option<(u32, u32)>,
    header_only: bool,
) -> Result<ImageInfo> {
    let mut info = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        inspect_png(label, data, header_only)?
    } else if data.starts_with(b"\xFF\xD8\xFF") {
        inspect_jpeg(label, data)?
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        inspect_webp(label, data)?
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        inspect_gif(label, data)?
    } else {
        let text = std::str::from_utf8(data)
            .map_err(|_| invalid(format!("unrecognized binary image format in {label}")))?;
        inspect_svg(label, text, data.len())?
    };

    if let Some((ew, eh)) = expected
        && (info.width != ew || info.height != eh)
    {
        info.flags.push(format!(
            "DIMENSION_MISMATCH(expected={ew}x{eh},actual={}x{})",
            info.width, info.height
        ));
    }
    Ok(info)
}

fn inspect_png(label: &str, data: &[u8], header_only: bool) -> Result<ImageInfo> {
    if data.len() < 33 || &data[12..16] != b"IHDR" {
        return Err(invalid(format!("truncated or invalid PNG IHDR in {label}")).into());
    }
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    let bit_depth = data[24];
    let color_type = data[25];
    let interlace = data[28];

    if width == 0 || height == 0 {
        return Err(invalid(format!("zero-dimension PNG in {label}")).into());
    }

    let (color_name, channels, has_alpha_type) = match color_type {
        0 => ("Grayscale", 1usize, false),
        2 => ("RGB", 3usize, false),
        3 => ("Palette", 1usize, false),
        4 => ("GrayAlpha", 2usize, true),
        6 => ("RGBA", 4usize, true),
        other => {
            return Err(invalid(format!("unsupported PNG color_type {other} in {label}")).into());
        }
    };

    let mut pos = 8usize;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat_compressed = Vec::new();
    let mut saw_iend = false;

    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let chunk_type = &data[pos + 4..pos + 8];
        let payload_start = pos + 8;
        let Some(payload_end) = payload_start.checked_add(len) else {
            break;
        };
        if payload_end + 4 > data.len() {
            break;
        }
        let payload = &data[payload_start..payload_end];
        match chunk_type {
            b"PLTE" => {
                for &rgb in payload.as_chunks::<3>().0 {
                    palette.push(rgb);
                }
            }
            b"tRNS" => {
                trns.extend_from_slice(payload);
            }
            b"IDAT" if !header_only => {
                idat_compressed.extend_from_slice(payload);
            }
            b"IEND" => {
                saw_iend = true;
                break;
            }
            _ => {}
        }
        pos = payload_end + 4;
    }

    let mut flags = Vec::new();
    if !saw_iend {
        flags.push("TRUNCATED_PNG_NO_IEND".to_string());
    }
    let has_alpha = has_alpha_type || !trns.is_empty();

    let exposure = if !header_only
        && interlace == 0
        && matches!(bit_depth, 1 | 2 | 4 | 8 | 16)
        && !idat_compressed.is_empty()
    {
        compute_png_exposure(
            width as usize,
            height as usize,
            bit_depth,
            color_type,
            channels,
            &palette,
            &trns,
            &idat_compressed,
            &mut flags,
        )
    } else {
        None
    };

    Ok(ImageInfo {
        path: label.to_string(),
        format: "PNG",
        bytes: data.len(),
        width,
        height,
        color_space: format!("{color_name}/{bit_depth}b"),
        has_alpha,
        view_box: None,
        svg_elements: None,
        exposure,
        flags,
    })
}

#[allow(clippy::too_many_arguments)]
fn compute_png_exposure(
    width: usize,
    height: usize,
    bit_depth: u8,
    color_type: u8,
    channels: usize,
    palette: &[[u8; 3]],
    trns: &[u8],
    idat: &[u8],
    flags: &mut Vec<String>,
) -> Option<ExposureStats> {
    let bits_per_pixel = channels.checked_mul(bit_depth as usize)?;
    let bpp = bits_per_pixel.div_ceil(8).max(1);
    let stride = width.checked_mul(bits_per_pixel)?.div_ceil(8);
    let expected_raw = height.checked_mul(stride.checked_add(1)?)?;
    if expected_raw == 0 || expected_raw > MAX_PNG_RAW_SCANLINE_BYTES {
        return None;
    }

    let raw = inflate_zlib_bounded(idat, expected_raw).ok()?;
    if raw.len() < expected_raw {
        return None;
    }

    let mut prev_row = vec![0u8; stride];
    let mut cur_row = vec![0u8; stride];
    let mut hist = [0u64; 256];
    let mut lum_sum: f64 = 0.0;
    let mut transparent_pixels: u64 = 0;
    let total_pixels = (width as u64) * (height as u64);

    for y in 0..height {
        let row_offset = y * (stride + 1);
        let filter = raw[row_offset];
        let src = &raw[row_offset + 1..row_offset + 1 + stride];
        match filter {
            0 => cur_row.copy_from_slice(src),
            1 => {
                cur_row[..bpp].copy_from_slice(&src[..bpp]);
                for i in bpp..stride {
                    cur_row[i] = src[i].wrapping_add(cur_row[i - bpp]);
                }
            }
            2 => {
                for ((dst, &s), &up) in cur_row.iter_mut().zip(src.iter()).zip(prev_row.iter()) {
                    *dst = s.wrapping_add(up);
                }
            }
            3 => {
                for i in 0..stride {
                    let left = if i >= bpp { cur_row[i - bpp] } else { 0 };
                    let up = prev_row[i];
                    cur_row[i] = src[i].wrapping_add((((left as u16) + (up as u16)) >> 1) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let left = if i >= bpp { cur_row[i - bpp] } else { 0 };
                    let up = prev_row[i];
                    let up_left = if i >= bpp { prev_row[i - bpp] } else { 0 };
                    cur_row[i] = src[i].wrapping_add(paeth_predictor(left, up, up_left));
                }
            }
            _ => return None,
        }

        if (color_type, bit_depth) == (6, 8) {
            for px in cur_row.as_chunks::<4>().0 {
                if px[3] == 0 {
                    transparent_pixels += 1;
                    continue;
                }
                let lum =
                    (((54u32 * px[0] as u32) + (183u32 * px[1] as u32) + (19u32 * px[2] as u32)) >> 8)
                        as u8;
                hist[lum as usize] += 1;
                lum_sum += lum as f64;
            }
        } else if (color_type, bit_depth) == (2, 8) {
            for px in cur_row.as_chunks::<3>().0 {
                let lum =
                    (((54u32 * px[0] as u32) + (183u32 * px[1] as u32) + (19u32 * px[2] as u32)) >> 8)
                        as u8;
                hist[lum as usize] += 1;
                lum_sum += lum as f64;
            }
        } else {
            for x in 0..width {
                let (r, g, b, a) = if bit_depth < 8 {
                    let samples_per_byte = (8 / bit_depth) as usize;
                    let byte_val = cur_row[x / samples_per_byte];
                    let shift = (samples_per_byte - 1 - (x % samples_per_byte)) * (bit_depth as usize);
                    let mask = (1u8 << bit_depth) - 1;
                    let sample = (byte_val >> shift) & mask;
                    if color_type == 3 {
                        let idx = sample as usize;
                        let [pr, pg, pb] = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                        let pa = trns.get(idx).copied().unwrap_or(255);
                        (pr, pg, pb, pa)
                    } else if color_type == 0 {
                        let scaled = ((sample as u16 * 255) / (mask as u16)) as u8;
                        (scaled, scaled, scaled, 255u8)
                    } else {
                        return None;
                    }
                } else {
                    let px = &cur_row[x * bpp..(x + 1) * bpp];
                    match (color_type, bit_depth) {
                        (0, 8) => (px[0], px[0], px[0], 255u8),
                        (0, 16) => (px[0], px[0], px[0], 255u8),
                        (2, 16) => (px[0], px[2], px[4], 255u8),
                        (3, 8) => {
                            let idx = px[0] as usize;
                            let [pr, pg, pb] = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                            let pa = trns.get(idx).copied().unwrap_or(255);
                            (pr, pg, pb, pa)
                        }
                        (4, 8) => (px[0], px[0], px[0], px[1]),
                        (4, 16) => (px[0], px[0], px[0], px[2]),
                        (6, 16) => (px[0], px[2], px[4], px[6]),
                        _ => return None,
                    }
                };
                if a == 0 {
                    transparent_pixels += 1;
                    continue;
                }
                let lum = (((54u32 * r as u32) + (183u32 * g as u32) + (19u32 * b as u32)) >> 8) as u8;
                hist[lum as usize] += 1;
                lum_sum += lum as f64;
            }
        }
        std::mem::swap(&mut prev_row, &mut cur_row);
    }

    let visible_pixels = total_pixels.saturating_sub(transparent_pixels);
    let transparent_pct =
        ((transparent_pixels as f64) * 1000.0 / (total_pixels as f64)).round() / 10.0;
    if visible_pixels == 0 {
        flags.push("ALL_TRANSPARENT".to_string());
        return Some(ExposureStats {
            mean_lum: 0.0,
            p05_lum: 0,
            p50_lum: 0,
            p95_lum: 0,
            dark_pct: 0.0,
            bright_pct: 0.0,
            transparent_pct,
        });
    }

    let mean_lum = (lum_sum / (visible_pixels as f64) * 10.0).round() / 10.0;
    let p05_lum = histogram_percentile(&hist, visible_pixels, 0.05);
    let p50_lum = histogram_percentile(&hist, visible_pixels, 0.50);
    let p95_lum = histogram_percentile(&hist, visible_pixels, 0.95);
    let dark_count: u64 = hist[0..5].iter().sum();
    let bright_count: u64 = hist[251..256].iter().sum();
    let dark_pct = ((dark_count as f64) * 1000.0 / (visible_pixels as f64)).round() / 10.0;
    let bright_pct = ((bright_count as f64) * 1000.0 / (visible_pixels as f64)).round() / 10.0;

    let distinct_bins = hist.iter().filter(|&&c| c > 0).count();
    if hist[0] == visible_pixels {
        flags.push("ALL_BLACK".to_string());
    } else if distinct_bins == 1 {
        flags.push("SOLID_COLOR".to_string());
    } else if mean_lum < 12.0 && dark_pct > 90.0 {
        flags.push("UNDEREXPOSED".to_string());
    } else if mean_lum > 245.0 && bright_pct > 90.0 {
        flags.push("OVEREXPOSED".to_string());
    }

    Some(ExposureStats {
        mean_lum,
        p05_lum,
        p50_lum,
        p95_lum,
        dark_pct,
        bright_pct,
        transparent_pct,
    })
}

fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let p = (a as i32) + (b as i32) - (c as i32);
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn histogram_percentile(hist: &[u64; 256], total: u64, q: f64) -> u8 {
    let threshold = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut cumulative = 0u64;
    for (bin, &count) in hist.iter().enumerate() {
        cumulative += count;
        if cumulative >= threshold {
            return bin as u8;
        }
    }
    255
}

fn inspect_jpeg(label: &str, data: &[u8]) -> Result<ImageInfo> {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let marker = data[i];
        i += 1;
        if matches!(marker, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 2 > data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > data.len() {
            break;
        }
        // SOF0..SOF15 (excluding DHT=0xC4, JPG=0xC8, DAC=0xCC)
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if seg_len < 8 {
                break;
            }
            let precision = data[i + 2];
            let height = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
            let width = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let comps = data[i + 7];
            let cs = match comps {
                1 => format!("Gray/{precision}b"),
                3 => format!("YCbCr/{precision}b"),
                4 => format!("CMYK/{precision}b"),
                other => format!("{other}ch/{precision}b"),
            };
            return Ok(ImageInfo {
                path: label.to_string(),
                format: "JPEG",
                bytes: data.len(),
                width,
                height,
                color_space: cs,
                has_alpha: false,
                view_box: None,
                svg_elements: None,
                exposure: None,
                flags: Vec::new(),
            });
        }
        i += seg_len;
    }
    Err(invalid(format!("could not locate JPEG SOF frame header in {label}")).into())
}

fn inspect_webp(label: &str, data: &[u8]) -> Result<ImageInfo> {
    if data.len() < 30 {
        return Err(invalid(format!("truncated WebP header in {label}")).into());
    }
    let chunk = &data[12..16];
    let (width, height, has_alpha, subfmt) = match chunk {
        b"VP8X" => {
            let flags = data[20];
            let alpha = (flags & 0x10) != 0;
            let w = 1 + u32::from_le_bytes([data[24], data[25], data[26], 0]);
            let h = 1 + u32::from_le_bytes([data[27], data[28], data[29], 0]);
            (w, h, alpha, if alpha { "VP8X+Alpha" } else { "VP8X" })
        }
        b"VP8L" => {
            if data[20] != 0x2F {
                return Err(invalid(format!("invalid VP8L signature in {label}")).into());
            }
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            let alpha = ((bits >> 28) & 1) != 0;
            (w, h, alpha, "VP8L-Lossless")
        }
        b"VP8 " => {
            // Lossy keyframe signature `9D 01 2A` at offset 23
            if data[23..26] != [0x9D, 0x01, 0x2A] {
                return Err(invalid(format!("invalid VP8 keyframe code in {label}")).into());
            }
            let w = (u16::from_le_bytes([data[26], data[27]]) & 0x3FFF) as u32;
            let h = (u16::from_le_bytes([data[28], data[29]]) & 0x3FFF) as u32;
            (w, h, false, "VP8-Lossy")
        }
        _ => return Err(invalid(format!("unsupported WebP chunk in {label}")).into()),
    };
    Ok(ImageInfo {
        path: label.to_string(),
        format: "WebP",
        bytes: data.len(),
        width,
        height,
        color_space: subfmt.to_string(),
        has_alpha,
        view_box: None,
        svg_elements: None,
        exposure: None,
        flags: Vec::new(),
    })
}

fn inspect_gif(label: &str, data: &[u8]) -> Result<ImageInfo> {
    if data.len() < 13 {
        return Err(invalid(format!("truncated GIF header in {label}")).into());
    }
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;
    Ok(ImageInfo {
        path: label.to_string(),
        format: "GIF",
        bytes: data.len(),
        width,
        height,
        color_space: "Indexed/8b".to_string(),
        has_alpha: true,
        view_box: None,
        svg_elements: None,
        exposure: None,
        flags: Vec::new(),
    })
}

fn inspect_svg(label: &str, text: &str, byte_len: usize) -> Result<ImageInfo> {
    let Some(svg_idx) = text.find("<svg") else {
        return Err(invalid(format!("missing `<svg` root element in {label}")).into());
    };
    let after_svg = &text[svg_idx..];
    let Some(tag_end) = after_svg.find('>') else {
        return Err(invalid(format!("unclosed `<svg` opening tag in {label}")).into());
    };
    let root_open = &after_svg[..=tag_end];

    let view_box = extract_xml_attr(root_open, "viewBox")
        .or_else(|| extract_xml_attr(root_open, "viewbox"));
    let raw_w = extract_xml_attr(root_open, "width");
    let raw_h = extract_xml_attr(root_open, "height");

    let mut vb_dims: Option<(u32, u32)> = None;
    let mut flags = Vec::new();
    if let Some(ref vb) = view_box {
        let nums: Vec<f64> = vb
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        if nums.len() == 4 {
            if nums[2] <= 0.0 || nums[3] <= 0.0 {
                flags.push("ZERO_VIEWBOX".to_string());
            } else {
                vb_dims = Some((nums[2].round() as u32, nums[3].round() as u32));
            }
        } else {
            flags.push("INVALID_VIEWBOX".to_string());
        }
    } else {
        flags.push("MISSING_VIEWBOX".to_string());
    }

    let width = raw_w
        .as_deref()
        .and_then(parse_svg_length)
        .or_else(|| vb_dims.map(|d| d.0))
        .unwrap_or(0);
    let height = raw_h
        .as_deref()
        .and_then(parse_svg_length)
        .or_else(|| vb_dims.map(|d| d.1))
        .unwrap_or(0);

    let mut elements = 0usize;
    for tag in [
        "<path", "<rect", "<circle", "<ellipse", "<line", "<polyline", "<polygon", "<text",
        "<image", "<use",
    ] {
        elements += after_svg.matches(tag).count();
    }
    if elements == 0 {
        flags.push("EMPTY_SVG".to_string());
    }
    if !after_svg.contains("</svg>") && !root_open.ends_with("/>") {
        flags.push("UNCLOSED_SVG".to_string());
    }

    Ok(ImageInfo {
        path: label.to_string(),
        format: "SVG",
        bytes: byte_len,
        width,
        height,
        color_space: "Vector/SVG".to_string(),
        has_alpha: true,
        view_box,
        svg_elements: Some(elements),
        exposure: None,
        flags,
    })
}

fn extract_xml_attr(tag: &str, attr_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let needle = attr_name.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let prev_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
            let mut j = i + needle.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if prev_ok && j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
                    let quote = bytes[j] as char;
                    let start = j + 1;
                    if let Some(off) = tag[start..].find(quote) {
                        return Some(tag[start..start + off].trim().to_string());
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn parse_svg_length(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.ends_with('%') {
        return None;
    }
    let num_part: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num_part
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0 && v.is_finite())
        .map(|v| v.round() as u32)
}

fn collect_image_targets(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in inputs {
        let meta = std::fs::metadata(p)
            .map_err(|e| invalid(format!("cannot access {}: {e}", p.display())))?;
        if meta.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|d| d.path()))
                .filter(|path| {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| {
                            matches!(
                                ext.to_ascii_lowercase().as_str(),
                                "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg"
                            )
                        })
                })
                .collect();
            entries.sort();
            out.extend(entries);
        } else {
            out.push(p.clone());
        }
        if out.len() >= MAX_IMAGE_FILES {
            out.truncate(MAX_IMAGE_FILES);
            break;
        }
    }
    if out.is_empty() {
        return Err(invalid("no supported image files found").into());
    }
    Ok(out)
}

pub fn run(args: &ImageProbeArgs) -> Result<String> {
    const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;
    let expected = args
        .expect
        .as_deref()
        .map(parse_expected_dimensions)
        .transpose()?;
    let files = collect_image_targets(&args.paths)?;

    let worker_count = files.len().clamp(1, 8);
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut slots: Vec<Option<Result<ImageInfo>>> = (0..files.len()).map(|_| None).collect();
    let slots_mutex = std::sync::Mutex::new(&mut slots);
    let header_only = args.header_only;

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let idx = next_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if idx >= files.len() {
                        break;
                    }
                    let file = &files[idx];
                    let res = (|| -> Result<ImageInfo> {
                        let meta = std::fs::metadata(file).map_err(|e| {
                            invalid(format!("cannot stat {}: {e}", file.display()))
                        })?;
                        if meta.len() > MAX_IMAGE_FILE_BYTES {
                            return Err(invalid(format!(
                                "image {} is {} MiB (exceeds 128 MiB probe limit)",
                                file.display(),
                                meta.len() / (1024 * 1024)
                            ))
                            .into());
                        }
                        let ext = file
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        if header_only && matches!(ext.as_str(), "png" | "gif" | "webp") {
                            use std::io::Read;
                            let mut buf = Vec::with_capacity(65_536.min(meta.len() as usize));
                            std::fs::File::open(file)?
                                .take(65_536)
                                .read_to_end(&mut buf)?;
                            let mut info = inspect_image_bytes(
                                &file.display().to_string(),
                                &buf,
                                expected,
                                true,
                            )?;
                            info.bytes = meta.len() as usize;
                            Ok(info)
                        } else {
                            let data = std::fs::read(file)?;
                            inspect_image_bytes(
                                &file.display().to_string(),
                                &data,
                                expected,
                                header_only,
                            )
                        }
                    })();
                    if let Ok(mut guard) = slots_mutex.lock() {
                        guard[idx] = Some(res);
                    }
                }
            });
        }
    });

    let mut items = Vec::with_capacity(files.len());
    for slot in slots.into_iter().flatten() {
        items.push(slot?);
    }

    let flagged = items.iter().filter(|i| !i.flags.is_empty()).count();
    if args.json {
        return clamp_json_budget(json!({
            "files": items.len(),
            "flagged": flagged,
            "items": items,
        }));
    }

    let mut lines = vec![format!(
        "image_probe files={} flagged={flagged}",
        items.len()
    )];
    for item in &items {
        let mut line = format!(
            "[{}] {} {}x{} · {} · {}B",
            item.path, item.format, item.width, item.height, item.color_space, item.bytes
        );
        if let Some(ref vb) = item.view_box {
            line.push_str(&format!(" · viewBox=\"{vb}\""));
        }
        if let Some(els) = item.svg_elements {
            line.push_str(&format!(" · elements={els}"));
        }
        if let Some(ref exp) = item.exposure {
            line.push_str(&format!(
                " · lum(mean={:.1}, p05={}, p50={}, p95={}, dark={:.1}%, bright={:.1}%, trans={:.1}%)",
                exp.mean_lum,
                exp.p05_lum,
                exp.p50_lum,
                exp.p95_lum,
                exp.dark_pct,
                exp.bright_pct,
                exp.transparent_pct
            ));
        }
        if !item.flags.is_empty() {
            line.push_str(&format!(" · FLAGS={:?}", item.flags));
        }
        lines.push(line);
    }
    Ok(clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET))
}
