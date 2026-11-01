use crate::{Result, invalid};
use std::collections::BTreeMap;
use std::path::Path;

pub const MAX_ARCHIVE_ENTRIES: usize = 2048;
pub const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXTRACTED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_COMPRESSION_RATIO: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub subject: Option<String>,
    pub creator: Option<String>,
    pub keywords: Option<String>,
    pub description: Option<String>,
    pub application: Option<String>,
    pub pages_or_slides: Option<String>,
}

impl DocumentMetadata {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.subject.is_none()
            && self.creator.is_none()
            && self.keywords.is_none()
            && self.description.is_none()
            && self.application.is_none()
            && self.pages_or_slides.is_none()
    }

    pub fn format_header(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("# [Document Metadata]\n");
        if let Some(title) = &self.title {
            out.push_str(&format!("Title: {title}\n"));
        }
        if let Some(creator) = &self.creator {
            out.push_str(&format!("Author: {creator}\n"));
        }
        if let Some(subject) = &self.subject {
            out.push_str(&format!("Subject: {subject}\n"));
        }
        if let Some(keywords) = &self.keywords {
            out.push_str(&format!("Keywords: {keywords}\n"));
        }
        if let Some(description) = &self.description {
            out.push_str(&format!("Description: {description}\n"));
        }
        if let Some(app) = &self.application {
            out.push_str(&format!("Application: {app}\n"));
        }
        if let Some(count) = &self.pages_or_slides {
            out.push_str(&format!("Count: {count}\n"));
        }
        out.push('\n');
        out
    }
}

#[derive(Clone, Debug)]
pub struct ZipArchive {
    entries: BTreeMap<String, Vec<u8>>,
    total_uncompressed: usize,
}

impl ZipArchive {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 22 || !bytes.starts_with(b"PK") {
            return Err(invalid("invalid ZIP document container header").into());
        }
        if let Some(eocd_offset) = find_eocd(bytes)
            && let Ok(archive) = parse_central_directory(bytes, eocd_offset)
        {
            return Ok(archive);
        }
        parse_local_headers(bytes)
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries.get(name).map(Vec::as_slice)
    }

    pub fn get_text(&self, name: &str) -> Option<String> {
        let raw = self.get(name)?;
        decode_text_bytes(raw).ok()
    }

    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn total_uncompressed(&self) -> usize {
        self.total_uncompressed
    }
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn is_safe_archive_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.starts_with('\\')
        && !name.contains('\0')
        && !name
            .split(['/', '\\'])
            .any(|part| part == "..")
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let min_offset = bytes.len().saturating_sub(22 + 65_535);
    for pos in (min_offset..=bytes.len() - 22).rev() {
        if bytes[pos..pos + 4] == [0x50, 0x4b, 0x05, 0x06] {
            let comment_len = usize::from(read_u16_le(bytes, pos + 20)?);
            if pos + 22 + comment_len <= bytes.len() {
                return Some(pos);
            }
        }
    }
    None
}

fn check_ratio_and_budget(
    compressed_size: usize,
    uncompressed_len: usize,
    total_uncompressed: &mut usize,
) -> Result<()> {
    if uncompressed_len > MAX_ENTRY_BYTES {
        return Err(invalid("ZIP document entry exceeds MAX_ENTRY_BYTES budget").into());
    }
    if uncompressed_len > 1024 * 1024
        && compressed_size > 0
        && uncompressed_len / compressed_size > MAX_COMPRESSION_RATIO
    {
        return Err(invalid("ZIP bomb detected: compression ratio exceeds safety limit").into());
    }
    *total_uncompressed = total_uncompressed.saturating_add(uncompressed_len);
    if *total_uncompressed > MAX_EXTRACTED_BYTES {
        return Err(invalid("ZIP document total extracted bytes exceed safety budget").into());
    }
    Ok(())
}

fn decompress_entry(method: u16, payload: &[u8], expected_uncompressed: usize) -> Result<Vec<u8>> {
    if expected_uncompressed > MAX_ENTRY_BYTES {
        return Err(invalid("ZIP entry declared size exceeds MAX_ENTRY_BYTES").into());
    }
    match method {
        0 => {
            if payload.len() > MAX_ENTRY_BYTES {
                return Err(invalid("stored ZIP entry exceeds MAX_ENTRY_BYTES").into());
            }
            Ok(payload.to_vec())
        }
        8 => inflate_raw_bounded(payload, MAX_ENTRY_BYTES),
        _ => Err(invalid(format!("unsupported ZIP compression method {method}")).into()),
    }
}

fn parse_central_directory(bytes: &[u8], eocd_offset: usize) -> Result<ZipArchive> {
    let total_entries = usize::from(
        read_u16_le(bytes, eocd_offset + 10)
            .ok_or_else(|| invalid("truncated EOCD entry count"))?,
    );
    if total_entries > MAX_ARCHIVE_ENTRIES {
        return Err(invalid("ZIP archive exceeds MAX_ARCHIVE_ENTRIES limit").into());
    }
    let cd_size =
        read_u32_le(bytes, eocd_offset + 12).ok_or_else(|| invalid("truncated EOCD size"))? as usize;
    let cd_offset = read_u32_le(bytes, eocd_offset + 16)
        .ok_or_else(|| invalid("truncated EOCD offset"))? as usize;
    if cd_offset.saturating_add(cd_size) > bytes.len() {
        return Err(invalid("ZIP central directory out of bounds").into());
    }
    let mut cursor = cd_offset;
    let mut entries = BTreeMap::new();
    let mut total_uncompressed = 0usize;
    for _ in 0..total_entries {
        if cursor + 46 > bytes.len()
            || bytes[cursor..cursor + 4] != [0x50, 0x4b, 0x01, 0x02]
        {
            return Err(invalid("invalid central directory file header").into());
        }
        let method = read_u16_le(bytes, cursor + 10).unwrap_or(0);
        let compressed_size = read_u32_le(bytes, cursor + 20).unwrap_or(0) as usize;
        let uncompressed_size = read_u32_le(bytes, cursor + 24).unwrap_or(0) as usize;
        let name_len = usize::from(read_u16_le(bytes, cursor + 28).unwrap_or(0));
        let extra_len = usize::from(read_u16_le(bytes, cursor + 30).unwrap_or(0));
        let comment_len = usize::from(read_u16_le(bytes, cursor + 32).unwrap_or(0));
        let local_offset = read_u32_le(bytes, cursor + 42).unwrap_or(0) as usize;
        let name_end = cursor + 46 + name_len;
        let next_cursor = name_end + extra_len + comment_len;
        if next_cursor > bytes.len() {
            return Err(invalid("truncated central directory entry").into());
        }
        let raw_name = std::str::from_utf8(&bytes[cursor + 46..name_end])
            .map_err(|_| invalid("non-UTF8 ZIP entry name"))?;
        let normalized_name = raw_name.replace('\\', "/");
        cursor = next_cursor;
        if normalized_name.ends_with('/') {
            continue;
        }
        if !is_safe_archive_entry_name(&normalized_name) {
            return Err(invalid("unsafe path traversal in ZIP entry name").into());
        }
        if local_offset + 30 > bytes.len()
            || bytes[local_offset..local_offset + 4] != [0x50, 0x4b, 0x03, 0x04]
        {
            return Err(invalid("invalid local file header offset in ZIP").into());
        }
        let local_name_len = usize::from(read_u16_le(bytes, local_offset + 26).unwrap_or(0));
        let local_extra_len = usize::from(read_u16_le(bytes, local_offset + 28).unwrap_or(0));
        let data_start = local_offset + 30 + local_name_len + local_extra_len;
        let data_end = data_start.saturating_add(compressed_size);
        if data_end > bytes.len() {
            return Err(invalid("ZIP entry payload out of bounds").into());
        }
        check_ratio_and_budget(compressed_size, uncompressed_size, &mut total_uncompressed)?;
        let data = decompress_entry(method, &bytes[data_start..data_end], uncompressed_size)?;
        if data.len() > uncompressed_size {
            check_ratio_and_budget(
                compressed_size,
                data.len() - uncompressed_size,
                &mut total_uncompressed,
            )?;
        }
        entries.insert(normalized_name, data);
    }
    Ok(ZipArchive {
        entries,
        total_uncompressed,
    })
}

fn parse_local_headers(bytes: &[u8]) -> Result<ZipArchive> {
    let mut cursor = 0usize;
    let mut entries = BTreeMap::new();
    let mut total_uncompressed = 0usize;
    while cursor + 4 <= bytes.len() {
        let sig = &bytes[cursor..cursor + 4];
        if sig == [0x50, 0x4b, 0x01, 0x02] || sig == [0x50, 0x4b, 0x05, 0x06] {
            break;
        }
        if sig != [0x50, 0x4b, 0x03, 0x04] {
            return Err(invalid("corrupt local header in ZIP stream").into());
        }
        if entries.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(invalid("ZIP archive exceeds MAX_ARCHIVE_ENTRIES limit").into());
        }
        if cursor + 30 > bytes.len() {
            return Err(invalid("truncated local file header").into());
        }
        let method = read_u16_le(bytes, cursor + 8).unwrap_or(0);
        let compressed_size = read_u32_le(bytes, cursor + 18).unwrap_or(0) as usize;
        let uncompressed_size = read_u32_le(bytes, cursor + 22).unwrap_or(0) as usize;
        let name_len = usize::from(read_u16_le(bytes, cursor + 26).unwrap_or(0));
        let extra_len = usize::from(read_u16_le(bytes, cursor + 28).unwrap_or(0));
        let name_start = cursor + 30;
        let name_end = name_start + name_len;
        let data_start = name_end + extra_len;
        let data_end = data_start.saturating_add(compressed_size);
        if data_end > bytes.len() {
            return Err(invalid("truncated local file entry payload").into());
        }
        let raw_name = std::str::from_utf8(&bytes[name_start..name_end])
            .map_err(|_| invalid("non-UTF8 local ZIP entry name"))?;
        let normalized_name = raw_name.replace('\\', "/");
        cursor = data_end;
        if normalized_name.ends_with('/') {
            continue;
        }
        if !is_safe_archive_entry_name(&normalized_name) {
            return Err(invalid("unsafe path traversal in ZIP entry name").into());
        }
        check_ratio_and_budget(compressed_size, uncompressed_size, &mut total_uncompressed)?;
        let data = decompress_entry(method, &bytes[data_start..data_end], uncompressed_size)?;
        if data.len() > uncompressed_size {
            check_ratio_and_budget(
                compressed_size,
                data.len() - uncompressed_size,
                &mut total_uncompressed,
            )?;
        }
        entries.insert(normalized_name, data);
    }
    Ok(ZipArchive {
        entries,
        total_uncompressed,
    })
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u64,
    bit_count: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bit_count: 0,
        }
    }

    #[inline]
    fn ensure_bits(&mut self, needed: u8) {
        while self.bit_count < needed && self.byte_pos < self.data.len() {
            self.bit_buf |= u64::from(self.data[self.byte_pos]) << self.bit_count;
            self.byte_pos += 1;
            self.bit_count += 8;
        }
    }

    #[inline]
    fn read_bits(&mut self, count: u8) -> Result<u32> {
        if count == 0 {
            return Ok(0);
        }
        self.ensure_bits(count);
        if self.bit_count < count {
            return Err(invalid("unexpected EOF in DEFLATE bit stream").into());
        }
        let mask = (1u64 << count) - 1;
        let val = (self.bit_buf & mask) as u32;
        self.bit_buf >>= count;
        self.bit_count -= count;
        Ok(val)
    }

    fn align_byte(&mut self) {
        let drop_bits = self.bit_count % 8;
        self.bit_buf >>= drop_bits;
        self.bit_count -= drop_bits;
    }

    fn read_u16_aligned(&mut self) -> Result<u16> {
        self.align_byte();
        let lo = self.read_bits(8)? as u16;
        let hi = self.read_bits(8)? as u16;
        Ok(lo | (hi << 8))
    }

    fn read_exact_bytes(&mut self, len: usize, out: &mut Vec<u8>) -> Result<()> {
        self.align_byte();
        for _ in 0..len {
            let b = self.read_bits(8)? as u8;
            out.push(b);
        }
        Ok(())
    }
}

struct HuffmanTree {
    counts: [u16; 16],
    symbols: Vec<u16>,
    max_bits: u8,
}

impl HuffmanTree {
    fn from_lengths(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u16; 16];
        let mut max_bits = 0u8;
        for &len in lengths {
            if len > 15 {
                return Err(invalid("invalid Huffman code length > 15").into());
            }
            if len > 0 {
                counts[usize::from(len)] += 1;
                max_bits = max_bits.max(len);
            }
        }
        let mut offsets = [0u16; 16];
        let mut total = 0u16;
        for bits in 1..=15 {
            offsets[bits] = total;
            total = total.saturating_add(counts[bits]);
        }
        let mut symbols = vec![0u16; usize::from(total)];
        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let idx = usize::from(offsets[usize::from(len)]);
                offsets[usize::from(len)] += 1;
                symbols[idx] = sym as u16;
            }
        }
        Ok(Self {
            counts,
            symbols,
            max_bits,
        })
    }

    fn fixed_lit_len() -> Self {
        let mut lengths = [0u8; 288];
        lengths[..144].fill(8);
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        lengths[280..288].fill(8);
        Self::from_lengths(&lengths).expect("valid fixed literal/length tree")
    }

    fn fixed_dist() -> Self {
        let lengths = [5u8; 32];
        Self::from_lengths(&lengths).expect("valid fixed distance tree")
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=usize::from(self.max_bits) {
            code |= reader.read_bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                let sym_idx = (index + (code - first)) as usize;
                return self
                    .symbols
                    .get(sym_idx)
                    .copied()
                    .ok_or_else(|| invalid("Huffman symbol out of range").into());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(invalid("invalid Huffman code in DEFLATE stream").into())
    }
}

pub fn inflate_raw_bounded(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut reader = BitReader::new(input);
    let mut out = Vec::with_capacity(input.len().saturating_mul(2).min(max_output).min(65_536));
    loop {
        let bfinal = reader.read_bits(1)?;
        let btype = reader.read_bits(2)?;
        match btype {
            0 => {
                let len = usize::from(reader.read_u16_aligned()?);
                let nlen = usize::from(reader.read_u16_aligned()?);
                if (len ^ 0xffff) != nlen {
                    return Err(invalid("corrupt stored block length in DEFLATE").into());
                }
                if out.len().saturating_add(len) > max_output {
                    return Err(invalid("DEFLATE output exceeds max_output budget").into());
                }
                reader.read_exact_bytes(len, &mut out)?;
            }
            1 | 2 => {
                let (lit_tree, dist_tree) = if btype == 1 {
                    (HuffmanTree::fixed_lit_len(), HuffmanTree::fixed_dist())
                } else {
                    decode_dynamic_trees(&mut reader)?
                };
                decode_huffman_block(&mut reader, &lit_tree, &dist_tree, &mut out, max_output)?;
            }
            _ => return Err(invalid("reserved DEFLATE block type 3").into()),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

pub fn inflate_zlib_bounded(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 6 {
        return Err(invalid("zlib stream too short").into());
    }
    let cmf = input[0];
    let flg = input[1];
    if (cmf & 0x0f) != 8 || ((u16::from(cmf) << 8) + u16::from(flg)) % 31 != 0 {
        return Err(invalid("invalid zlib header").into());
    }
    let header_len = if (flg & 0x20) != 0 { 6 } else { 2 };
    if input.len() <= header_len {
        return Err(invalid("truncated zlib payload").into());
    }
    inflate_raw_bounded(&input[header_len..], max_output)
}

fn decode_dynamic_trees(reader: &mut BitReader<'_>) -> Result<(HuffmanTree, HuffmanTree)> {
    let hlit = (reader.read_bits(5)? as usize) + 257;
    let hdist = (reader.read_bits(5)? as usize) + 1;
    let hclen = (reader.read_bits(4)? as usize) + 4;
    const CL_ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CL_ORDER[i]] = reader.read_bits(3)? as u8;
    }
    let cl_tree = HuffmanTree::from_lengths(&cl_lengths)?;
    let total_codes = hlit + hdist;
    let mut lengths = Vec::with_capacity(total_codes);
    while lengths.len() < total_codes {
        let sym = cl_tree.decode(reader)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths
                    .last()
                    .ok_or_else(|| invalid("DEFLATE repeat code 16 with no previous length"))?;
                let repeat = (reader.read_bits(2)? as usize) + 3;
                for _ in 0..repeat {
                    lengths.push(prev);
                }
            }
            17 => {
                let repeat = (reader.read_bits(3)? as usize) + 3;
                lengths.resize(lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = (reader.read_bits(7)? as usize) + 11;
                lengths.resize(lengths.len() + repeat, 0);
            }
            _ => return Err(invalid("invalid code length symbol in DEFLATE").into()),
        }
        if lengths.len() > total_codes {
            return Err(invalid("DEFLATE dynamic code lengths overflow").into());
        }
    }
    let lit_tree = HuffmanTree::from_lengths(&lengths[..hlit])?;
    let dist_tree = HuffmanTree::from_lengths(&lengths[hlit..])?;
    Ok((lit_tree, dist_tree))
}

fn decode_huffman_block(
    reader: &mut BitReader<'_>,
    lit_tree: &HuffmanTree,
    dist_tree: &HuffmanTree,
    out: &mut Vec<u8>,
    max_output: usize,
) -> Result<()> {
    const LEN_BASE: [usize; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    loop {
        let sym = lit_tree.decode(reader)?;
        match sym {
            0..=255 => {
                if out.len() >= max_output {
                    return Err(invalid("DEFLATE output exceeds max_output budget").into());
                }
                out.push(sym as u8);
            }
            256 => break,
            257..=285 => {
                let len_idx = usize::from(sym - 257);
                let length =
                    LEN_BASE[len_idx] + (reader.read_bits(LEN_EXTRA[len_idx])? as usize);
                let dist_sym = usize::from(dist_tree.decode(reader)?);
                if dist_sym >= 30 {
                    return Err(invalid("invalid distance code in DEFLATE").into());
                }
                let distance =
                    DIST_BASE[dist_sym] + (reader.read_bits(DIST_EXTRA[dist_sym])? as usize);
                if distance == 0 || distance > out.len() {
                    return Err(invalid("DEFLATE back-reference distance out of bounds").into());
                }
                if out.len().saturating_add(length) > max_output {
                    return Err(invalid("DEFLATE output exceeds max_output budget").into());
                }
                let start = out.len() - distance;
                for i in 0..length {
                    let b = out[start + i];
                    out.push(b);
                }
            }
            _ => return Err(invalid("invalid literal/length symbol in DEFLATE").into()),
        }
    }
    Ok(())
}

/// Encode bytes into a valid RFC 1951 Fixed-Huffman DEFLATE stream (BTYPE=01, BFINAL=1).
pub fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    struct BitWriter {
        bytes: Vec<u8>,
        buf: u64,
        count: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                buf: 0,
                count: 0,
            }
        }
        fn write_bits(&mut self, val: u32, bits: u8) {
            self.buf |= u64::from(val) << self.count;
            self.count += bits;
            while self.count >= 8 {
                self.bytes.push((self.buf & 0xff) as u8);
                self.buf >>= 8;
                self.count -= 8;
            }
        }
        fn write_huffman_msb(&mut self, code: u16, bits: u8) {
            for i in (0..bits).rev() {
                self.write_bits(u32::from((code >> i) & 1), 1);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.count > 0 {
                self.bytes.push((self.buf & 0xff) as u8);
            }
            self.bytes
        }
    }
    let mut writer = BitWriter::new();
    writer.write_bits(1, 1); // BFINAL = 1
    writer.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)
    for &b in data {
        if b < 144 {
            writer.write_huffman_msb(0x30 + u16::from(b), 8);
        } else {
            writer.write_huffman_msb(0x190 + u16::from(b - 144), 9);
        }
    }
    writer.write_huffman_msb(0, 7); // End-of-block symbol 256 (0000000)
    writer.finish()
}

/// Build an in-memory ZIP archive (supporting Stored method=0 and Deflated method=8).
pub fn build_zip_archive(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for &(name, payload, deflate) in entries {
        let local_offset = out.len() as u32;
        let (method, compressed) = if deflate {
            (8u16, deflate_fixed(payload))
        } else {
            (0u16, payload.to_vec())
        };
        let name_bytes = name.as_bytes();
        // Local File Header
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&compressed);

        // Central Directory File Header
        cd.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&local_offset.to_le_bytes());
        cd.extend_from_slice(name_bytes);
    }
    let cd_offset = out.len() as u32;
    let cd_size = cd.len() as u32;
    let count = entries.len() as u16;
    out.extend_from_slice(&cd);
    // EOCD
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub fn decode_text_bytes(bytes: &[u8]) -> Result<String> {
    if let Some(stripped) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let s = std::str::from_utf8(stripped)
            .map_err(|e| invalid(format!("invalid UTF-8 after BOM: {e}")))?;
        return Ok(normalize_line_endings(s));
    }
    if let Some(stripped) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(stripped, true);
    }
    if let Some(stripped) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(stripped, false);
    }
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) {
        if bytes[1] == 0 && bytes[3] == 0 && bytes[0] != 0 {
            return decode_utf16(bytes, true);
        }
        if bytes[0] == 0 && bytes[2] == 0 && bytes[1] != 0 {
            return decode_utf16(bytes, false);
        }
    }
    let s = std::str::from_utf8(bytes).map_err(|e| invalid(format!("non-UTF8 text: {e}")))?;
    Ok(normalize_line_endings(s))
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String> {
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| {
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut decoded = String::with_capacity(bytes.len() / 2);
    for ch in char::decode_utf16(units) {
        let ch = ch.map_err(|_| invalid("invalid UTF-16 surrogate sequence"))?;
        if ch != '\u{feff}' && ch != '\0' {
            decoded.push(ch);
        }
    }
    Ok(normalize_line_endings(&decoded))
}

pub fn normalize_line_endings(input: &str) -> String {
    if !input.contains('\r') && !input.contains('\0') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                out.push('\n');
            }
            '\0' => {}
            other => out.push(other),
        }
    }
    out
}

pub fn decode_xml_entities(input: &str) -> String {
    if !input.contains('&') && !input.contains('\r') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        if let Some(semi) = after_amp.find(';') {
            let entity = &after_amp[..semi];
            let decoded = match entity {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(ch) = decoded {
                if ch != '\0' {
                    out.push(ch);
                }
                rest = &after_amp[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    normalize_line_endings(&out)
}

pub fn extract_xml_tag_text(xml: &str, tag_suffix: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(rel) = xml[search_from..].find('<') {
        let start = search_from + rel;
        let after_lt = &xml[start + 1..];
        let Some(gt_rel) = after_lt.find('>') else {
            break;
        };
        let raw_tag = &after_lt[..gt_rel];
        if !raw_tag.starts_with('/') && !raw_tag.starts_with('?') && !raw_tag.starts_with('!') {
            let self_closing = raw_tag.ends_with('/');
            let tag_name = raw_tag
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            let local_name = tag_name.rsplit_once(':').map_or(tag_name, |(_, l)| l);
            if local_name.eq_ignore_ascii_case(tag_suffix) && !self_closing {
                let content_start = start + 1 + gt_rel + 1;
                let close_pat = format!("</{tag_name}>");
                if let Some(close_rel) = xml[content_start..].find(&close_pat) {
                    let inner = &xml[content_start..content_start + close_rel];
                    let text = decode_xml_entities(inner).trim().to_owned();
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
        search_from = start + 1 + gt_rel + 1;
    }
    None
}

pub fn extract_archive_metadata(archive: &ZipArchive) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();
    if let Some(core_xml) = archive.get_text("docProps/core.xml") {
        meta.title = extract_xml_tag_text(&core_xml, "title");
        meta.subject = extract_xml_tag_text(&core_xml, "subject");
        meta.creator = extract_xml_tag_text(&core_xml, "creator")
            .or_else(|| extract_xml_tag_text(&core_xml, "lastModifiedBy"));
        meta.keywords = extract_xml_tag_text(&core_xml, "keywords");
        meta.description = extract_xml_tag_text(&core_xml, "description");
    }
    if let Some(app_xml) = archive.get_text("docProps/app.xml") {
        meta.application = extract_xml_tag_text(&app_xml, "Application");
        meta.pages_or_slides = extract_xml_tag_text(&app_xml, "Slides")
            .map(|n| format!("{n} slides"))
            .or_else(|| extract_xml_tag_text(&app_xml, "Pages").map(|n| format!("{n} pages")));
    }
    if let Some(odf_meta) = archive.get_text("meta.xml") {
        if meta.title.is_none() {
            meta.title = extract_xml_tag_text(&odf_meta, "title");
        }
        if meta.subject.is_none() {
            meta.subject = extract_xml_tag_text(&odf_meta, "subject");
        }
        if meta.creator.is_none() {
            meta.creator = extract_xml_tag_text(&odf_meta, "creator")
                .or_else(|| extract_xml_tag_text(&odf_meta, "initial-creator"));
        }
        if meta.keywords.is_none() {
            meta.keywords = extract_xml_tag_text(&odf_meta, "keyword");
        }
        if meta.description.is_none() {
            meta.description = extract_xml_tag_text(&odf_meta, "description");
        }
    }
    meta
}

pub fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

pub fn extract_xml_attr(tag: &str, attr_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace).unwrap_or(tag.len());
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len()
            && bytes[i] != b'='
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = &tag[key_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let val = if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let slice = &tag[val_start..i];
            if i < bytes.len() {
                i += 1;
            }
            slice
        } else {
            let val_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
                i += 1;
            }
            &tag[val_start..i]
        };
        let local_k = key.rsplit_once(':').map_or(key, |(_, l)| l);
        if key.eq_ignore_ascii_case(attr_name) || local_k.eq_ignore_ascii_case(attr_name) {
            return Some(decode_xml_entities(val));
        }
    }
    None
}

pub fn parse_ooxml_relationships(rels_xml: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut cursor = 0usize;
    while let Some(rel) = rels_xml[cursor..].find("<Relationship") {
        let start = cursor + rel;
        let Some(gt_rel) = rels_xml[start..].find('>') else {
            break;
        };
        let tag = &rels_xml[start + 1..start + gt_rel];
        if let (Some(id), Some(target)) = (
            extract_xml_attr(tag, "Id"),
            extract_xml_attr(tag, "Target"),
        ) {
            map.insert(id, target);
        }
        cursor = start + gt_rel + 1;
    }
    map
}

fn word_heading_level(style_val: &str, outline_lvl: Option<usize>) -> Option<usize> {
    let lower = style_val.to_ascii_lowercase();
    if lower == "title" || lower == "제목" {
        return Some(1);
    }
    if lower == "subtitle" || lower == "부제" {
        return Some(2);
    }
    for prefix in ["heading", "heading ", "제목", "제목 ", "h"] {
        if let Some(rest) = lower.strip_prefix(prefix)
            && let Ok(n) = rest.trim().parse::<usize>()
            && (1..=6).contains(&n)
        {
            return Some(n);
        }
    }
    outline_lvl.map(|lvl| (lvl + 1).clamp(1, 6))
}

pub fn extract_docx_xml_paragraphs(
    xml: &str,
    rels: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cursor = 0usize;
    while cursor < xml.len() {
        let next_tbl = xml[cursor..].find("<w:tbl").and_then(|pos| {
            let after = xml.as_bytes().get(cursor + pos + 6).copied().unwrap_or(b' ');
            (after == b'>' || after.is_ascii_whitespace()).then_some(cursor + pos)
        });
        let next_p = find_word_tag(xml, cursor, "w:p");
        match (next_tbl, next_p) {
            (Some(tbl_pos), Some(p_pos)) if tbl_pos < p_pos => {
                if let Some(end_tbl) = xml[tbl_pos..].find("</w:tbl>") {
                    let tbl_slice = &xml[tbl_pos..tbl_pos + end_tbl + 8];
                    let table_lines = extract_word_table(tbl_slice, rels);
                    if !table_lines.is_empty() {
                        lines.extend(table_lines);
                        lines.push(String::new());
                    }
                    cursor = tbl_pos + end_tbl + 8;
                    continue;
                }
                cursor = tbl_pos + 6;
            }
            (_, Some(p_pos)) => {
                if let Some((p_text, next_pos)) = extract_single_word_paragraph(xml, p_pos, rels) {
                    if !p_text.trim().is_empty() {
                        lines.push(p_text);
                    }
                    cursor = next_pos;
                } else {
                    cursor = p_pos + 4;
                }
            }
            _ => break,
        }
    }
    lines
}

fn find_word_tag(xml: &str, from: usize, tag_name: &str) -> Option<usize> {
    let mut search = from;
    let needle = format!("<{tag_name}");
    while let Some(rel) = xml[search..].find(&needle) {
        let pos = search + rel;
        let next_byte = xml.as_bytes().get(pos + needle.len()).copied().unwrap_or(b' ');
        if next_byte == b'>' || next_byte == b'/' || next_byte.is_ascii_whitespace() {
            return Some(pos);
        }
        search = pos + needle.len();
    }
    None
}

fn extract_single_word_paragraph(
    xml: &str,
    p_start: usize,
    rels: &BTreeMap<String, String>,
) -> Option<(String, usize)> {
    let gt_rel = xml[p_start..].find('>')?;
    let open_tag = &xml[p_start + 1..p_start + gt_rel];
    if open_tag.ends_with('/') {
        return Some((String::new(), p_start + gt_rel + 1));
    }
    let body_start = p_start + gt_rel + 1;
    let close_rel = xml[body_start..].find("</w:p>")?;
    let p_body = &xml[body_start..body_start + close_rel];
    let next_pos = body_start + close_rel + 6;

    let mut style_val: Option<String> = None;
    let mut outline_lvl: Option<usize> = None;
    let mut list_ilvl: Option<usize> = None;
    let mut has_num_pr = false;
    let mut text_buf = String::new();
    let mut active_hyperlink: Option<String> = None;
    let mut hyperlink_text = String::new();
    let mut in_del = false;

    let mut pos = 0usize;
    while let Some(lt) = p_body[pos..].find('<') {
        let tag_start = pos + lt;
        let Some(gt) = p_body[tag_start..].find('>') else {
            break;
        };
        let raw_tag = &p_body[tag_start + 1..tag_start + gt];
        let next_tag_end = tag_start + gt + 1;
        let trimmed = raw_tag.trim();
        let is_close = trimmed.starts_with('/');
        let is_self_close = trimmed.ends_with('/');
        let name = trimmed
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");

        match name {
            "w:pStyle" if !is_close => {
                style_val = extract_xml_attr(trimmed, "val");
            }
            "w:outlineLvl" if !is_close => {
                outline_lvl = extract_xml_attr(trimmed, "val").and_then(|v| v.parse().ok());
            }
            "w:numPr" if !is_close => {
                has_num_pr = true;
            }
            "w:ilvl" if !is_close => {
                list_ilvl = extract_xml_attr(trimmed, "val").and_then(|v| v.parse().ok());
            }
            "w:bookmarkStart" if !is_close => {
                if let Some(bm) = extract_xml_attr(trimmed, "name")
                    && !bm.starts_with('_')
                {
                    text_buf.push_str(&format!("{{#{bm}}} "));
                }
            }
            "w:hyperlink" if !is_close && !is_self_close => {
                let target = extract_xml_attr(trimmed, "id")
                    .and_then(|id| rels.get(&id).cloned())
                    .or_else(|| extract_xml_attr(trimmed, "anchor").map(|a| format!("#{a}")));
                active_hyperlink = target;
                hyperlink_text.clear();
            }
            "w:hyperlink" if is_close => {
                if let Some(url) = active_hyperlink.take() {
                    if hyperlink_text.is_empty() {
                        text_buf.push_str(&url);
                    } else {
                        text_buf.push_str(&format!("[{}]({})", hyperlink_text.trim(), url));
                    }
                    hyperlink_text.clear();
                }
            }
            "w:del" if !is_close && !is_self_close => {
                in_del = true;
            }
            "w:del" if is_close => {
                in_del = false;
            }
            "w:br" | "w:cr" if !is_close => {
                text_buf.push(' ');
            }
            "w:tab" if !is_close => {
                text_buf.push('\t');
            }
            "w:t" | "w:delText" if !is_close && !is_self_close => {
                let close_needle = format!("</{name}>");
                if let Some(close_t) = p_body[next_tag_end..].find(&close_needle) {
                    let raw_inner = &p_body[next_tag_end..next_tag_end + close_t];
                    let decoded = decode_xml_entities(raw_inner);
                    if name == "w:delText" || in_del {
                        let snippet = format!("~~{}~~", decoded);
                        if active_hyperlink.is_some() {
                            hyperlink_text.push_str(&snippet);
                        } else {
                            text_buf.push_str(&snippet);
                        }
                    } else if active_hyperlink.is_some() {
                        hyperlink_text.push_str(&decoded);
                    } else {
                        text_buf.push_str(&decoded);
                    }
                    pos = next_tag_end + close_t + close_needle.len();
                    continue;
                }
            }
            _ => {}
        }
        pos = next_tag_end;
    }

    let cleaned = text_buf.trim().to_owned();
    if cleaned.is_empty() {
        return Some((String::new(), next_pos));
    }

    if let Some(level) = style_val
        .as_deref()
        .and_then(|s| word_heading_level(s, outline_lvl))
        .or_else(|| outline_lvl.map(|l| (l + 1).clamp(1, 6)))
    {
        let hashes = "#".repeat(level);
        return Some((format!("{hashes} {cleaned}"), next_pos));
    }

    if has_num_pr || list_ilvl.is_some() {
        let indent = "  ".repeat(list_ilvl.unwrap_or(0));
        return Some((format!("{indent}- {cleaned}"), next_pos));
    }

    Some((cleaned, next_pos))
}

fn extract_word_table(tbl_xml: &str, rels: &BTreeMap<String, String>) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut tr_cursor = 0usize;
    while let Some(tr_pos) = find_word_tag(tbl_xml, tr_cursor, "w:tr") {
        let Some(end_tr) = tbl_xml[tr_pos..].find("</w:tr>") else {
            break;
        };
        let tr_slice = &tbl_xml[tr_pos..tr_pos + end_tr + 7];
        let mut row_cells = Vec::new();
        let mut tc_cursor = 0usize;
        while let Some(tc_pos) = find_word_tag(tr_slice, tc_cursor, "w:tc") {
            let Some(end_tc) = tr_slice[tc_pos..].find("</w:tc>") else {
                break;
            };
            let tc_slice = &tr_slice[tc_pos..tc_pos + end_tc + 7];
            let mut cell_parts = Vec::new();
            let mut p_cursor = 0usize;
            while let Some(p_pos) = find_word_tag(tc_slice, p_cursor, "w:p") {
                if let Some((p_text, next_p)) = extract_single_word_paragraph(tc_slice, p_pos, rels)
                {
                    let trimmed = p_text.trim();
                    if !trimmed.is_empty() {
                        cell_parts.push(trimmed.replace('|', "\\|"));
                    }
                    p_cursor = next_p;
                } else {
                    break;
                }
            }
            row_cells.push(cell_parts.join(" "));
            tc_cursor = tc_pos + end_tc + 7;
        }
        if !row_cells.is_empty() && row_cells.iter().any(|c| !c.is_empty()) {
            rows.push(row_cells);
        }
        tr_cursor = tr_pos + end_tr + 7;
    }
    format_markdown_table(&rows)
}

pub fn format_markdown_table(rows: &[Vec<String>]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (idx, row) in rows.iter().enumerate() {
        let mut padded: Vec<String> = row
            .iter()
            .map(|cell| {
                cell.replace("\r\n", " ")
                    .replace(['\r', '\n'], " ")
                    .replace('|', "\\|")
            })
            .collect();
        while padded.len() < col_count {
            padded.push(String::new());
        }
        lines.push(format!("| {} |", padded.join(" | ")));
        if idx == 0 {
            let sep = vec!["---"; col_count].join(" | ");
            lines.push(format!("| {sep} |"));
        }
    }
    lines
}

fn extract_word_notes_or_comments(xml: &str, item_tag: &str, prefix_label: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let empty_rels = BTreeMap::new();
    let close_tag = format!("</{item_tag}>");
    while let Some(pos) = find_word_tag(xml, cursor, item_tag) {
        let Some(gt) = xml[pos..].find('>') else {
            break;
        };
        let open_tag = &xml[pos + 1..pos + gt];
        if open_tag.ends_with('/') {
            cursor = pos + gt + 1;
            continue;
        }
        let body_start = pos + gt + 1;
        let Some(end_rel) = xml[body_start..].find(&close_tag) else {
            break;
        };
        let body = &xml[body_start..body_start + end_rel];
        let id = extract_xml_attr(open_tag, "id").unwrap_or_default();
        let note_type = extract_xml_attr(open_tag, "type").unwrap_or_default();
        if note_type == "separator" || note_type == "continuationSeparator" {
            cursor = body_start + end_rel + close_tag.len();
            continue;
        }
        let author = extract_xml_attr(open_tag, "author");
        let date = extract_xml_attr(open_tag, "date");
        let paragraphs = extract_docx_xml_paragraphs(body, &empty_rels);
        let combined = paragraphs.join(" ").trim().to_owned();
        if !combined.is_empty() {
            if let Some(author) = author {
                if let Some(date) = date {
                    out.push(format!("- [{prefix_label} #{id} by {author} ({date})]: {combined}"));
                } else {
                    out.push(format!("- [{prefix_label} #{id} by {author}]: {combined}"));
                }
            } else {
                out.push(format!("[^{id}]: {combined}"));
            }
        }
        cursor = body_start + end_rel + close_tag.len();
    }
    out
}

pub fn extract_docx(archive: &ZipArchive) -> Result<String> {
    let meta = extract_archive_metadata(archive);
    let mut out = meta.format_header();
    let rels = archive
        .get_text("word/_rels/document.xml.rels")
        .map(|xml| parse_ooxml_relationships(&xml))
        .unwrap_or_default();

    let mut header_names: Vec<&str> = archive
        .entry_names()
        .filter(|n| n.starts_with("word/header") && n.ends_with(".xml"))
        .collect();
    header_names.sort_unstable();
    let mut header_lines = Vec::new();
    for name in header_names {
        if let Some(xml) = archive.get_text(name) {
            header_lines.extend(extract_docx_xml_paragraphs(&xml, &rels));
        }
    }
    if !header_lines.is_empty() {
        out.push_str("## [Document Header]\n");
        for line in header_lines {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }

    let doc_xml = archive
        .get_text("word/document.xml")
        .ok_or_else(|| invalid("missing word/document.xml in DOCX container"))?;
    let body_lines = extract_docx_xml_paragraphs(&doc_xml, &rels);
    for line in body_lines {
        out.push_str(&line);
        out.push('\n');
    }

    if let Some(footnotes_xml) = archive.get_text("word/footnotes.xml") {
        let notes = extract_word_notes_or_comments(&footnotes_xml, "w:footnote", "Footnote");
        if !notes.is_empty() {
            out.push_str("\n## [Footnotes]\n");
            for note in notes {
                out.push_str(&note);
                out.push('\n');
            }
        }
    }

    if let Some(endnotes_xml) = archive.get_text("word/endnotes.xml") {
        let notes = extract_word_notes_or_comments(&endnotes_xml, "w:endnote", "Endnote");
        if !notes.is_empty() {
            out.push_str("\n## [Endnotes]\n");
            for note in notes {
                out.push_str(&note);
                out.push('\n');
            }
        }
    }

    if let Some(comments_xml) = archive.get_text("word/comments.xml") {
        let comments = extract_word_notes_or_comments(&comments_xml, "w:comment", "Comment");
        if !comments.is_empty() {
            out.push_str("\n## [Comments]\n");
            for comment in comments {
                out.push_str(&comment);
                out.push('\n');
            }
        }
    }

    let mut footer_names: Vec<&str> = archive
        .entry_names()
        .filter(|n| n.starts_with("word/footer") && n.ends_with(".xml"))
        .collect();
    footer_names.sort_unstable();
    let mut footer_lines = Vec::new();
    for name in footer_names {
        if let Some(xml) = archive.get_text(name) {
            footer_lines.extend(extract_docx_xml_paragraphs(&xml, &rels));
        }
    }
    if !footer_lines.is_empty() {
        out.push_str("\n## [Document Footer]\n");
        for line in footer_lines {
            out.push_str(&line);
            out.push('\n');
        }
    }

    Ok(out)
}

fn parse_trailing_number(stem: &str, prefix: &str) -> Option<usize> {
    let rest = stem.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<usize>().ok()
}

fn extract_drawingml_paragraphs(
    xml: &str,
    rels: &BTreeMap<String, String>,
    bullet_default: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cursor = 0usize;
    while let Some(p_pos) = find_word_tag(xml, cursor, "a:p") {
        let Some(gt) = xml[p_pos..].find('>') else {
            break;
        };
        let open_tag = &xml[p_pos + 1..p_pos + gt];
        if open_tag.ends_with('/') {
            cursor = p_pos + gt + 1;
            continue;
        }
        let body_start = p_pos + gt + 1;
        let Some(end_rel) = xml[body_start..].find("</a:p>") else {
            break;
        };
        let p_body = &xml[body_start..body_start + end_rel];
        cursor = body_start + end_rel + 6;

        let mut lvl: Option<usize> = None;
        let mut has_bu_none = false;
        let mut has_bu = false;
        let mut text_buf = String::new();
        let mut pending_link: Option<String> = None;

        let mut pos = 0usize;
        while let Some(lt) = p_body[pos..].find('<') {
            let tag_start = pos + lt;
            let Some(tag_gt) = p_body[tag_start..].find('>') else {
                break;
            };
            let raw_tag = &p_body[tag_start + 1..tag_start + tag_gt];
            let next_tag_end = tag_start + tag_gt + 1;
            let trimmed = raw_tag.trim();
            let is_close = trimmed.starts_with('/');
            let is_self_close = trimmed.ends_with('/');
            let name = trimmed
                .trim_start_matches('/')
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            match name {
                "a:pPr" if !is_close => {
                    lvl = extract_xml_attr(trimmed, "lvl").and_then(|v| v.parse().ok());
                }
                "a:buNone" if !is_close => {
                    has_bu_none = true;
                }
                "a:buChar" | "a:buAutoNum" if !is_close => {
                    has_bu = true;
                }
                "a:hlinkClick" if !is_close => {
                    if let Some(id) = extract_xml_attr(trimmed, "id")
                        && let Some(target) = rels.get(&id)
                    {
                        pending_link = Some(target.clone());
                    }
                }
                "a:br" if !is_close => {
                    text_buf.push(' ');
                }
                "a:tab" if !is_close => {
                    text_buf.push('\t');
                }
                "a:t" | "dgm:t" | "c:v" if !is_close && !is_self_close => {
                    let close_needle = format!("</{name}>");
                    if let Some(close_t) = p_body[next_tag_end..].find(&close_needle) {
                        let raw_inner = &p_body[next_tag_end..next_tag_end + close_t];
                        let decoded = decode_xml_entities(raw_inner);
                        if let Some(url) = pending_link.take() {
                            text_buf.push_str(&format!("[{}]({})", decoded.trim(), url));
                        } else {
                            text_buf.push_str(&decoded);
                        }
                        pos = next_tag_end + close_t + close_needle.len();
                        continue;
                    }
                }
                _ => {}
            }
            pos = next_tag_end;
        }
        let cleaned = text_buf.trim().to_owned();
        if cleaned.is_empty() {
            continue;
        }
        if !has_bu_none && (has_bu || lvl.is_some() || bullet_default) {
            let indent = "  ".repeat(lvl.unwrap_or(0));
            lines.push(format!("{indent}- {cleaned}"));
        } else {
            lines.push(cleaned);
        }
    }
    lines
}

fn extract_drawingml_table(tbl_xml: &str, rels: &BTreeMap<String, String>) -> Vec<String> {
    let mut rows = Vec::new();
    let mut tr_cursor = 0usize;
    while let Some(tr_pos) = find_word_tag(tbl_xml, tr_cursor, "a:tr") {
        let Some(end_tr) = tbl_xml[tr_pos..].find("</a:tr>") else {
            break;
        };
        let tr_slice = &tbl_xml[tr_pos..tr_pos + end_tr + 7];
        let mut cells = Vec::new();
        let mut tc_cursor = 0usize;
        while let Some(tc_pos) = find_word_tag(tr_slice, tc_cursor, "a:tc") {
            let Some(end_tc) = tr_slice[tc_pos..].find("</a:tc>") else {
                break;
            };
            let tc_slice = &tr_slice[tc_pos..tc_pos + end_tc + 7];
            let p_lines = extract_drawingml_paragraphs(tc_slice, rels, false);
            let joined = p_lines
                .into_iter()
                .map(|s| s.replace('|', "\\|"))
                .collect::<Vec<_>>()
                .join(" ");
            cells.push(joined);
            tc_cursor = tc_pos + end_tc + 7;
        }
        if !cells.is_empty() && cells.iter().any(|c| !c.is_empty()) {
            rows.push(cells);
        }
        tr_cursor = tr_pos + end_tr + 7;
    }
    format_markdown_table(&rows)
}

fn extract_pptx_slide_content(
    slide_num: usize,
    slide_xml: &str,
    rels: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut title: Option<String> = None;
    let mut subtitle: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();

    let mut sp_cursor = 0usize;
    while sp_cursor < slide_xml.len() {
        let next_tbl = find_word_tag(slide_xml, sp_cursor, "a:tbl");
        let next_sp = find_word_tag(slide_xml, sp_cursor, "p:sp");
        match (next_tbl, next_sp) {
            (Some(tbl_pos), Some(sp_pos)) if tbl_pos < sp_pos => {
                if let Some(end_tbl) = slide_xml[tbl_pos..].find("</a:tbl>") {
                    let tbl_slice = &slide_xml[tbl_pos..tbl_pos + end_tbl + 8];
                    let table_lines = extract_drawingml_table(tbl_slice, rels);
                    if !table_lines.is_empty() {
                        body_lines.extend(table_lines);
                    }
                    sp_cursor = tbl_pos + end_tbl + 8;
                    continue;
                }
                sp_cursor = tbl_pos + 6;
            }
            (Some(tbl_pos), None) => {
                if let Some(end_tbl) = slide_xml[tbl_pos..].find("</a:tbl>") {
                    let tbl_slice = &slide_xml[tbl_pos..tbl_pos + end_tbl + 8];
                    let table_lines = extract_drawingml_table(tbl_slice, rels);
                    if !table_lines.is_empty() {
                        body_lines.extend(table_lines);
                    }
                    sp_cursor = tbl_pos + end_tbl + 8;
                    continue;
                }
                break;
            }
            (_, Some(sp_pos)) => {
                let Some(end_sp) = slide_xml[sp_pos..].find("</p:sp>") else {
                    break;
                };
                let sp_slice = &slide_xml[sp_pos..sp_pos + end_sp + 7];
                sp_cursor = sp_pos + end_sp + 7;

                let ph_type = find_word_tag(sp_slice, 0, "p:ph").and_then(|ph_pos| {
                    let gt = sp_slice[ph_pos..].find('>')?;
                    let tag = &sp_slice[ph_pos + 1..ph_pos + gt];
                    extract_xml_attr(tag, "type").or_else(|| Some("body".into()))
                });

                let is_title = matches!(
                    ph_type.as_deref(),
                    Some("title") | Some("ctrTitle")
                );
                let is_subtitle = matches!(ph_type.as_deref(), Some("subTitle"));
                let is_body_placeholder = matches!(ph_type.as_deref(), Some("body") | Some("obj"));

                let paras = extract_drawingml_paragraphs(sp_slice, rels, is_body_placeholder);
                if paras.is_empty() {
                    continue;
                }
                if is_title && title.is_none() {
                    title = Some(
                        paras
                            .into_iter()
                            .map(|l| l.trim_start_matches("- ").to_owned())
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                } else if is_subtitle && subtitle.is_none() {
                    subtitle = Some(
                        paras
                            .into_iter()
                            .map(|l| l.trim_start_matches("- ").to_owned())
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                } else {
                    body_lines.extend(paras);
                }
            }
            (None, None) => break,
        }
    }

    let mut out = Vec::new();
    if let Some(title) = title {
        out.push(format!("## [Slide {slide_num}] {title}"));
    } else {
        out.push(format!("## [Slide {slide_num}]"));
    }
    if let Some(sub) = subtitle {
        out.push(format!("### {sub}"));
    }
    out.extend(body_lines);
    out
}

fn extract_all_local_tag_texts(xml: &str, local_tags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(lt) = xml[pos..].find('<') {
        let start = pos + lt;
        let Some(gt) = xml[start..].find('>') else {
            break;
        };
        let raw_tag = &xml[start + 1..start + gt];
        let next_pos = start + gt + 1;
        let trimmed = raw_tag.trim();
        if !trimmed.starts_with('/') && !trimmed.starts_with('?') && !trimmed.starts_with('!') && !trimmed.ends_with('/') {
            let name = trimmed.split_whitespace().next().unwrap_or("");
            let local = name.rsplit_once(':').map_or(name, |(_, l)| l);
            if local_tags.iter().any(|t| t.eq_ignore_ascii_case(local)) {
                let close_pat = format!("</{name}>");
                if let Some(close_rel) = xml[next_pos..].find(&close_pat) {
                    let inner = &xml[next_pos..next_pos + close_rel];
                    if !inner.contains('<') {
                        let decoded = decode_xml_entities(inner).trim().to_owned();
                        if !decoded.is_empty() {
                            out.push(decoded);
                        }
                        pos = next_pos + close_rel + close_pat.len();
                        continue;
                    }
                }
            }
        }
        pos = next_pos;
    }
    out
}

pub fn extract_pptx(archive: &ZipArchive) -> Result<String> {
    let meta = extract_archive_metadata(archive);
    let mut out = meta.format_header();

    let mut slide_entries: Vec<(usize, String)> = archive
        .entry_names()
        .filter_map(|name| {
            let file = name.strip_prefix("ppt/slides/")?;
            let stem = file.strip_suffix(".xml")?;
            let num = parse_trailing_number(stem, "slide")?;
            Some((num, name.to_owned()))
        })
        .collect();
    slide_entries.sort_by_key(|(num, _)| *num);

    if slide_entries.is_empty() {
        return Err(invalid("no slides found in PPTX container").into());
    }

    for (slide_num, slide_path) in slide_entries {
        let Some(slide_xml) = archive.get_text(&slide_path) else {
            continue;
        };
        let rels_path = format!("ppt/slides/_rels/slide{slide_num}.xml.rels");
        let rels = archive
            .get_text(&rels_path)
            .map(|xml| parse_ooxml_relationships(&xml))
            .unwrap_or_default();

        let lines = extract_pptx_slide_content(slide_num, &slide_xml, &rels);
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }

        let notes_path = format!("ppt/notesSlides/notesSlide{slide_num}.xml");
        if let Some(notes_xml) = archive.get_text(&notes_path) {
            let note_paras = extract_drawingml_paragraphs(&notes_xml, &BTreeMap::new(), false)
                .into_iter()
                .filter(|line| line.parse::<usize>().is_err())
                .collect::<Vec<_>>();
            if !note_paras.is_empty() {
                out.push_str(&format!("### [Slide {slide_num} Notes]\n"));
                for note in note_paras {
                    out.push_str(&note);
                    out.push('\n');
                }
            }
        }

        let comments_path = format!("ppt/comments/comment{slide_num}.xml");
        if let Some(comments_xml) = archive.get_text(&comments_path) {
            let comment_texts = extract_all_local_tag_texts(&comments_xml, &["text"]);
            if !comment_texts.is_empty() {
                out.push_str(&format!("### [Slide {slide_num} Comments]\n"));
                for comment in comment_texts {
                    out.push_str(&format!("- {comment}\n"));
                }
            }
        }
        out.push('\n');
    }

    let mut diagram_entries: Vec<&str> = archive
        .entry_names()
        .filter(|n| n.starts_with("ppt/diagrams/data") && n.ends_with(".xml"))
        .collect();
    diagram_entries.sort_unstable();
    let mut diagram_lines = Vec::new();
    for entry in diagram_entries {
        if let Some(xml) = archive.get_text(entry) {
            diagram_lines.extend(extract_all_local_tag_texts(&xml, &["t"]));
        }
    }
    if !diagram_lines.is_empty() {
        out.push_str("## [SmartArt Diagrams]\n");
        for line in diagram_lines {
            out.push_str(&format!("- {line}\n"));
        }
        out.push('\n');
    }

    let mut chart_entries: Vec<&str> = archive
        .entry_names()
        .filter(|n| n.starts_with("ppt/charts/chart") && n.ends_with(".xml"))
        .collect();
    chart_entries.sort_unstable();
    let mut chart_lines = Vec::new();
    for entry in chart_entries {
        if let Some(xml) = archive.get_text(entry) {
            chart_lines.extend(extract_all_local_tag_texts(&xml, &["v"]));
        }
    }
    if !chart_lines.is_empty() {
        out.push_str("## [Charts]\n");
        for line in chart_lines {
            out.push_str(&format!("- {line}\n"));
        }
    }

    Ok(out)
}

pub const MAX_TABLE_COLS: usize = 32;
pub const MAX_SHEET_ROWS: usize = 2000;

fn parse_excel_shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut cursor = 0usize;
    while let Some(si_pos) = find_word_tag(xml, cursor, "si") {
        let Some(gt) = xml[si_pos..].find('>') else {
            break;
        };
        let open_tag = &xml[si_pos + 1..si_pos + gt];
        if open_tag.ends_with('/') {
            strings.push(String::new());
            cursor = si_pos + gt + 1;
            continue;
        }
        let body_start = si_pos + gt + 1;
        let Some(end_rel) = xml[body_start..].find("</si>") else {
            break;
        };
        let si_body = &xml[body_start..body_start + end_rel];
        cursor = body_start + end_rel + 5;
        let mut combined = String::new();
        let mut t_pos = 0usize;
        while let Some(pos) = find_word_tag(si_body, t_pos, "t") {
            let Some(t_gt) = si_body[pos..].find('>') else {
                break;
            };
            let t_open = &si_body[pos + 1..pos + t_gt];
            if t_open.ends_with('/') {
                t_pos = pos + t_gt + 1;
                continue;
            }
            let inner_start = pos + t_gt + 1;
            let Some(close_t) = si_body[inner_start..].find("</t>") else {
                break;
            };
            combined.push_str(&decode_xml_entities(&si_body[inner_start..inner_start + close_t]));
            t_pos = inner_start + close_t + 4;
        }
        strings.push(combined.trim().to_owned());
    }
    strings
}

fn excel_col_index(cell_ref: &str) -> Option<usize> {
    let letters: String = cell_ref
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut col = 0usize;
    for ch in letters.bytes() {
        let digit = usize::from(ch.to_ascii_uppercase() - b'A' + 1);
        col = col.saturating_mul(26).saturating_add(digit);
    }
    col.checked_sub(1)
}

fn parse_workbook_sheets(
    workbook_xml: &str,
    rels: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut sheets = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = find_word_tag(workbook_xml, cursor, "sheet") {
        let Some(gt) = workbook_xml[pos..].find('>') else {
            break;
        };
        let tag = &workbook_xml[pos + 1..pos + gt];
        cursor = pos + gt + 1;
        let Some(name) = extract_xml_attr(tag, "name") else {
            continue;
        };
        let target = extract_xml_attr(tag, "id")
            .and_then(|rid| rels.get(&rid).cloned())
            .map(|t| {
                let cleaned = t.trim_start_matches('/');
                if cleaned.starts_with("xl/") {
                    cleaned.to_owned()
                } else {
                    format!("xl/{cleaned}")
                }
            });
        if let Some(target) = target {
            sheets.push((name, target));
        }
    }
    sheets
}

fn extract_xlsx_worksheet_rows(
    sheet_xml: &str,
    shared_strings: &[String],
) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row_cursor = 0usize;
    while let Some(row_pos) = find_word_tag(sheet_xml, row_cursor, "row") {
        if rows.len() >= MAX_SHEET_ROWS {
            break;
        }
        let Some(gt) = sheet_xml[row_pos..].find('>') else {
            break;
        };
        let open_tag = &sheet_xml[row_pos + 1..row_pos + gt];
        if open_tag.ends_with('/') {
            row_cursor = row_pos + gt + 1;
            continue;
        }
        let body_start = row_pos + gt + 1;
        let Some(end_rel) = sheet_xml[body_start..].find("</row>") else {
            break;
        };
        let row_body = &sheet_xml[body_start..body_start + end_rel];
        row_cursor = body_start + end_rel + 6;

        let mut cells: Vec<String> = Vec::new();
        let mut c_cursor = 0usize;
        while let Some(c_pos) = find_word_tag(row_body, c_cursor, "c") {
            let Some(c_gt) = row_body[c_pos..].find('>') else {
                break;
            };
            let c_open = &row_body[c_pos + 1..c_pos + c_gt];
            let cell_ref = extract_xml_attr(c_open, "r").unwrap_or_default();
            let cell_type = extract_xml_attr(c_open, "t").unwrap_or_default();
            let col_idx = excel_col_index(&cell_ref)
                .unwrap_or(cells.len())
                .min(MAX_TABLE_COLS - 1);
            if c_open.ends_with('/') {
                c_cursor = c_pos + c_gt + 1;
                continue;
            }
            let c_body_start = c_pos + c_gt + 1;
            let Some(c_end_rel) = row_body[c_body_start..].find("</c>") else {
                break;
            };
            let c_body = &row_body[c_body_start..c_body_start + c_end_rel];
            c_cursor = c_body_start + c_end_rel + 4;

            let formula = extract_xml_tag_text(c_body, "f");
            let raw_v = extract_xml_tag_text(c_body, "v");
            let value_str = match cell_type.as_str() {
                "s" => raw_v
                    .as_deref()
                    .and_then(|v| v.parse::<usize>().ok())
                    .and_then(|idx| shared_strings.get(idx).cloned())
                    .unwrap_or_default(),
                "inlineStr" => extract_all_local_tag_texts(c_body, &["t"]).join(""),
                "str" => raw_v.unwrap_or_default(),
                "e" => raw_v
                    .map(|err| format!("[Error: {err}]"))
                    .unwrap_or_default(),
                "b" => match raw_v.as_deref() {
                    Some("1") | Some("true") => "TRUE".into(),
                    Some("0") | Some("false") => "FALSE".into(),
                    _ => raw_v.unwrap_or_default(),
                },
                _ => raw_v.unwrap_or_default(),
            };

            let display = match (value_str.trim().is_empty(), formula) {
                (false, Some(f)) => format!("{} (={})", value_str.trim(), f.trim()),
                (true, Some(f)) => format!("={}", f.trim()),
                (false, None) => value_str.trim().to_owned(),
                (true, None) => String::new(),
            };
            if !display.is_empty() {
                if cells.len() <= col_idx {
                    cells.resize(col_idx + 1, String::new());
                }
                cells[col_idx] = display.replace('|', "\\|");
            }
        }
        if cells.iter().any(|c| !c.is_empty()) {
            rows.push(cells);
        }
    }
    rows
}

fn extract_xlsx_comments(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tag_name in ["comment", "threadedComment"] {
        let mut cursor = 0usize;
        let close_pat = format!("</{tag_name}>");
        while let Some(pos) = find_word_tag(xml, cursor, tag_name) {
            let Some(gt) = xml[pos..].find('>') else {
                break;
            };
            let open_tag = &xml[pos + 1..pos + gt];
            if open_tag.ends_with('/') {
                cursor = pos + gt + 1;
                continue;
            }
            let body_start = pos + gt + 1;
            let Some(end_rel) = xml[body_start..].find(&close_pat) else {
                break;
            };
            let body = &xml[body_start..body_start + end_rel];
            cursor = body_start + end_rel + close_pat.len();
            let cell_ref = extract_xml_attr(open_tag, "ref").unwrap_or_else(|| "Cell".into());
            let texts = extract_all_local_tag_texts(body, &["t", "text"]);
            let joined = texts.join(" ").trim().to_owned();
            if !joined.is_empty() {
                out.push(format!("- [{cell_ref}]: {joined}"));
            }
        }
    }
    out
}

pub fn extract_xlsx(archive: &ZipArchive) -> Result<String> {
    let meta = extract_archive_metadata(archive);
    let mut out = meta.format_header();
    let shared_strings = archive
        .get_text("xl/sharedStrings.xml")
        .map(|xml| parse_excel_shared_strings(&xml))
        .unwrap_or_default();
    let wb_rels = archive
        .get_text("xl/_rels/workbook.xml.rels")
        .map(|xml| parse_ooxml_relationships(&xml))
        .unwrap_or_default();

    let mut sheets = archive
        .get_text("xl/workbook.xml")
        .map(|xml| parse_workbook_sheets(&xml, &wb_rels))
        .unwrap_or_default();

    if sheets.is_empty() {
        let mut fallback: Vec<(usize, String)> = archive
            .entry_names()
            .filter_map(|name| {
                let file = name.strip_prefix("xl/worksheets/")?;
                let stem = file.strip_suffix(".xml")?;
                let num = parse_trailing_number(stem, "sheet")?;
                Some((num, name.to_owned()))
            })
            .collect();
        fallback.sort_by_key(|(num, _)| *num);
        sheets = fallback
            .into_iter()
            .map(|(num, path)| (format!("Sheet{num}"), path))
            .collect();
    }

    if sheets.is_empty() {
        return Err(invalid("no worksheets found in XLSX container").into());
    }

    for (sheet_name, sheet_path) in sheets {
        let Some(sheet_xml) = archive.get_text(&sheet_path) else {
            continue;
        };
        out.push_str(&format!("## [Sheet: {sheet_name}]\n"));
        let rows = extract_xlsx_worksheet_rows(&sheet_xml, &shared_strings);
        for line in format_markdown_table(&rows) {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }

    let mut comment_entries: Vec<&str> = archive
        .entry_names()
        .filter(|n| {
            (n.starts_with("xl/comments") || n.starts_with("xl/threadedComments/"))
                && n.ends_with(".xml")
        })
        .collect();
    comment_entries.sort_unstable();
    let mut all_comments = Vec::new();
    for entry in comment_entries {
        if let Some(xml) = archive.get_text(entry) {
            all_comments.extend(extract_xlsx_comments(&xml));
        }
    }
    if !all_comments.is_empty() {
        out.push_str("## [Sheet Comments]\n");
        for comment in all_comments {
            out.push_str(&comment);
            out.push('\n');
        }
    }

    Ok(out)
}

pub fn parse_delimited_records(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    let _ = chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else if ch == '\r' {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                field.push(' ');
            } else if ch == '\n' {
                field.push(' ');
            } else {
                field.push(ch);
            }
        } else if ch == '"' && field.is_empty() {
            in_quotes = true;
        } else if ch == delimiter {
            if current_row.len() < MAX_TABLE_COLS {
                current_row.push(field.trim().replace('|', "\\|"));
            }
            field.clear();
        } else if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                let _ = chars.next();
            }
            if current_row.len() < MAX_TABLE_COLS {
                current_row.push(field.trim().replace('|', "\\|"));
            }
            field.clear();
            if current_row.iter().any(|c| !c.is_empty()) {
                rows.push(std::mem::take(&mut current_row));
                if rows.len() >= MAX_SHEET_ROWS {
                    break;
                }
            } else {
                current_row.clear();
            }
        } else {
            field.push(ch);
        }
    }
    if !field.is_empty() || !current_row.is_empty() {
        if current_row.len() < MAX_TABLE_COLS {
            current_row.push(field.trim().replace('|', "\\|"));
        }
        if current_row.iter().any(|c| !c.is_empty()) && rows.len() < MAX_SHEET_ROWS {
            rows.push(current_row);
        }
    }
    rows
}

fn extract_odf_inline_text(xml: &str) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    let mut active_href: Option<String> = None;
    let mut link_text = String::new();

    while pos < xml.len() {
        if let Some(lt) = xml[pos..].find('<') {
            let text_part = &xml[pos..pos + lt];
            let decoded = decode_xml_entities(text_part);
            if active_href.is_some() {
                link_text.push_str(&decoded);
            } else {
                out.push_str(&decoded);
            }
            let tag_start = pos + lt;
            let Some(gt) = xml[tag_start..].find('>') else {
                break;
            };
            let raw_tag = xml[tag_start + 1..tag_start + gt].trim();
            pos = tag_start + gt + 1;
            let is_close = raw_tag.starts_with('/');
            let is_self_close = raw_tag.ends_with('/');
            let name = raw_tag
                .trim_start_matches('/')
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            match name {
                "text:s" if !is_close => {
                    let count = extract_xml_attr(raw_tag, "c")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(16);
                    let spaces = " ".repeat(count);
                    if active_href.is_some() {
                        link_text.push_str(&spaces);
                    } else {
                        out.push_str(&spaces);
                    }
                }
                "text:tab" if !is_close => {
                    out.push('\t');
                }
                "text:line-break" if !is_close => {
                    out.push(' ');
                }
                "text:a" if !is_close && !is_self_close => {
                    active_href = extract_xml_attr(raw_tag, "href");
                    link_text.clear();
                }
                "text:a" if is_close => {
                    if let Some(url) = active_href.take() {
                        if link_text.trim().is_empty() {
                            out.push_str(&url);
                        } else {
                            out.push_str(&format!("[{}]({})", link_text.trim(), url));
                        }
                        link_text.clear();
                    }
                }
                _ => {}
            }
        } else {
            let decoded = decode_xml_entities(&xml[pos..]);
            if active_href.is_some() {
                link_text.push_str(&decoded);
            } else {
                out.push_str(&decoded);
            }
            break;
        }
    }
    out.trim().to_owned()
}

fn extract_odf_table(tbl_xml: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut tr_cursor = 0usize;
    while let Some(tr_pos) = find_word_tag(tbl_xml, tr_cursor, "table:table-row") {
        if rows.len() >= MAX_SHEET_ROWS {
            break;
        }
        let Some(gt) = tbl_xml[tr_pos..].find('>') else {
            break;
        };
        let open_tag = &tbl_xml[tr_pos + 1..tr_pos + gt];
        if open_tag.ends_with('/') {
            tr_cursor = tr_pos + gt + 1;
            continue;
        }
        let body_start = tr_pos + gt + 1;
        let Some(end_tr) = tbl_xml[body_start..].find("</table:table-row>") else {
            break;
        };
        let tr_body = &tbl_xml[body_start..body_start + end_tr];
        tr_cursor = body_start + end_tr + 18;

        let mut cells = Vec::new();
        let mut tc_cursor = 0usize;
        while let Some(tc_pos) = find_word_tag(tr_body, tc_cursor, "table:table-cell") {
            let Some(tc_gt) = tr_body[tc_pos..].find('>') else {
                break;
            };
            let tc_open = &tr_body[tc_pos + 1..tc_pos + tc_gt];
            let repeat = extract_xml_attr(tc_open, "number-columns-repeated")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1);
            if tc_open.ends_with('/') {
                let bounded = repeat.min(4);
                for _ in 0..bounded {
                    if cells.len() < MAX_TABLE_COLS {
                        cells.push(String::new());
                    }
                }
                tc_cursor = tc_pos + tc_gt + 1;
                continue;
            }
            let tc_body_start = tc_pos + tc_gt + 1;
            let Some(end_tc) = tr_body[tc_body_start..].find("</table:table-cell>") else {
                break;
            };
            let tc_body = &tr_body[tc_body_start..tc_body_start + end_tc];
            tc_cursor = tc_body_start + end_tc + 19;
            let text = extract_odf_inline_text(tc_body).replace('|', "\\|");
            let bounded_repeat = if text.is_empty() {
                repeat.min(4)
            } else {
                repeat.min(MAX_TABLE_COLS)
            };
            for _ in 0..bounded_repeat {
                if cells.len() < MAX_TABLE_COLS {
                    cells.push(text.clone());
                }
            }
        }
        while cells.last().is_some_and(String::is_empty) {
            cells.pop();
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    rows
}

fn extract_odf_blocks(xml: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cursor = 0usize;
    while cursor < xml.len() {
        let next_h = find_word_tag(xml, cursor, "text:h");
        let next_p = find_word_tag(xml, cursor, "text:p");
        let next_tbl = find_word_tag(xml, cursor, "table:table");
        let mut candidates = [
            next_h.map(|pos| (pos, "text:h")),
            next_p.map(|pos| (pos, "text:p")),
            next_tbl.map(|pos| (pos, "table:table")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        candidates.sort_by_key(|(pos, _)| *pos);
        let Some((pos, kind)) = candidates.first().copied() else {
            break;
        };
        let Some(gt) = xml[pos..].find('>') else {
            break;
        };
        let open_tag = &xml[pos + 1..pos + gt];
        if open_tag.ends_with('/') {
            cursor = pos + gt + 1;
            continue;
        }
        let body_start = pos + gt + 1;
        let close_needle = format!("</{kind}>");
        let Some(end_rel) = xml[body_start..].find(&close_needle) else {
            break;
        };
        let body = &xml[body_start..body_start + end_rel];
        cursor = body_start + end_rel + close_needle.len();

        match kind {
            "text:h" => {
                let lvl = extract_xml_attr(open_tag, "outline-level")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 6);
                let text = extract_odf_inline_text(body);
                if !text.is_empty() {
                    lines.push(format!("{} {text}", "#".repeat(lvl)));
                }
            }
            "text:p" => {
                let text = extract_odf_inline_text(body);
                if !text.is_empty() {
                    lines.push(text);
                }
            }
            "table:table" => {
                let rows = extract_odf_table(&xml[pos..cursor]);
                lines.extend(format_markdown_table(&rows));
            }
            _ => {}
        }
    }
    lines
}

pub fn extract_odf(archive: &ZipArchive, ext: &str) -> Result<String> {
    let meta = extract_archive_metadata(archive);
    let mut out = meta.format_header();
    let content_xml = archive
        .get_text("content.xml")
        .ok_or_else(|| invalid("missing content.xml in ODF container"))?;

    match ext {
        "odp" => {
            let mut cursor = 0usize;
            let mut slide_num = 0usize;
            while let Some(page_pos) = find_word_tag(&content_xml, cursor, "draw:page") {
                let Some(gt) = content_xml[page_pos..].find('>') else {
                    break;
                };
                let open_tag = &content_xml[page_pos + 1..page_pos + gt];
                if open_tag.ends_with('/') {
                    cursor = page_pos + gt + 1;
                    continue;
                }
                let body_start = page_pos + gt + 1;
                let Some(end_page) = content_xml[body_start..].find("</draw:page>") else {
                    break;
                };
                let page_body = &content_xml[body_start..body_start + end_page];
                cursor = body_start + end_page + 12;
                slide_num += 1;

                let page_name = extract_xml_attr(open_tag, "name");
                let (main_part, notes_part) =
                    if let Some(notes_pos) = find_word_tag(page_body, 0, "presentation:notes") {
                        (&page_body[..notes_pos], Some(&page_body[notes_pos..]))
                    } else {
                        (page_body, None)
                    };
                if let Some(name) = page_name {
                    out.push_str(&format!("## [Slide {slide_num}] {name}\n"));
                } else {
                    out.push_str(&format!("## [Slide {slide_num}]\n"));
                }
                for line in extract_odf_blocks(main_part) {
                    out.push_str(&line);
                    out.push('\n');
                }
                if let Some(notes_xml) = notes_part {
                    let note_lines = extract_odf_blocks(notes_xml);
                    if !note_lines.is_empty() {
                        out.push_str(&format!("### [Slide {slide_num} Notes]\n"));
                        for line in note_lines {
                            out.push_str(&line);
                            out.push('\n');
                        }
                    }
                }
                out.push('\n');
            }
        }
        "ods" => {
            let mut cursor = 0usize;
            let mut sheet_idx = 0usize;
            while let Some(tbl_pos) = find_word_tag(&content_xml, cursor, "table:table") {
                let Some(gt) = content_xml[tbl_pos..].find('>') else {
                    break;
                };
                let open_tag = &content_xml[tbl_pos + 1..tbl_pos + gt];
                if open_tag.ends_with('/') {
                    cursor = tbl_pos + gt + 1;
                    continue;
                }
                let body_start = tbl_pos + gt + 1;
                let Some(end_tbl) = content_xml[body_start..].find("</table:table>") else {
                    break;
                };
                let tbl_slice = &content_xml[tbl_pos..body_start + end_tbl + 14];
                cursor = body_start + end_tbl + 14;
                sheet_idx += 1;
                let sheet_name = extract_xml_attr(open_tag, "name")
                    .unwrap_or_else(|| format!("Sheet{sheet_idx}"));
                out.push_str(&format!("## [Sheet: {sheet_name}]\n"));
                let rows = extract_odf_table(tbl_slice);
                for line in format_markdown_table(&rows) {
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
        }
        _ => {
            for line in extract_odf_blocks(&content_xml) {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

pub fn extract_svg_or_xhtml_text(xml: &str) -> String {
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut pos = 0usize;
    let mut skip_depth = 0usize;

    while pos < xml.len() {
        if let Some(lt) = xml[pos..].find('<') {
            if skip_depth == 0 {
                let decoded = decode_xml_entities(&xml[pos..pos + lt]);
                let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
                if !collapsed.is_empty() {
                    if !current_line.is_empty() && !current_line.ends_with(' ') {
                        current_line.push(' ');
                    }
                    current_line.push_str(&collapsed);
                }
            }
            let tag_start = pos + lt;
            if xml[tag_start..].starts_with("<!--")
                && let Some(end_c) = xml[tag_start + 4..].find("-->")
            {
                pos = tag_start + 4 + end_c + 3;
                continue;
            }
            let Some(gt) = xml[tag_start..].find('>') else {
                break;
            };
            let raw_tag = xml[tag_start + 1..tag_start + gt].trim();
            pos = tag_start + gt + 1;
            let is_close = raw_tag.starts_with('/');
            let is_self_close = raw_tag.ends_with('/');
            let name = raw_tag
                .trim_start_matches('/')
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            let local = name.rsplit_once(':').map_or(name, |(_, l)| l).to_ascii_lowercase();

            if matches!(local.as_str(), "style" | "script" | "defs") {
                if !is_close && !is_self_close {
                    skip_depth += 1;
                } else if is_close {
                    skip_depth = skip_depth.saturating_sub(1);
                }
                continue;
            }
            if skip_depth > 0 {
                continue;
            }
            match local.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !is_close => {
                    if !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_owned());
                    }
                    current_line.clear();
                    let lvl = local[1..].parse::<usize>().unwrap_or(1);
                    current_line.push_str(&"#".repeat(lvl));
                    current_line.push(' ');
                }
                "li" if !is_close => {
                    if !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_owned());
                    }
                    current_line = "- ".into();
                }
                "p" | "div" | "section" | "article" | "tr" | "title" | "desc" | "text" | "h1"
                | "h2" | "h3" | "h4" | "h5" | "h6" | "li"
                    if is_close =>
                {
                    if !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_owned());
                    }
                    current_line.clear();
                }
                "td" | "th" if is_close => {
                    current_line.push_str(" | ");
                }
                "br" => {
                    if !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_owned());
                    }
                    current_line.clear();
                }
                _ => {}
            }
        } else {
            if skip_depth == 0 {
                let decoded = decode_xml_entities(&xml[pos..]);
                let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
                if !collapsed.is_empty() {
                    if !current_line.is_empty() && !current_line.ends_with(' ') {
                        current_line.push(' ');
                    }
                    current_line.push_str(&collapsed);
                }
            }
            break;
        }
    }
    if !current_line.trim().is_empty() {
        lines.push(current_line.trim().to_owned());
    }
    lines.join("\n")
}

pub fn extract_epub(archive: &ZipArchive) -> Result<String> {
    let mut out = String::new();
    let opf_path = archive
        .get_text("META-INF/container.xml")
        .and_then(|xml| {
            let pos = find_word_tag(&xml, 0, "rootfile")?;
            let gt = xml[pos..].find('>')?;
            extract_xml_attr(&xml[pos + 1..pos + gt], "full-path")
        })
        .or_else(|| {
            archive
                .entry_names()
                .find(|n| n.ends_with(".opf"))
                .map(str::to_owned)
        })
        .ok_or_else(|| invalid("missing OPF package manifest in EPUB"))?;

    let opf_dir = Path::new(&opf_path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let opf_xml = archive
        .get_text(&opf_path)
        .ok_or_else(|| invalid("missing OPF file in EPUB archive"))?;

    let title = extract_xml_tag_text(&opf_xml, "title");
    let creator = extract_xml_tag_text(&opf_xml, "creator");
    if title.is_some() || creator.is_some() {
        out.push_str("# [Document Metadata]\n");
        if let Some(t) = title {
            out.push_str(&format!("Title: {t}\n"));
        }
        if let Some(c) = creator {
            out.push_str(&format!("Author: {c}\n"));
        }
        out.push('\n');
    }

    let mut manifest_items = BTreeMap::new();
    let mut cursor = 0usize;
    while let Some(pos) = find_word_tag(&opf_xml, cursor, "item") {
        let Some(gt) = opf_xml[pos..].find('>') else {
            break;
        };
        let tag = &opf_xml[pos + 1..pos + gt];
        cursor = pos + gt + 1;
        if let (Some(id), Some(href)) = (extract_xml_attr(tag, "id"), extract_xml_attr(tag, "href"))
        {
            let resolved = if opf_dir.is_empty() {
                href
            } else {
                format!("{opf_dir}/{href}")
            };
            manifest_items.insert(id, resolved);
        }
    }

    let mut chapter_paths = Vec::new();
    cursor = 0;
    while let Some(pos) = find_word_tag(&opf_xml, cursor, "itemref") {
        let Some(gt) = opf_xml[pos..].find('>') else {
            break;
        };
        let tag = &opf_xml[pos + 1..pos + gt];
        cursor = pos + gt + 1;
        if let Some(idref) = extract_xml_attr(tag, "idref")
            && let Some(path) = manifest_items.get(&idref)
        {
            chapter_paths.push(path.clone());
        }
    }

    if chapter_paths.is_empty() {
        chapter_paths = archive
            .entry_names()
            .filter(|n| n.ends_with(".xhtml") || n.ends_with(".html"))
            .map(str::to_owned)
            .collect();
    }

    for (idx, chapter_path) in chapter_paths.iter().enumerate() {
        if let Some(xhtml) = archive.get_text(chapter_path) {
            let chapter_title = extract_xml_tag_text(&xhtml, "title")
                .or_else(|| extract_xml_tag_text(&xhtml, "h1"))
                .unwrap_or_else(|| chapter_path.clone());
            out.push_str(&format!("## [Chapter {}] {chapter_title}\n", idx + 1));
            let body_text = extract_svg_or_xhtml_text(&xhtml);
            if !body_text.is_empty() {
                out.push_str(&body_text);
                out.push('\n');
            }
            out.push('\n');
        }
    }

    Ok(out)
}

pub fn zlib_compress_fixed(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x9c]; // CMF/FLG valid zlib header (0x789c % 31 == 0)
    out.extend_from_slice(&deflate_fixed(data));
    // Adler-32 checksum
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    let adler = (b << 16) | a;
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

pub fn decode_pdf_string_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        let hex: Vec<u8> = trimmed[1..trimmed.len() - 1]
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let mut raw = Vec::with_capacity(hex.len() / 2);
        for pair in hex.chunks(2) {
            let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = pair
                .get(1)
                .and_then(|&b| (b as char).to_digit(16))
                .unwrap_or(0) as u8;
            raw.push((hi << 4) | lo);
        }
        if raw.starts_with(&[0xFE, 0xFF])
            && let Ok(s) = decode_utf16(&raw[2..], false)
        {
            return s;
        }
        return String::from_utf8_lossy(&raw).into_owned();
    }
    let inner = if trimmed.starts_with('(') && trimmed.ends_with(')') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let bytes = inner.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => out_bytes.push(b'\n'),
                b'r' => out_bytes.push(b'\r'),
                b't' => out_bytes.push(b'\t'),
                b'b' => out_bytes.push(0x08),
                b'f' => out_bytes.push(0x0c),
                b'(' => out_bytes.push(b'('),
                b')' => out_bytes.push(b')'),
                b'\\' => out_bytes.push(b'\\'),
                b'\r' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        i += 1;
                    }
                }
                b'\n' => {}
                b'0'..=b'7' => {
                    let mut val = u32::from(bytes[i] - b'0');
                    for _ in 0..2 {
                        if i + 1 < bytes.len() && (b'0'..=b'7').contains(&bytes[i + 1]) {
                            i += 1;
                            val = (val << 3) + u32::from(bytes[i] - b'0');
                        } else {
                            break;
                        }
                    }
                    out_bytes.push((val & 0xff) as u8);
                }
                other => out_bytes.push(other),
            }
        } else {
            out_bytes.push(bytes[i]);
        }
        i += 1;
    }
    if out_bytes.starts_with(&[0xFE, 0xFF])
        && let Ok(s) = decode_utf16(&out_bytes[2..], false)
    {
        return s;
    }
    String::from_utf8_lossy(&out_bytes).into_owned()
}

fn parse_pdf_content_stream(stream_text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let bytes = stream_text.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'(' {
            let start = i;
            i += 1;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'(' {
                    depth += 1;
                } else if bytes[i] == b')' {
                    depth -= 1;
                }
                i += 1;
            }
            let decoded = decode_pdf_string_token(&stream_text[start..i]);
            current_line.push_str(&decoded);
        } else if bytes[i] == b'<' && bytes.get(i + 1) != Some(&b'<') {
            let start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            let decoded = decode_pdf_string_token(&stream_text[start..i]);
            current_line.push_str(&decoded);
        } else if bytes[i] == b'[' {
            i += 1;
            while i < bytes.len() && bytes[i] != b']' {
                if bytes[i] == b'(' {
                    let s_start = i;
                    i += 1;
                    let mut depth = 1usize;
                    while i < bytes.len() && depth > 0 {
                        if bytes[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if bytes[i] == b'(' {
                            depth += 1;
                        } else if bytes[i] == b')' {
                            depth -= 1;
                        }
                        i += 1;
                    }
                    current_line.push_str(&decode_pdf_string_token(&stream_text[s_start..i]));
                } else if bytes[i] == b'<' && bytes.get(i + 1) != Some(&b'<') {
                    let s_start = i;
                    while i < bytes.len() && bytes[i] != b'>' {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                    current_line.push_str(&decode_pdf_string_token(&stream_text[s_start..i]));
                } else if bytes[i] == b'-' || bytes[i].is_ascii_digit() {
                    let n_start = i;
                    i += 1;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_digit() || bytes[i] == b'.')
                    {
                        i += 1;
                    }
                    if let Ok(kern) = stream_text[n_start..i].parse::<f64>()
                        && kern < -120.0
                        && !current_line.ends_with(' ')
                    {
                        current_line.push(' ');
                    }
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1;
            }
        } else {
            let op_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b'(' | b')' | b'[' | b']' | b'<' | b'>' | b'/')
            {
                i += 1;
            }
            let op = &stream_text[op_start..i];
            if matches!(op, "T*" | "TD" | "Td" | "ET" | "'" | "\"") {
                let trimmed = current_line.trim();
                if !trimmed.is_empty() {
                    lines.push(trimmed.to_owned());
                }
                current_line.clear();
            } else if i == op_start {
                i += 1;
            }
        }
    }
    let trimmed = current_line.trim();
    if !trimmed.is_empty() {
        lines.push(trimmed.to_owned());
    }
    lines
}

fn extract_pdf_info_field(raw_latin1: &str, key: &str) -> Option<String> {
    let pos = raw_latin1.find(key)?;
    let rest = raw_latin1[pos + key.len()..].trim_start();
    if rest.starts_with('(') {
        let bytes = rest.as_bytes();
        let mut i = 1usize;
        let mut depth = 1usize;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'(' {
                depth += 1;
            } else if bytes[i] == b')' {
                depth -= 1;
            }
            i += 1;
        }
        let val = decode_pdf_string_token(&rest[..i]).trim().to_owned();
        (!val.is_empty()).then_some(val)
    } else if rest.starts_with('<') && !rest.starts_with("<<") {
        let end = rest.find('>')?;
        let val = decode_pdf_string_token(&rest[..=end]).trim().to_owned();
        (!val.is_empty()).then_some(val)
    } else {
        None
    }
}

pub fn extract_pdf(bytes: &[u8]) -> Result<String> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(invalid("invalid PDF header").into());
    }
    let raw_str = String::from_utf8_lossy(bytes);
    let meta = DocumentMetadata {
        title: extract_pdf_info_field(&raw_str, "/Title"),
        creator: extract_pdf_info_field(&raw_str, "/Author")
            .or_else(|| extract_pdf_info_field(&raw_str, "/Creator")),
        subject: extract_pdf_info_field(&raw_str, "/Subject"),
        keywords: extract_pdf_info_field(&raw_str, "/Keywords"),
        application: extract_pdf_info_field(&raw_str, "/Producer"),
        ..Default::default()
    };

    let mut out = meta.format_header();
    let mut page_num = 0usize;
    let mut search_pos = 0usize;

    while let Some(s_rel) = memchr::memmem::find(&bytes[search_pos..], b"stream") {
        let s_pos = search_pos + s_rel;
        let header_window_start = s_pos.saturating_sub(256);
        let dict_slice = String::from_utf8_lossy(&bytes[header_window_start..s_pos]);
        let mut data_start = s_pos + 6;
        if bytes.get(data_start) == Some(&b'\r') && bytes.get(data_start + 1) == Some(&b'\n') {
            data_start += 2;
        } else if bytes.get(data_start) == Some(&b'\n') || bytes.get(data_start) == Some(&b'\r') {
            data_start += 1;
        } else {
            search_pos = s_pos + 6;
            continue;
        }
        let Some(end_rel) = memchr::memmem::find(&bytes[data_start..], b"endstream") else {
            break;
        };
        let mut data_end = data_start + end_rel;
        while data_end > data_start
            && matches!(bytes[data_end - 1], b'\r' | b'\n' | b' ')
        {
            data_end -= 1;
        }
        let stream_bytes = &bytes[data_start..data_end];
        search_pos = data_start + end_rel + 9;

        let decoded_bytes = if dict_slice.contains("/FlateDecode") {
            inflate_zlib_bounded(stream_bytes, MAX_ENTRY_BYTES)
                .or_else(|_| inflate_raw_bounded(stream_bytes, MAX_ENTRY_BYTES))
                .unwrap_or_default()
        } else {
            stream_bytes.to_vec()
        };
        if decoded_bytes.is_empty() {
            continue;
        }
        let content_str = String::from_utf8_lossy(&decoded_bytes);
        if dict_slice.contains("/ObjStm")
            && out.is_empty()
            && let Some(obj_title) = extract_pdf_info_field(&content_str, "/Title")
        {
            out.push_str(&format!("# [Document Metadata]\nTitle: {obj_title}\n\n"));
        }
        if content_str.contains("BT")
            || content_str.contains("Tj")
            || content_str.contains("TJ")
        {
            let page_lines = parse_pdf_content_stream(&content_str);
            if !page_lines.is_empty() {
                page_num += 1;
                out.push_str(&format!("## [Page {page_num}]\n"));
                for line in page_lines {
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }

    if out.trim().is_empty() {
        return Err(invalid("PDF contains no extractable text streams").into());
    }
    Ok(out)
}

pub fn extract_rtf(bytes: &[u8]) -> Result<String> {
    let raw = String::from_utf8_lossy(bytes);
    if !raw.trim_start().starts_with("{\\rtf") {
        return Err(invalid("invalid RTF header").into());
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut skip_depth: Option<usize> = None;
    let mut uc_skip = 1usize;
    let mut out = String::new();

    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '{' => {
                depth += 1;
                i += 1;
                if skip_depth.is_none() {
                    let rest: String = chars[i..chars.len().min(i + 24)].iter().collect();
                    let trimmed = rest.trim_start();
                    if trimmed.starts_with("\\*")
                        || trimmed.starts_with("\\fonttbl")
                        || trimmed.starts_with("\\colortbl")
                        || trimmed.starts_with("\\stylesheet")
                        || trimmed.starts_with("\\pict")
                        || trimmed.starts_with("\\info")
                    {
                        skip_depth = Some(depth);
                    }
                }
            }
            '}' => {
                if skip_depth == Some(depth) {
                    skip_depth = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                if skip_depth.is_some() {
                    if chars[i].is_ascii_alphabetic() {
                        while i < chars.len() && chars[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        if i < chars.len() && (chars[i] == '-' || chars[i].is_ascii_digit()) {
                            i += 1;
                            while i < chars.len() && chars[i].is_ascii_digit() {
                                i += 1;
                            }
                        }
                        if i < chars.len() && chars[i] == ' ' {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                    continue;
                }
                match chars[i] {
                    '\\' | '{' | '}' => {
                        out.push(chars[i]);
                        i += 1;
                    }
                    '~' => {
                        out.push(' ');
                        i += 1;
                    }
                    '\'' => {
                        i += 1;
                        if i + 1 < chars.len() {
                            let hex_s: String = chars[i..i + 2].iter().collect();
                            if let Ok(b) = u8::from_str_radix(&hex_s, 16) {
                                out.push(b as char);
                            }
                            i += 2;
                        }
                    }
                    '\n' | '\r' => {
                        out.push('\n');
                        i += 1;
                    }
                    _ if chars[i].is_ascii_alphabetic() => {
                        let w_start = i;
                        while i < chars.len() && chars[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        let word: String = chars[w_start..i].iter().collect();
                        let n_start = i;
                        if i < chars.len() && (chars[i] == '-' || chars[i].is_ascii_digit()) {
                            i += 1;
                            while i < chars.len() && chars[i].is_ascii_digit() {
                                i += 1;
                            }
                        }
                        let num_str: String = chars[n_start..i].iter().collect();
                        if i < chars.len() && chars[i] == ' ' {
                            i += 1;
                        }
                        match word.as_str() {
                            "par" | "line" | "row" => out.push('\n'),
                            "tab" => out.push('\t'),
                            "cell" => out.push_str(" | "),
                            "uc" => {
                                if let Ok(n) = num_str.parse::<usize>() {
                                    uc_skip = n;
                                }
                            }
                            "u" => {
                                if let Ok(code) = num_str.parse::<i32>() {
                                    let code_u16 = if code < 0 {
                                        (code + 65536) as u16
                                    } else {
                                        code as u16
                                    };
                                    if let Some(c) = char::from_u32(u32::from(code_u16)) {
                                        out.push(c);
                                    }
                                    for _ in 0..uc_skip {
                                        if i < chars.len() {
                                            if chars[i] == '\\'
                                                && i + 3 < chars.len()
                                                && chars[i + 1] == '\''
                                            {
                                                i += 4;
                                            } else {
                                                i += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            '\r' | '\n' => {
                i += 1;
            }
            other => {
                if skip_depth.is_none() {
                    out.push(other);
                }
                i += 1;
            }
        }
    }
    let cleaned = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("{cleaned}\n"))
}

fn json_lines(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub fn extract_ipynb(bytes: &[u8]) -> Result<String> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| invalid(format!("invalid ipynb JSON: {e}")))?;
    let lang = root
        .pointer("/metadata/kernelspec/language")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            root.pointer("/metadata/language_info/name")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("python");
    let cells = root
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid("ipynb missing cells array"))?;

    let mut out = String::new();
    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("code");
        let source = cell
            .get("source")
            .map(json_lines)
            .unwrap_or_default();
        if source.trim().is_empty() {
            continue;
        }
        let cell_num = idx + 1;
        if cell_type == "markdown" {
            out.push_str(&format!("## [Notebook Cell {cell_num}: markdown]\n"));
            out.push_str(source.trim_end());
            out.push_str("\n\n");
        } else if cell_type == "raw" {
            out.push_str(&format!("## [Notebook Cell {cell_num}: raw]\n"));
            out.push_str(source.trim_end());
            out.push_str("\n\n");
        } else {
            if let Some(exec_count) = cell.get("execution_count").and_then(serde_json::Value::as_u64) {
                out.push_str(&format!(
                    "## [Notebook Cell {cell_num}: code ({lang}) In[{exec_count}]]\n"
                ));
            } else {
                out.push_str(&format!("## [Notebook Cell {cell_num}: code ({lang})]\n"));
            }
            out.push_str(source.trim_end());
            out.push('\n');
            if let Some(outputs) = cell.get("outputs").and_then(serde_json::Value::as_array) {
                for output in outputs {
                    let text_out = output
                        .get("text")
                        .map(json_lines)
                        .or_else(|| output.pointer("/data/text/plain").map(json_lines))
                        .or_else(|| output.pointer("/data/text/markdown").map(json_lines))
                        .or_else(|| output.pointer("/data/text/latex").map(json_lines))
                        .unwrap_or_default();
                    if !text_out.trim().is_empty() {
                        out.push_str("### [Cell Output]\n");
                        for line in text_out.lines().take(25) {
                            out.push_str(line);
                            out.push('\n');
                        }
                    } else if output.pointer("/data/image/png").is_some() {
                        out.push_str("### [Cell Visual Output: image/png]\n");
                    } else if output.pointer("/data/image/svg+xml").is_some() {
                        out.push_str("### [Cell Visual Output: image/svg+xml]\n");
                    }
                    if let (Some(ename), Some(evalue)) = (
                        output.get("ename").and_then(serde_json::Value::as_str),
                        output.get("evalue").and_then(serde_json::Value::as_str),
                    ) {
                        out.push_str(&format!("### [Cell Error] {ename}: {evalue}\n"));
                        if let Some(tb) = output.get("traceback").and_then(serde_json::Value::as_array) {
                            for tb_line in tb.iter().filter_map(serde_json::Value::as_str).take(12) {
                                let clean = strip_ansi_escapes(tb_line);
                                for sub in clean.lines() {
                                    let trimmed_sub = sub.trim_end();
                                    if !trimmed_sub.is_empty() {
                                        out.push_str(trimmed_sub);
                                        out.push('\n');
                                    }
                                }
                            }
                        }
                    }
                }
            }
            out.push('\n');
        }
    }
    Ok(out)
}

fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn extract_jsonl(text: &str) -> String {
    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut key_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(obj) = val.as_object() {
                for k in obj.keys() {
                    if key_set.len() < 16 {
                        key_set.insert(k.clone());
                    }
                }
            }
            records.push(val);
        }
    }
    if records.is_empty() {
        return text.to_owned();
    }
    let mut out = String::from("# [Document Metadata]\n");
    out.push_str(&format!("Count: {} records\n", records.len()));
    if !key_set.is_empty() {
        let keys: Vec<&str> = key_set.iter().map(String::as_str).collect();
        out.push_str(&format!("Keys: {}\n", keys.join(", ")));
    }
    out.push('\n');
    for (idx, val) in records.iter().enumerate() {
        let rec_num = idx + 1;
        out.push_str(&format!("## [Record {rec_num}]\n"));
        if let Some(obj) = val.as_object() {
            for (k, v) in obj {
                match v {
                    serde_json::Value::String(s) => out.push_str(&format!("{k}: {s}\n")),
                    _ => out.push_str(&format!("{k}: {v}\n")),
                }
            }
        } else {
            out.push_str(&format!("{val}\n"));
        }
        out.push('\n');
    }
    out
}

pub fn extract_vtt_or_srt(text: &str) -> String {
    let normalized = normalize_line_endings(text);
    let mut out = String::new();
    let mut cue_idx = 0usize;
    let blocks: Vec<&str> = normalized.split("\n\n").collect();
    for block in blocks {
        let trimmed = block.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("WEBVTT")
            || trimmed.starts_with("STYLE")
            || trimmed.starts_with("REGION")
        {
            continue;
        }
        if let Some(note_rest) = trimmed.strip_prefix("NOTE") {
            let note_body = note_rest.trim();
            if !note_body.is_empty() {
                out.push_str(&format!("### [Note] {note_body}\n\n"));
            }
            continue;
        }
        let lines: Vec<&str> = trimmed.lines().collect();
        let Some(arrow_pos) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        cue_idx += 1;
        let cue_id = if arrow_pos > 0 && !lines[0].trim().is_empty() {
            lines[0].trim().to_owned()
        } else {
            cue_idx.to_string()
        };
        let timing = lines[arrow_pos].trim();
        out.push_str(&format!("### [Cue {cue_id}: {timing}]\n"));
        for &payload_line in &lines[arrow_pos + 1..] {
            let clean = strip_vtt_tags(payload_line.trim());
            if !clean.is_empty() {
                out.push_str(&clean);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

fn strip_vtt_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after_lt = &rest[lt + 1..];
        if let Some(gt) = after_lt.find('>') {
            let tag = after_lt[..gt].trim();
            if let Some(speaker) = tag.strip_prefix("v ") {
                let speaker = speaker.trim();
                if !speaker.is_empty() {
                    out.push_str(&format!("[{speaker}]: "));
                }
            }
            rest = &after_lt[gt + 1..];
        } else {
            out.push('<');
            rest = after_lt;
        }
    }
    out.push_str(rest);
    out
}

pub fn extract_xliff_or_tmx(xml: &str) -> String {
    let mut out = String::new();
    let mut cursor = 0usize;
    while let Some(pos) = [
        find_word_tag(xml, cursor, "trans-unit"),
        find_word_tag(xml, cursor, "tu"),
        find_word_tag(xml, cursor, "unit"),
    ]
    .into_iter()
    .flatten()
    .min()
    {
        let Some(gt) = xml[pos..].find('>') else {
            break;
        };
        let open_tag = &xml[pos + 1..pos + gt];
        let tag_name = open_tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("trans-unit");
        if open_tag.ends_with('/') {
            cursor = pos + gt + 1;
            continue;
        }
        let body_start = pos + gt + 1;
        let close_needle = format!("</{tag_name}>");
        let Some(end_rel) = xml[body_start..].find(&close_needle) else {
            break;
        };
        let body = &xml[body_start..body_start + end_rel];
        cursor = body_start + end_rel + close_needle.len();

        let id = extract_xml_attr(open_tag, "id")
            .or_else(|| extract_xml_attr(open_tag, "tuid"))
            .or_else(|| extract_xml_attr(open_tag, "resname"))
            .unwrap_or_else(|| "unit".into());
        let source = extract_xml_tag_text(body, "source");
        let target = extract_xml_tag_text(body, "target");
        let notes = extract_all_local_tag_texts(body, &["note"]);
        let segs = extract_all_local_tag_texts(body, &["seg"]);
        out.push_str(&format!("### [Translation Unit: {id}]\n"));
        if let Some(src) = source {
            out.push_str(&format!("Source: {src}\n"));
        }
        if let Some(tgt) = target {
            out.push_str(&format!("Target: {tgt}\n"));
        }
        for seg in segs {
            out.push_str(&format!("Segment: {seg}\n"));
        }
        for note in notes {
            out.push_str(&format!("Note: {note}\n"));
        }
        out.push('\n');
    }
    out
}

pub fn is_container_or_rich_document(path: &Path) -> bool {
    let Some(ext) = extension_lower(path) else {
        return false;
    };
    matches!(
        ext.as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "pptx"
            | "pptm"
            | "potx"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "odt"
            | "odp"
            | "ods"
            | "epub"
            | "pdf"
            | "rtf"
            | "ipynb"
            | "xliff"
            | "xlf"
            | "tmx"
    )
}

pub fn is_binary_container_document(path: &Path) -> bool {
    let Some(ext) = extension_lower(path) else {
        return false;
    };
    matches!(
        ext.as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "pptx"
            | "pptm"
            | "potx"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "odt"
            | "odp"
            | "ods"
            | "epub"
            | "pdf"
            | "rtf"
    )
}

pub fn extract_document_bytes(path: &Path, bytes: &[u8]) -> Result<Option<String>> {
    let ext = extension_lower(path).unwrap_or_default();
    match ext.as_str() {
        "docx" | "docm" | "dotx" => {
            let archive = ZipArchive::parse(bytes)?;
            Ok(Some(extract_docx(&archive)?))
        }
        "pptx" | "pptm" | "potx" => {
            let archive = ZipArchive::parse(bytes)?;
            Ok(Some(extract_pptx(&archive)?))
        }
        "xlsx" | "xlsm" | "xltx" => {
            let archive = ZipArchive::parse(bytes)?;
            Ok(Some(extract_xlsx(&archive)?))
        }
        "odt" | "odp" | "ods" => {
            let archive = ZipArchive::parse(bytes)?;
            Ok(Some(extract_odf(&archive, &ext)?))
        }
        "epub" => {
            let archive = ZipArchive::parse(bytes)?;
            Ok(Some(extract_epub(&archive)?))
        }
        "pdf" => Ok(Some(extract_pdf(bytes)?)),
        "rtf" => Ok(Some(extract_rtf(bytes)?)),
        "ipynb" => Ok(Some(extract_ipynb(bytes)?)),
        "xliff" | "xlf" | "tmx" => {
            let text = decode_text_bytes(bytes)?;
            let extracted = extract_xliff_or_tmx(&text);
            if extracted.trim().is_empty() {
                Ok(Some(text))
            } else {
                Ok(Some(extracted))
            }
        }
        _ => {
            let is_doc = path.to_str().is_some_and(is_document_path);
            if is_doc
                && bytes.len() >= 4
                && (bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]))
            {
                return Ok(Some(decode_text_bytes(bytes)?));
            }
            Ok(None)
        }
    }
}

pub fn is_markdown_path(path: &str) -> bool {
    let p = Path::new(path);
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        return matches!(
            ext.to_ascii_lowercase().as_str(),
            "md" | "mdx" | "markdown" | "mdown" | "mkd" | "qmd" | "rmd"
        );
    }
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "README"
                    | "CHANGELOG"
                    | "LICENSE"
                    | "CONTRIBUTING"
                    | "AUTHORS"
                    | "NOTICE"
                    | "SECURITY"
                    | "CODEOWNERS"
            )
        })
}

pub fn is_agent_instruction_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".instructions.md")
        || lower.ends_with(".prompt.md")
        || lower.ends_with(".agent.md")
    {
        return true;
    }
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "AGENTS.md"
                    | "CLAUDE.md"
                    | "GEMINI.md"
                    | "SKILL.md"
                    | "copilot-instructions.md"
            )
        })
}

pub fn is_document_path(path: &str) -> bool {
    if is_markdown_path(path) {
        return true;
    }
    let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "pptx"
            | "pptm"
            | "potx"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "odt"
            | "odp"
            | "ods"
            | "epub"
            | "pdf"
            | "rtf"
            | "ipynb"
            | "rst"
            | "adoc"
            | "asciidoc"
            | "org"
            | "typ"
            | "tex"
            | "sty"
            | "cls"
            | "bib"
            | "csv"
            | "tsv"
            | "jsonl"
            | "ndjson"
            | "jsonlines"
            | "svg"
            | "xliff"
            | "xlf"
            | "tmx"
            | "po"
            | "pot"
            | "vtt"
            | "srt"
            | "eml"
            | "mermaid"
            | "mmd"
            | "puml"
            | "plantuml"
    )
}

fn is_markdown_gfm_alert(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('>') else {
        return false;
    };
    let rest = rest.trim_start();
    let upper = rest.to_ascii_uppercase();
    [
        "[!NOTE]",
        "[!TIP]",
        "[!IMPORTANT]",
        "[!WARNING]",
        "[!CAUTION]",
    ]
    .iter()
    .any(|tag| upper.starts_with(tag))
}

fn is_markdown_admonition_open(trimmed: &str) -> bool {
    if let Some(rest) = trimmed.strip_prefix(":::") {
        let rest = rest.trim();
        return !rest.is_empty()
            && !rest.starts_with(':')
            && rest
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic());
    }
    if let Some(rest) = trimmed.strip_prefix("!!! ") {
        return rest
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic());
    }
    false
}

fn is_markdown_footnote_definition(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("[^") else {
        return false;
    };
    let Some((id, after)) = rest.split_once("]:") else {
        return false;
    };
    !id.is_empty()
        && !id.contains([' ', '\t', ']'])
        && (after.is_empty() || after.starts_with([' ', '\t']))
}

fn is_markdown_link_ref_definition(trimmed: &str) -> bool {
    if !trimmed.starts_with('[') || trimmed.starts_with("[^") {
        return false;
    }
    let Some((label, after)) = trimmed[1..].split_once("]:") else {
        return false;
    };
    let label_trim = label.trim();
    if label_trim.is_empty()
        || matches!(label_trim, "x" | "X")
        || label.contains(['[', ']'])
    {
        return false;
    }
    let target = after.trim_start();
    target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with('/')
        || target.starts_with("./")
        || target.starts_with("../")
        || target.starts_with('#')
        || target.starts_with('<')
}

fn is_markdown_term_definition(trimmed: &str) -> bool {
    if let Some(rest) = trimmed.strip_prefix(": ") {
        return !rest.trim().is_empty();
    }
    let candidate = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    if let Some(rest) = candidate.strip_prefix("**")
        && let Some((term, tail)) = rest.split_once("**")
        && !term.is_empty()
        && term.len() <= 64
    {
        let tail = tail.trim_start();
        return tail.starts_with(':') || tail.starts_with('—') || tail.starts_with("- ");
    }
    if let Some(rest) = candidate.strip_prefix('`')
        && !rest.starts_with('`')
        && let Some((term, tail)) = rest.split_once('`')
        && !term.is_empty()
        && term.len() <= 64
    {
        let tail = tail.trim_start();
        return tail.starts_with(':') || tail.starts_with('—');
    }
    false
}

pub fn document_definition_priority(line: &str) -> u8 {
    let raw = line.strip_prefix('\u{feff}').unwrap_or(line);
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    let has_leading_tab = raw.starts_with('\t');
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        return 0;
    }
    if is_extracted_unit_boundary(trimmed) {
        return 4;
    }
    // CommonMark ATX headings allow 0..=3 leading spaces (4+ is an indented code block)
    if !has_leading_tab && leading_spaces <= 3 && trimmed.starts_with('#') {
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes)
            && matches!(trimmed.as_bytes().get(hashes), Some(b' ' | b'\t'))
        {
            return if hashes <= 2 { 4 } else { 3 };
        }
        if trimmed.starts_with("#+TITLE:")
            || trimmed.starts_with("#+title:")
            || trimmed.starts_with("#+NAME:")
        {
            return 4;
        }
        if trimmed.starts_with("#let ")
            || trimmed.starts_with("#show ")
            || trimmed.starts_with("#figure(")
            || trimmed.starts_with("#table(")
            || trimmed.starts_with("#outline(")
        {
            return 3;
        }
    }
    // Typst / AsciiDoc headings (= Title, == Section)
    if !has_leading_tab && leading_spaces <= 3 && trimmed.starts_with('=') {
        let eqs = trimmed.bytes().take_while(|&b| b == b'=').count();
        if (1..=6).contains(&eqs) && trimmed.as_bytes().get(eqs) == Some(&b' ') {
            return if eqs <= 2 { 4 } else { 3 };
        }
    }
    // Org-mode headings (* Heading, ** Subheading)
    if !has_leading_tab && leading_spaces == 0 && trimmed.starts_with('*') {
        let stars = trimmed.bytes().take_while(|&b| b == b'*').count();
        if (1..=6).contains(&stars)
            && trimmed.as_bytes().get(stars) == Some(&b' ')
            && !trimmed.ends_with('*')
        {
            return if stars <= 2 { 4 } else { 3 };
        }
    }
    // AsciiDoc anchor [[id]]
    if trimmed.starts_with("[[") && trimmed.ends_with("]]") && trimmed.len() > 4 {
        return 3;
    }
    // reStructuredText anchors and directives (.. _anchor:, .. function::)
    if let Some(rest) = trimmed.strip_prefix(".. ")
        && ((rest.starts_with('_') && rest.ends_with(':')) || rest.contains("::"))
    {
        return 3;
    }
    // LaTeX sectioning & macro definitions
    if trimmed.starts_with('\\') {
        for prefix in [
            "\\part{",
            "\\part*{",
            "\\chapter{",
            "\\chapter*{",
            "\\section{",
            "\\section*{",
        ] {
            if trimmed.starts_with(prefix) {
                return 4;
            }
        }
        for prefix in [
            "\\subsection{",
            "\\subsection*{",
            "\\subsubsection{",
            "\\subsubsection*{",
            "\\paragraph{",
            "\\paragraph*{",
            "\\subparagraph{",
            "\\subparagraph*{",
            "\\newcommand{",
            "\\renewcommand{",
            "\\DeclareRobustCommand{",
            "\\newtheorem{",
            "\\caption{",
            "\\label{",
            "\\def\\",
            "\\begin{theorem}",
            "\\begin{lemma}",
            "\\begin{proposition}",
            "\\begin{corollary}",
            "\\begin{definition}",
            "\\begin{algorithm}",
        ] {
            if trimmed.starts_with(prefix) {
                return 3;
            }
        }
    }
    // BibTeX entries
    if trimmed.starts_with('@') {
        let lower = trimmed.to_ascii_lowercase();
        for entry in [
            "@article{",
            "@book{",
            "@inproceedings{",
            "@conference{",
            "@incollection{",
            "@misc{",
            "@phdthesis{",
            "@mastersthesis{",
            "@techreport{",
            "@manual{",
            "@unpublished{",
            "@online{",
            "@software{",
            "@startuml",
            "@startmindmap",
            "@startwbs",
        ] {
            if lower.starts_with(entry) {
                return 4;
            }
        }
    }
    // Mermaid diagram headers
    if !has_leading_tab && leading_spaces <= 2 {
        for diagram in [
            "flowchart ",
            "graph ",
            "sequenceDiagram",
            "classDiagram",
            "stateDiagram",
            "erDiagram",
            "gantt",
            "mindmap",
            "timeline",
            "gitGraph",
            "C4Context",
        ] {
            if trimmed.starts_with(diagram) {
                return 4;
            }
        }
    }
    // GFM Alerts (> [!NOTE], > [!IMPORTANT], etc.) and ::: Admonitions
    if !has_leading_tab && leading_spaces <= 3 {
        if is_markdown_gfm_alert(trimmed) || is_markdown_admonition_open(trimmed) {
            return 3;
        }
        if is_markdown_footnote_definition(trimmed) {
            return 3;
        }
        if is_markdown_link_ref_definition(trimmed) || is_markdown_term_definition(trimmed) {
            return 2;
        }
    }
    // HTML headings, summary headers, and anchors in Markdown
    if trimmed.starts_with('<') {
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("<h1") || lower.starts_with("<h2") {
            return 4;
        }
        if lower.starts_with("<h3")
            || lower.starts_with("<h4")
            || lower.starts_with("<h5")
            || lower.starts_with("<h6")
            || lower.starts_with("<summary")
            || lower.starts_with("<a id=")
            || lower.starts_with("<a name=")
        {
            return 3;
        }
    }
    // Frontmatter / Document metadata primary & Copilot instruction keys
    if !has_leading_tab && leading_spaces <= 2 {
        for key in ["Title: ", "title: ", "name: ", "id: ", "slug: ", "applyTo: "] {
            if trimmed.starts_with(key) && trimmed.len() > key.len() {
                return 4;
            }
        }
        for key in [
            "Subject: ",
            "description: ",
            "sidebar_label: ",
            "tools: ",
            "model: ",
            "agent: ",
            "handoffs: ",
            "argument-hint: ",
            "summary: ",
            "permalink: ",
            "aliases: ",
        ] {
            if trimmed.starts_with(key) && trimmed.len() > key.len() {
                return 3;
            }
        }
    }
    0
}

pub fn is_document_definition_line(line: &str) -> bool {
    document_definition_priority(line) > 0
}

fn is_pipe_table_separator(trimmed: &str) -> bool {
    trimmed.contains('|')
        && trimmed.bytes().filter(|&b| b == b'-').count() >= 3
        && trimmed
            .bytes()
            .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t'))
}

pub fn is_document_boilerplate_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Markdown table separator row: | --- | --- | or --- | ---
    if is_pipe_table_separator(trimmed) {
        return true;
    }
    // Horizontal rules, frontmatter delimiters, Setext underlines, HTML rules/breaks
    if matches!(
        trimmed,
        "---" | "+++" | "***" | "___" | ":::" | "```" | "````" | "~~~" | "<hr>" | "<hr/>" | "<hr />" | "<br>" | "<br/>" | "<br />"
    ) {
        return true;
    }
    if trimmed.len() >= 3
        && (trimmed.bytes().all(|b| b == b'-')
            || trimmed.bytes().all(|b| b == b'=')
            || trimmed.bytes().all(|b| b == b'*')
            || trimmed.bytes().all(|b| b == b'_'))
    {
        return true;
    }
    // HTML comments (e.g. <!-- markdownlint-disable -->, <!-- prettier-ignore -->)
    if trimmed.starts_with("<!--") && trimmed.ends_with("-->") {
        return true;
    }
    // Shield / CI badge-only lines
    if (trimmed.starts_with("[![") || trimmed.starts_with("!["))
        && (trimmed.contains("shields.io")
            || trimmed.contains("badge")
            || (trimmed.contains("github.com/") && trimmed.contains("/actions/workflows/")))
    {
        return true;
    }
    // Pure Table-of-Contents internal anchor bullet lines: - [Section](#section)
    let list_body = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .map(str::trim_start)
        .unwrap_or("");
    if list_body.starts_with('[') && list_body.ends_with(')') && list_body.contains("](#") {
        return true;
    }
    // MDX / document import lines
    if crate::query::is_import_line(trimmed) {
        return true;
    }
    trimmed.starts_with("\\usepackage")
        || trimmed.starts_with("\\documentclass")
        || trimmed.starts_with("#import ")
        || trimmed.starts_with("include::")
}

pub fn matches_extension_case_insensitive(candidate: &str, suffix: &str) -> bool {
    if candidate.len() < suffix.len() {
        return false;
    }
    let tail = &candidate.as_bytes()[candidate.len() - suffix.len()..];
    tail.eq_ignore_ascii_case(suffix.as_bytes()) && is_document_path(candidate)
}

fn heading_level_of_line(line: &str) -> Option<usize> {
    let raw = line.strip_prefix('\u{feff}').unwrap_or(line);
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    if raw.starts_with('\t') || leading_spaces > 3 {
        return None;
    }
    let trimmed = &raw[leading_spaces..];
    if trimmed.starts_with('#') {
        let n = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&n) && matches!(trimmed.as_bytes().get(n), Some(b' ' | b'\t')) {
            return Some(n);
        }
    }
    if trimmed.starts_with('=') {
        let n = trimmed.bytes().take_while(|&b| b == b'=').count();
        if (1..=6).contains(&n) && trimmed.as_bytes().get(n) == Some(&b' ') {
            return Some(n);
        }
    }
    if leading_spaces == 0 && trimmed.starts_with('*') {
        let n = trimmed.bytes().take_while(|&b| b == b'*').count();
        if (1..=6).contains(&n)
            && trimmed.as_bytes().get(n) == Some(&b' ')
            && !trimmed.ends_with('*')
        {
            return Some(n);
        }
    }
    if trimmed.starts_with("\\part") {
        return Some(1);
    }
    if trimmed.starts_with("\\chapter") || trimmed.starts_with("\\section") {
        return Some(2);
    }
    if trimmed.starts_with("\\subsection") {
        return Some(3);
    }
    if trimmed.starts_with("\\subsubsection") {
        return Some(4);
    }
    None
}

fn is_extracted_unit_boundary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("## [Slide ")
        || trimmed.starts_with("## [Sheet: ")
        || trimmed.starts_with("## [Page ")
        || trimmed.starts_with("## [Notebook Cell ")
        || trimmed.starts_with("## [Chapter ")
        || trimmed.starts_with("## [Record ")
        || trimmed.starts_with("# [Document Metadata]")
        || trimmed.starts_with("## [Footnotes]")
        || trimmed.starts_with("## [Endnotes]")
        || trimmed.starts_with("## [Comments]")
        || trimmed.starts_with("## [SmartArt Diagrams]")
        || trimmed.starts_with("## [Charts]")
        || trimmed.starts_with("## [Sheet Comments]")
        || trimmed.starts_with("### [Translation Unit: ")
        || trimmed.starts_with("### [Cue ")
}

fn parse_fence_marker(line: &str) -> Option<(u8, usize, bool)> {
    let raw = line.strip_prefix('\u{feff}').unwrap_or(line);
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    if raw.starts_with('\t') || leading_spaces > 3 {
        return None;
    }
    let trimmed = &raw[leading_spaces..];
    if trimmed.starts_with("@startuml") || trimmed.starts_with("@enduml") {
        return Some((b'@', 9, trimmed.starts_with("@enduml")));
    }
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let run_len = trimmed.bytes().take_while(|&b| b == first).count();
    if run_len < 3 {
        return None;
    }
    let rest = trimmed[run_len..].trim();
    if first == b'`' && rest.contains('`') {
        return None;
    }
    Some((first, run_len, rest.is_empty()))
}

struct MarkdownBlockMasks {
    in_fence: Vec<bool>,
    fence_bounds: Vec<(usize, usize)>,
    math_bounds: Vec<(usize, usize)>,
    frontmatter_end: Option<usize>,
}

fn compute_markdown_block_masks(lines: &[String]) -> MarkdownBlockMasks {
    let n = lines.len();
    let mut in_fence = vec![false; n];
    let mut fence_bounds = Vec::new();
    let mut math_bounds = Vec::new();
    let mut frontmatter_end = None;

    if n >= 2 {
        let first_trim = lines[0]
            .strip_prefix('\u{feff}')
            .unwrap_or(&lines[0])
            .trim();
        if matches!(first_trim, "---" | "+++") {
            for (idx, line) in lines.iter().enumerate().skip(1) {
                let t = line.trim();
                if t == first_trim || (first_trim == "---" && t == "...") {
                    frontmatter_end = Some(idx);
                    break;
                }
            }
        }
    }

    let start_scan = frontmatter_end.map_or(0, |end| end + 1);
    let mut open_fence: Option<(u8, usize, usize)> = None;
    let mut open_math: Option<usize> = None;

    for (idx, line) in lines.iter().enumerate().skip(start_scan) {
        if let Some((f_char, f_len, f_start)) = open_fence {
            in_fence[idx] = true;
            if let Some((c_char, c_len, is_pure_close)) = parse_fence_marker(line)
                && is_pure_close
                && c_char == f_char
                && ((f_char == b'@') || c_len >= f_len)
            {
                fence_bounds.push((f_start, idx));
                open_fence = None;
            }
            continue;
        }
        if let Some((f_char, f_len, _)) = parse_fence_marker(line) {
            in_fence[idx] = true;
            open_fence = Some((f_char, f_len, idx));
            continue;
        }
        let trimmed = line.trim();
        if trimmed == "$$"
            || (trimmed.starts_with("$$") && !trimmed[2..].trim().ends_with("$$"))
            || trimmed == "\\["
            || trimmed == "\\]"
        {
            if let Some(m_start) = open_math {
                math_bounds.push((m_start, idx));
                open_math = None;
            } else {
                open_math = Some(idx);
            }
        }
    }
    if let Some((_, _, f_start)) = open_fence {
        fence_bounds.push((f_start, n.saturating_sub(1)));
    }
    if let Some(m_start) = open_math {
        math_bounds.push((m_start, n.saturating_sub(1)));
    }

    MarkdownBlockMasks {
        in_fence,
        fence_bounds,
        math_bounds,
        frontmatter_end,
    }
}

fn is_setext_underline(line: &str) -> Option<usize> {
    let raw = line.strip_prefix('\u{feff}').unwrap_or(line);
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    if raw.starts_with('\t') || leading_spaces > 3 {
        return None;
    }
    let trimmed = raw[leading_spaces..].trim_end();
    if trimmed.len() < 3 {
        return None;
    }
    if trimmed.bytes().all(|b| b == b'=') {
        Some(1)
    } else if trimmed.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

fn is_valid_setext_title_line(line: &str) -> bool {
    let raw = line.strip_prefix('\u{feff}').unwrap_or(line);
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    if raw.starts_with('\t') || leading_spaces > 3 {
        return false;
    }
    let trimmed = raw[leading_spaces..].trim();
    if trimmed.is_empty() || is_pipe_table_separator(trimmed) {
        return false;
    }
    !trimmed.starts_with(['#', '>', '-', '*', '+', '|', '<', '`', '~', ':'])
}

fn effective_heading_level_at(
    lines: &[String],
    idx: usize,
    masks: &MarkdownBlockMasks,
) -> Option<usize> {
    if masks.in_fence.get(idx).copied().unwrap_or(false) {
        return None;
    }
    if masks.frontmatter_end.is_some_and(|fm_end| idx <= fm_end) {
        return None;
    }
    if let Some(lvl) = heading_level_of_line(&lines[idx]) {
        return Some(lvl);
    }
    if idx + 1 < lines.len()
        && !masks.in_fence[idx + 1]
        && is_valid_setext_title_line(&lines[idx])
        && let Some(lvl) = is_setext_underline(&lines[idx + 1])
    {
        return Some(lvl);
    }
    None
}

fn is_table_line_in_context(lines: &[String], idx: usize) -> bool {
    let trimmed = lines[idx].trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3 {
        return true;
    }
    if !trimmed.contains('|')
        || trimmed.starts_with(['#', '>', '`', '~'])
        || (trimmed.starts_with("- [") && trimmed.ends_with(')'))
    {
        return false;
    }
    is_pipe_table_separator(trimmed)
        || (idx > 0 && is_pipe_table_separator(lines[idx - 1].trim()))
        || (idx + 1 < lines.len() && is_pipe_table_separator(lines[idx + 1].trim()))
}

pub fn clamp_document_snippet(
    path: &str,
    lines: &[String],
    min_idx: usize,
    max_idx: usize,
) -> Option<(usize, usize)> {
    if !is_document_path(path) || lines.len() <= 2 || min_idx > max_idx || max_idx >= lines.len() {
        return None;
    }

    let masks = compute_markdown_block_masks(lines);

    // 1. YAML/TOML Frontmatter block at start of file (--- .. ---/... or +++ .. +++)
    if let Some(fm_end) = masks.frontmatter_end
        && max_idx <= fm_end
    {
        let mut end = fm_end;
        // Include immediately following blank line + H1 title if present
        let mut look = fm_end + 1;
        while look < lines.len() && lines[look].trim().is_empty() {
            look += 1;
        }
        if look < lines.len() && heading_level_of_line(&lines[look]) == Some(1) {
            end = look;
        }
        return Some((0, end));
    }

    // 2. Pipe table clamping: if any hit is inside a Markdown/extracted table, keep table header, caption & rows
    if !masks.in_fence[min_idx]
        && !masks.in_fence[max_idx]
        && (is_table_line_in_context(lines, min_idx) || is_table_line_in_context(lines, max_idx))
    {
        let pivot = if is_table_line_in_context(lines, min_idx) {
            min_idx
        } else {
            max_idx
        };
        let mut tbl_start = pivot;
        while tbl_start > 0 && is_table_line_in_context(lines, tbl_start - 1) {
            tbl_start -= 1;
        }
        let mut tbl_end = pivot;
        while tbl_end + 1 < lines.len() && is_table_line_in_context(lines, tbl_end + 1) {
            tbl_end += 1;
        }
        tbl_start = tbl_start.min(min_idx);
        tbl_end = tbl_end.max(max_idx);
        // Include an immediately preceding heading or caption (allowing 1 blank line)
        if tbl_start > 0 {
            let prev = tbl_start - 1;
            if effective_heading_level_at(lines, prev, &masks).is_some()
                || lines[prev].trim_start().starts_with("Table:")
                || lines[prev].trim_start().starts_with(": ")
            {
                tbl_start = prev;
            } else if lines[prev].trim().is_empty()
                && prev > 0
                && effective_heading_level_at(lines, prev - 1, &masks).is_some()
            {
                tbl_start = prev - 1;
            }
        }
        return Some((tbl_start, tbl_end));
    }

    // 2b. Markdown Admonition container (:::note .. :::)
    if is_markdown_path(path) && !masks.in_fence[min_idx] {
        let mut adm_open = None;
        for idx in (0..=min_idx).rev() {
            let t = lines[idx].trim();
            if idx < min_idx && t == ":::" {
                break;
            }
            if is_markdown_admonition_open(t) {
                adm_open = Some(idx);
                break;
            }
        }
        if let Some(open_idx) = adm_open {
            for (close_idx, line) in lines.iter().enumerate().skip(max_idx) {
                if close_idx > open_idx && line.trim() == ":::" {
                    return Some((open_idx, close_idx));
                }
            }
        }

        // 2c. HTML <details> .. </details> collapsible block
        let mut details_open = None;
        for idx in (0..=min_idx).rev() {
            let lower = lines[idx].trim().to_ascii_lowercase();
            if idx < min_idx && lower.contains("</details>") {
                break;
            }
            if lower.starts_with("<details") {
                details_open = Some(idx);
                break;
            }
        }
        if let Some(open_idx) = details_open {
            for (close_idx, line) in lines.iter().enumerate().skip(max_idx) {
                if line.trim().to_ascii_lowercase().contains("</details>") {
                    return Some((open_idx, close_idx));
                }
            }
        }

        // 2d. Blockquote / GFM Alert (> [!NOTE] ..) contiguous block
        if lines[min_idx].trim_start().starts_with('>')
            && lines[max_idx].trim_start().starts_with('>')
        {
            let mut bq_start = min_idx;
            while bq_start > 0 && lines[bq_start - 1].trim_start().starts_with('>') {
                bq_start -= 1;
            }
            let mut bq_end = max_idx;
            while bq_end + 1 < lines.len() && lines[bq_end + 1].trim_start().starts_with('>') {
                bq_end += 1;
            }
            if bq_start > 0 && effective_heading_level_at(lines, bq_start - 1, &masks).is_some() {
                bq_start -= 1;
            }
            return Some((bq_start, bq_end));
        }

        // 2e. Multi-line footnote definition ([^id]: ... followed by indented continuation lines)
        let mut fn_start = None;
        if is_markdown_footnote_definition(lines[min_idx].trim_start()) {
            fn_start = Some(min_idx);
        } else if lines[min_idx].starts_with("  ") || lines[min_idx].starts_with('\t') {
            for idx in (0..min_idx).rev() {
                if is_markdown_footnote_definition(lines[idx].trim_start()) {
                    fn_start = Some(idx);
                    break;
                }
                if !(lines[idx].starts_with("  ") || lines[idx].starts_with('\t')) {
                    break;
                }
            }
        }
        if let Some(start_idx) = fn_start {
            let mut fn_end = max_idx;
            while fn_end + 1 < lines.len() {
                let next = &lines[fn_end + 1];
                if next.starts_with("  ") || next.starts_with('\t') {
                    fn_end += 1;
                } else {
                    break;
                }
            }
            return Some((start_idx, fn_end));
        }
    }

    // 3. LaTeX environment (\begin{...} .. \end{...}), BibTeX entry (@article{...} .. }), or VTT/SRT cue
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if (ext == "vtt" || ext == "srt") && !lines.iter().any(|l| is_extracted_unit_boundary(l)) {
        let mut start = min_idx;
        while start > 0 && !lines[start - 1].trim().is_empty() {
            start -= 1;
        }
        let mut end = max_idx;
        while end + 1 < lines.len() && !lines[end + 1].trim().is_empty() {
            end += 1;
        }
        return Some((start, end));
    }
    if ext == "bib" {
        let mut start = 0usize;
        for idx in (0..=min_idx).rev() {
            if lines[idx].trim_start().starts_with('@') {
                start = idx;
                break;
            }
        }
        let mut end = lines.len() - 1;
        for (idx, line) in lines.iter().enumerate().skip(max_idx) {
            if idx > start && line.trim() == "}" {
                end = idx;
                break;
            }
            if idx > max_idx && line.trim_start().starts_with('@') {
                end = idx - 1;
                break;
            }
        }
        return Some((start, end));
    }
    if ext == "tex" || ext == "sty" || ext == "cls" {
        for idx in (0..=min_idx).rev() {
            let trimmed = lines[idx].trim_start();
            if let Some(rest) = trimmed.strip_prefix("\\begin{")
                && let Some(env_end) = rest.find('}')
            {
                let env_name = &rest[..env_end];
                if env_name != "document" {
                    let close_tag = format!("\\end{{{env_name}}}");
                    for (end_idx, end_line) in lines.iter().enumerate().skip(max_idx) {
                        if end_line.contains(&close_tag) {
                            return Some((idx, end_idx));
                        }
                    }
                }
            }
        }
    }

    // 4. Extracted container unit boundaries (Slide / Sheet / Page / Cell / Chapter)
    let mut new_start = masks.frontmatter_end.map_or(0, |fm| {
        if min_idx > fm { fm + 1 } else { 0 }
    });
    let mut new_end = lines.len() - 1;
    let mut found_unit = false;
    for idx in (new_start..=min_idx).rev() {
        if !masks.in_fence[idx] && is_extracted_unit_boundary(&lines[idx]) {
            new_start = idx;
            found_unit = true;
            break;
        }
    }
    if found_unit {
        for (idx, line) in lines.iter().enumerate().skip(max_idx + 1) {
            if !masks.in_fence[idx] && is_extracted_unit_boundary(line) {
                new_end = idx - 1;
                break;
            }
        }
    } else {
        // 5. Fence-aware ATX & Setext Heading Section Clamping
        let mut start_level: Option<usize> = None;
        for idx in (new_start..=min_idx).rev() {
            if let Some(lvl) = effective_heading_level_at(lines, idx, &masks) {
                new_start = idx;
                start_level = Some(lvl);
                break;
            }
        }
        for idx in (max_idx + 1)..lines.len() {
            if let Some(lvl) = effective_heading_level_at(lines, idx, &masks)
                && start_level.is_none_or(|s_lvl| lvl <= s_lvl)
            {
                new_end = idx - 1;
                break;
            }
        }
    }

    // 6. Fenced code block (` ``` ` / `~~~`) and math block (`$$`) integrity check
    for &(f_start, f_end) in &masks.fence_bounds {
        if new_start > f_start && new_start <= f_end {
            new_start = f_start;
        }
        if new_end >= f_start && new_end < f_end {
            new_end = f_end;
        }
    }
    for &(m_start, m_end) in &masks.math_bounds {
        if new_start > m_start && new_start <= m_end {
            new_start = m_start;
        }
        if new_end >= m_start && new_end < m_end {
            new_end = m_end;
        }
    }

    while new_start < min_idx && lines[new_start].trim().is_empty() {
        new_start += 1;
    }
    while new_end > max_idx && lines[new_end].trim().is_empty() {
        new_end -= 1;
    }
    Some((new_start, new_end))
}

fn strip_inline_markdown_for_slug(raw: &str) -> (String, Option<String>) {
    let mut text = raw.trim();
    // Strip trailing closing hashes in ATX headings: "## Heading ##" -> "Heading"
    while text.ends_with('#') {
        let without_hashes = text.trim_end_matches('#');
        if without_hashes.is_empty() || without_hashes.ends_with([' ', '\t']) {
            text = without_hashes.trim_end();
        } else {
            break;
        }
    }
    // Check explicit {#custom-id} suffix
    let mut explicit_id = None;
    if let Some(open_brace) = text.rfind("{#")
        && text.ends_with('}')
    {
        let id = text[open_brace + 2..text.len() - 1].trim();
        if !id.is_empty() && !id.contains(' ') {
            explicit_id = Some(id.to_ascii_lowercase());
            text = text[..open_brace].trim_end();
        }
    }
    // Unwrap inline links [label](url) -> label
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open_br) = rest.find('[') {
        out.push_str(&rest[..open_br]);
        let after_open = &rest[open_br + 1..];
        if let Some(close_br) = after_open.find("](") {
            let label = &after_open[..close_br];
            let after_paren = &after_open[close_br + 2..];
            if let Some(close_p) = after_paren.find(')') {
                out.push_str(label);
                rest = &after_paren[close_p + 1..];
                continue;
            }
        }
        out.push('[');
        rest = after_open;
    }
    out.push_str(rest);
    // Strip inline HTML tags (e.g. <code>, </code>, <em>, <strong>) and formatting markers
    let mut clean_inline = String::with_capacity(out.len());
    let mut scan = out.as_str();
    while let Some(lt) = scan.find('<') {
        clean_inline.push_str(&scan[..lt]);
        let after_lt = &scan[lt + 1..];
        if let Some(gt) = after_lt.find('>') {
            let tag = &after_lt[..gt];
            if !tag.is_empty()
                && !tag.contains(' ')
                && tag
                    .trim_start_matches('/')
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric())
            {
                scan = &after_lt[gt + 1..];
                continue;
            }
        }
        clean_inline.push('<');
        scan = after_lt;
    }
    clean_inline.push_str(scan);
    let clean_inline = clean_inline.replace(['`', '*', '~'], "");
    (clean_inline, explicit_id)
}

pub fn github_heading_slug(heading_line: &str) -> String {
    let raw = heading_line.strip_prefix('\u{feff}').unwrap_or(heading_line).trim();
    let stripped = raw.trim_start_matches(['#', '=', '*']).trim_start();
    let (cleaned, explicit_id) = strip_inline_markdown_for_slug(stripped);
    if let Some(id) = explicit_id {
        return id;
    }
    let mut slug = String::with_capacity(cleaned.len());
    for ch in cleaned.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                slug.push(lower);
            }
        } else if ch == ' ' || ch == '\t' || ch == '-' {
            slug.push('-');
        } else if ch == '_' {
            slug.push('_');
        }
    }
    slug.trim_matches('-').to_owned()
}

fn collapse_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_hyphen {
                out.push('-');
            }
            prev_hyphen = true;
        } else {
            out.push(ch);
            prev_hyphen = false;
        }
    }
    out
}

pub fn resolve_markdown_anchor_range(text: &str, anchor: &str) -> Option<(usize, usize)> {
    let target = anchor.trim().trim_start_matches('#').to_lowercase();
    if target.is_empty() {
        return None;
    }
    let target_collapsed = collapse_hyphens(&target);
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        return None;
    }
    let masks = compute_markdown_block_masks(&lines);
    let unit_prefix = if let Some(num) = target.strip_prefix("cell-") {
        Some(format!("## [notebook cell {num}:"))
    } else if let Some(num) = target.strip_prefix("slide-") {
        Some(format!("## [slide {num}]"))
    } else if let Some(num) = target.strip_prefix("page-") {
        Some(format!("## [page {num}]"))
    } else {
        target
            .strip_prefix("record-")
            .map(|num| format!("## [record {num}]"))
    };
    if let Some(prefix) = unit_prefix {
        for (idx, line) in lines.iter().enumerate() {
            if !masks.in_fence[idx] && line.trim_start().to_lowercase().starts_with(&prefix) {
                let mut end_idx = lines.len() - 1;
                for (next_idx, next_line) in lines.iter().enumerate().skip(idx + 1) {
                    if !masks.in_fence[next_idx] && is_extracted_unit_boundary(next_line) {
                        end_idx = next_idx - 1;
                        break;
                    }
                }
                while end_idx > idx && lines[end_idx].trim().is_empty() {
                    end_idx -= 1;
                }
                return Some((idx + 1, (end_idx + 1).min(idx + 120)));
            }
        }
    }
    let mut slug_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for idx in 0..lines.len() {
        if masks.in_fence[idx] {
            continue;
        }
        // Check explicit HTML <a id="..."> or <a name="...">
        let lower_line = lines[idx].to_lowercase();
        if lower_line.contains(&format!("id=\"{target}\""))
            || lower_line.contains(&format!("name=\"{target}\""))
            || lower_line.contains(&format!("id='{target}'"))
            || lower_line.contains(&format!("name='{target}'"))
        {
            let end = (idx + 40).min(lines.len());
            return Some((idx + 1, end));
        }
        let Some(level) = effective_heading_level_at(&lines, idx, &masks) else {
            continue;
        };
        let base_slug = github_heading_slug(&lines[idx]);
        if base_slug.is_empty() {
            continue;
        }
        let count = slug_counts.entry(base_slug.clone()).or_insert(0);
        let versioned_slug = if *count == 0 {
            base_slug.clone()
        } else {
            format!("{base_slug}-{count}")
        };
        *count += 1;

        if versioned_slug == target
            || base_slug == target
            || collapse_hyphens(&versioned_slug) == target_collapsed
        {
            let mut end_idx = lines.len() - 1;
            let skip_from = if heading_level_of_line(&lines[idx]).is_some() {
                idx + 1
            } else {
                idx + 2
            };
            for next_idx in skip_from..lines.len() {
                if let Some(next_lvl) = effective_heading_level_at(&lines, next_idx, &masks)
                    && next_lvl <= level
                {
                    end_idx = next_idx - 1;
                    break;
                }
            }
            while end_idx > idx && lines[end_idx].trim().is_empty() {
                end_idx -= 1;
            }
            let clamped_end = end_idx.min(idx + 119);
            return Some((idx + 1, clamped_end + 1));
        }
    }
    None
}

pub fn inspect_structure(path: &str, text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let masks = compute_markdown_block_masks(&lines);
    let is_md = is_markdown_path(path);
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut out = Vec::new();
    if matches!(ext.as_str(), "jsonl" | "ndjson" | "jsonlines")
        && !lines.iter().any(|l| is_extracted_unit_boundary(l))
    {
        let mut rec_num = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                rec_num += 1;
                let summary = val
                    .get("id")
                    .or_else(|| val.get("name"))
                    .or_else(|| val.get("type"))
                    .or_else(|| val.get("event"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("object");
                out.push(format!("L{}: [Record {rec_num}] {summary}", idx + 1));
            }
        }
        return out;
    }
    if ext == "svg" {
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if let Some(title) = extract_xml_tag_text(trimmed, "title") {
                out.push(format!("L{}: [svg:title] {title}", idx + 1));
            } else if let Some(desc) = extract_xml_tag_text(trimmed, "desc") {
                out.push(format!("L{}: [svg:desc] {desc}", idx + 1));
            } else if (trimmed.starts_with("<g ") || trimmed.starts_with("<symbol "))
                && let Some(id) = extract_xml_attr(trimmed, "id")
            {
                out.push(format!("L{}: [svg:id] #{id}", idx + 1));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(fm_end) = masks.frontmatter_end {
        for (idx, line) in lines.iter().enumerate().take(fm_end).skip(1) {
            let trimmed = line.trim();
            if trimmed.starts_with("title:") || trimmed.starts_with("name:") {
                out.push(format!("L{}: [frontmatter] {}", idx + 1, trimmed));
            }
        }
    }
    for (idx, line) in lines.iter().enumerate() {
        if masks.in_fence[idx] {
            continue;
        }
        if is_extracted_unit_boundary(line) {
            out.push(format!("L{}: {}", idx + 1, line.trim()));
        } else if effective_heading_level_at(&lines, idx, &masks).is_some() {
            if is_md {
                let slug = github_heading_slug(line);
                if !slug.is_empty() {
                    out.push(format!("L{}: {} (#{slug})", idx + 1, line.trim()));
                    continue;
                }
            }
            out.push(format!("L{}: {}", idx + 1, line.trim()));
        }
    }
    out
}






