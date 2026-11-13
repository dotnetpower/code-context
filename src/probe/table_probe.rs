use super::http_probe;
use super::json_probe::compact_sample_value;
use super::{
    BBox, Predicate, TableProbeArgs, clamp_json_budget, clamp_utf8_bytes, truncate_utf8_chars,
};
use crate::inline_budget::INLINE_BUDGET;
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

const MAX_PARQUET_FOOTER_BYTES: usize = 32 * 1024 * 1024;
const MAX_THRIFT_DEPTH: usize = 32;
const MAX_THRIFT_LIST_LEN: usize = 200_000;
const MAX_CSV_LINES: usize = 500_000;

#[derive(Clone, Debug, Serialize)]
pub struct ParquetSummary {
    pub source: String,
    pub format: &'static str,
    pub version: i32,
    pub num_rows: i64,
    pub row_groups: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_row_groups: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rows_upper_bound: Option<i64>,
    pub total_compressed_bytes: i64,
    pub total_uncompressed_bytes: i64,
    pub codecs: Vec<String>,
    pub columns: Vec<ParquetColumnInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geo_metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParquetColumnInfo {
    pub name: String,
    pub physical_type: &'static str,
    pub repetition: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub converted_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_sample: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sample: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteSummary {
    pub source: String,
    pub format: &'static str,
    pub engine: &'static str,
    pub page_size: u32,
    pub wal_mode: bool,
    pub tables: Vec<SqliteTableInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_rows: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteTableInfo {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<i64>,
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CsvSummary {
    pub source: String,
    pub format: &'static str,
    pub delimiter: &'static str,
    pub rows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rows: Option<usize>,
    pub columns: Vec<CsvColInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub counts_by: Vec<(String, usize)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CsvColInfo {
    pub name: String,
    pub non_empty: usize,
}

// ============================================================================
// Pure-Rust Apache Parquet Thrift Compact Protocol Footer Inspector
// ============================================================================

struct CompactReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> CompactReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| invalid("unexpected EOF in Parquet Thrift metadata"))?;
        self.pos += 1;
        Ok(b)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| invalid("Thrift length overflow"))?;
        if end > self.buf.len() {
            return Err(invalid("unexpected EOF reading Thrift binary field").into());
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_varint_u64(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            if shift >= 70 {
                return Err(invalid("varint too long in Parquet footer").into());
            }
            let b = self.read_u8()?;
            if shift == 63 && (b & 0x7E) != 0 {
                return Err(invalid("u64 varint overflow in Parquet footer").into());
            }
            result |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn read_zigzag_i64(&mut self) -> Result<i64> {
        let n = self.read_varint_u64()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    fn read_zigzag_i32(&mut self) -> Result<i32> {
        let raw = self.read_varint_u64()?;
        let n = u32::try_from(raw)
            .map_err(|_| invalid("i32 zigzag varint overflow in Parquet footer"))?;
        Ok(((n >> 1) as i32) ^ -((n & 1) as i32))
    }

    fn read_zigzag_i16(&mut self) -> Result<i16> {
        let v = self.read_zigzag_i32()?;
        i16::try_from(v).map_err(|_| invalid("i16 zigzag varint overflow in Parquet footer").into())
    }

    fn read_binary(&mut self) -> Result<&'a [u8]> {
        let raw = self.read_varint_u64()?;
        let len = usize::try_from(raw).map_err(|_| invalid("Thrift binary length overflow"))?;
        self.read_bytes(len)
    }

    fn read_string(&mut self) -> Result<String> {
        let bytes = self.read_binary()?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn read_field_header(&mut self, prev_id: i16) -> Result<Option<(i16, u8)>> {
        let b = self.read_u8()?;
        if b == 0 {
            return Ok(None);
        }
        let delta = ((b >> 4) & 0x0F) as i16;
        let wire_type = b & 0x0F;
        let field_id = if delta == 0 {
            self.read_zigzag_i16()?
        } else {
            prev_id.saturating_add(delta)
        };
        Ok(Some((field_id, wire_type)))
    }

    fn read_list_header(&mut self) -> Result<(usize, u8)> {
        let b = self.read_u8()?;
        let size_nibble = (b >> 4) & 0x0F;
        let elem_type = b & 0x0F;
        let len = if size_nibble == 0x0F {
            let raw = self.read_varint_u64()?;
            usize::try_from(raw).map_err(|_| invalid("Parquet Thrift list length overflow"))?
        } else {
            size_nibble as usize
        };
        if len > MAX_THRIFT_LIST_LEN {
            return Err(invalid(format!("Parquet Thrift list too large ({len})")).into());
        }
        Ok((len, elem_type))
    }

    fn skip_value(&mut self, wire_type: u8, depth: usize) -> Result<()> {
        if depth > MAX_THRIFT_DEPTH {
            return Err(invalid("Parquet Thrift nesting exceeded limit").into());
        }
        match wire_type {
            1 | 2 => Ok(()),
            3 => {
                self.read_u8()?;
                Ok(())
            }
            4..=6 => {
                self.read_varint_u64()?;
                Ok(())
            }
            7 => {
                self.read_bytes(8)?;
                Ok(())
            }
            8 => {
                self.read_binary()?;
                Ok(())
            }
            9 | 10 => {
                let (len, elem_type) = self.read_list_header()?;
                for _ in 0..len {
                    // In lists/sets, compact bool elements consume 1 byte (`1` or `2`).
                    if matches!(elem_type, 1 | 2) {
                        self.read_u8()?;
                    } else {
                        self.skip_value(elem_type, depth + 1)?;
                    }
                }
                Ok(())
            }
            11 => {
                let len = self.read_varint_u64()? as usize;
                if len > MAX_THRIFT_LIST_LEN {
                    return Err(invalid("Parquet Thrift map too large").into());
                }
                if len > 0 {
                    let kv = self.read_u8()?;
                    let kt = (kv >> 4) & 0x0F;
                    let vt = kv & 0x0F;
                    for _ in 0..len {
                        if matches!(kt, 1 | 2) {
                            self.read_u8()?;
                        } else {
                            self.skip_value(kt, depth + 1)?;
                        }
                        if matches!(vt, 1 | 2) {
                            self.read_u8()?;
                        } else {
                            self.skip_value(vt, depth + 1)?;
                        }
                    }
                }
                Ok(())
            }
            12 => {
                let mut prev = 0i16;
                while let Some((fid, wt)) = self.read_field_header(prev)? {
                    prev = fid;
                    self.skip_value(wt, depth + 1)?;
                }
                Ok(())
            }
            other => Err(invalid(format!("unknown Thrift wire type {other}")).into()),
        }
    }
}

#[derive(Default)]
struct RawSchemaElem {
    name: String,
    phys_type: Option<i32>,
    rep_type: Option<i32>,
    num_children: Option<i32>,
    converted_type: Option<i32>,
}

#[derive(Default)]
struct RawColStats {
    path: String,
    phys_type: i32,
    codec: i32,
    uncompressed: i64,
    compressed: i64,
    min_bytes: Option<Vec<u8>>,
    max_bytes: Option<Vec<u8>>,
}

#[derive(Default)]
struct RawRowGroup {
    num_rows: i64,
    total_byte_size: i64,
    cols: Vec<RawColStats>,
}

fn physical_type_name(t: i32) -> &'static str {
    match t {
        0 => "BOOLEAN",
        1 => "INT32",
        2 => "INT64",
        3 => "INT96",
        4 => "FLOAT",
        5 => "DOUBLE",
        6 => "BYTE_ARRAY",
        7 => "FIXED_LEN_BYTE_ARRAY",
        _ => "UNKNOWN",
    }
}

fn repetition_name(r: Option<i32>) -> &'static str {
    match r {
        Some(0) => "REQUIRED",
        Some(1) => "OPTIONAL",
        Some(2) => "REPEATED",
        _ => "GROUP",
    }
}

fn converted_type_name(c: i32) -> Option<&'static str> {
    Some(match c {
        0 => "UTF8",
        1 => "MAP",
        3 => "LIST",
        5 => "DECIMAL",
        6 => "DATE",
        9 => "TIMESTAMP_MILLIS",
        10 => "TIMESTAMP_MICROS",
        11 => "UINT_8",
        12 => "UINT_16",
        13 => "UINT_32",
        14 => "UINT_64",
        15 => "INT_8",
        16 => "INT_16",
        17 => "INT_32",
        18 => "INT_64",
        19 => "JSON",
        20 => "BSON",
        _ => return None,
    })
}

fn codec_name(c: i32) -> &'static str {
    match c {
        0 => "UNCOMPRESSED",
        1 => "SNAPPY",
        2 => "GZIP",
        3 => "LZO",
        4 => "BROTLI",
        5 => "LZ4",
        6 => "ZSTD",
        7 => "LZ4_RAW",
        _ => "OTHER",
    }
}

fn decode_stat_scalar(phys_type: i32, converted: Option<i32>, raw: &[u8]) -> Option<String> {
    match phys_type {
        0 if !raw.is_empty() => Some((raw[0] != 0).to_string()),
        1 if raw.len() == 4 => Some(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]).to_string()),
        2 if raw.len() == 8 => Some(
            i64::from_le_bytes([raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7]])
                .to_string(),
        ),
        4 if raw.len() == 4 => {
            let v = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            Some(format!("{v:.5}"))
        }
        5 if raw.len() == 8 => {
            let v = f64::from_le_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]);
            Some(format!("{v:.6}"))
        }
        6 => {
            if converted == Some(0) || std::str::from_utf8(raw).is_ok() {
                let s = String::from_utf8_lossy(raw);
                if s.chars().all(|c| !c.is_control()) {
                    return Some(truncate_utf8_chars(&s, 48));
                }
            }
            Some(format!("<{}B>", raw.len()))
        }
        _ => None,
    }
}

fn stat_as_f64(phys_type: i32, raw: &[u8]) -> Option<f64> {
    match phys_type {
        4 if raw.len() == 4 => {
            let v = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64;
            v.is_finite().then_some(v)
        }
        5 if raw.len() == 8 => {
            let v = f64::from_le_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]);
            v.is_finite().then_some(v)
        }
        _ => None,
    }
}

fn compare_parquet_stat(phys_type: i32, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    match phys_type {
        1 if a.len() == 4 && b.len() == 4 => {
            let va = i32::from_le_bytes([a[0], a[1], a[2], a[3]]);
            let vb = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            va.cmp(&vb)
        }
        2 if a.len() == 8 && b.len() == 8 => {
            let va = i64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
            let vb = i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            va.cmp(&vb)
        }
        4 if a.len() == 4 && b.len() == 4 => {
            let va = f32::from_le_bytes([a[0], a[1], a[2], a[3]]);
            let vb = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            va.total_cmp(&vb)
        }
        5 if a.len() == 8 && b.len() == 8 => {
            let va = f64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
            let vb = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            va.total_cmp(&vb)
        }
        _ => a.cmp(b),
    }
}

fn parse_parquet_footer_bytes(
    source_label: &str,
    footer: &[u8],
    bbox_filter: Option<&BBox>,
) -> Result<ParquetSummary> {
    let mut r = CompactReader::new(footer);
    let mut version = 1i32;
    let mut num_rows = 0i64;
    let mut schema_elems: Vec<RawSchemaElem> = Vec::new();
    let mut row_groups: Vec<RawRowGroup> = Vec::new();
    let mut kv_meta: BTreeMap<String, String> = BTreeMap::new();

    let mut prev = 0i16;
    while let Some((fid, wt)) = r.read_field_header(prev)? {
        prev = fid;
        match (fid, wt) {
            (1, 5) => version = r.read_zigzag_i32()?,
            (2, 9) => {
                let (len, elem_wt) = r.read_list_header()?;
                for _ in 0..len {
                    if elem_wt != 12 {
                        r.skip_value(elem_wt, 1)?;
                        continue;
                    }
                    let mut elem = RawSchemaElem::default();
                    let mut sp = 0i16;
                    while let Some((sfid, swt)) = r.read_field_header(sp)? {
                        sp = sfid;
                        match (sfid, swt) {
                            (1, 5) => elem.phys_type = Some(r.read_zigzag_i32()?),
                            (3, 5) => elem.rep_type = Some(r.read_zigzag_i32()?),
                            (4, 8) => elem.name = r.read_string()?,
                            (5, 5) => elem.num_children = Some(r.read_zigzag_i32()?),
                            (6, 5) => elem.converted_type = Some(r.read_zigzag_i32()?),
                            (10, 12) => {
                                // Parquet v2 LogicalType union struct
                                let mut lp = 0i16;
                                while let Some((lfid, lwt)) = r.read_field_header(lp)? {
                                    lp = lfid;
                                    let mapped = match lfid {
                                        1 => Some(0),  // STRING -> UTF8
                                        2 => Some(1),  // MAP
                                        3 => Some(3),  // LIST
                                        5 => Some(5),  // DECIMAL
                                        6 => Some(6),  // DATE
                                        8 => Some(10), // TIMESTAMP
                                        12 => Some(19), // JSON
                                        13 => Some(20), // BSON
                                        _ => None,
                                    };
                                    if elem.converted_type.is_none() {
                                        elem.converted_type = mapped;
                                    }
                                    r.skip_value(lwt, 3)?;
                                }
                            }
                            _ => r.skip_value(swt, 2)?,
                        }
                    }
                    schema_elems.push(elem);
                }
            }
            (3, 6) => num_rows = r.read_zigzag_i64()?,
            (4, 9) => {
                let (len, elem_wt) = r.read_list_header()?;
                for _ in 0..len {
                    if elem_wt != 12 {
                        r.skip_value(elem_wt, 1)?;
                        continue;
                    }
                    row_groups.push(parse_row_group(&mut r)?);
                }
            }
            (5, 9) => {
                let (len, elem_wt) = r.read_list_header()?;
                for _ in 0..len {
                    if elem_wt != 12 {
                        r.skip_value(elem_wt, 1)?;
                        continue;
                    }
                    let mut k = String::new();
                    let mut v = String::new();
                    let mut kp = 0i16;
                    while let Some((kfid, kwt)) = r.read_field_header(kp)? {
                        kp = kfid;
                        match (kfid, kwt) {
                            (1, 8) => k = r.read_string()?,
                            (2, 8) => v = r.read_string()?,
                            _ => r.skip_value(kwt, 2)?,
                        }
                    }
                    if !k.is_empty() {
                        kv_meta.insert(k, v);
                    }
                }
            }
            _ => r.skip_value(wt, 1)?,
        }
    }

    // Reconstruct leaf dotted paths from nested SchemaElement tree (skipping index 0 = root `schema`).
    let leaf_schemas = reconstruct_leaf_schemas(&schema_elems);

    struct ColBoundAcc {
        phys_type: i32,
        min_bytes: Option<Vec<u8>>,
        max_bytes: Option<Vec<u8>>,
    }

    let mut total_compressed = 0i64;
    let mut total_uncompressed = 0i64;
    let mut codec_set: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut col_raw_bounds: BTreeMap<String, ColBoundAcc> = BTreeMap::new();
    let converted_lookup: BTreeMap<String, Option<i32>> = leaf_schemas
        .iter()
        .map(|(path, e)| (path.clone(), e.converted_type))
        .collect();

    let mut matched_rgs = 0usize;
    let mut matched_rows_ub = 0i64;

    for rg in &row_groups {
        for c in &rg.cols {
            total_compressed = total_compressed.saturating_add(c.compressed);
            total_uncompressed = total_uncompressed.saturating_add(c.uncompressed);
            *codec_set.entry(codec_name(c.codec)).or_default() += 1;

            let entry = col_raw_bounds
                .entry(c.path.clone())
                .or_insert_with(|| ColBoundAcc {
                    phys_type: c.phys_type,
                    min_bytes: None,
                    max_bytes: None,
                });
            if let Some(ref mb) = c.min_bytes {
                let replace = match entry.min_bytes {
                    None => true,
                    Some(ref cur) => compare_parquet_stat(c.phys_type, mb, cur).is_lt(),
                };
                if replace {
                    entry.min_bytes = Some(mb.clone());
                }
            }
            if let Some(ref xb) = c.max_bytes {
                let replace = match entry.max_bytes {
                    None => true,
                    Some(ref cur) => compare_parquet_stat(c.phys_type, xb, cur).is_gt(),
                };
                if replace {
                    entry.max_bytes = Some(xb.clone());
                }
            }
        }
        if let Some(qbox) = bbox_filter
            && row_group_intersects_bbox(rg, qbox)
        {
            matched_rgs += 1;
            matched_rows_ub = matched_rows_ub.saturating_add(rg.num_rows);
        }
    }

    let columns: Vec<ParquetColumnInfo> = leaf_schemas
        .into_iter()
        .take(48)
        .map(|(path, elem)| {
            let conv = converted_lookup.get(&path).copied().flatten();
            let (min_s, max_s) = col_raw_bounds
                .remove(&path)
                .map(|acc| {
                    (
                        acc.min_bytes
                            .as_deref()
                            .and_then(|b| decode_stat_scalar(acc.phys_type, conv, b)),
                        acc.max_bytes
                            .as_deref()
                            .and_then(|b| decode_stat_scalar(acc.phys_type, conv, b)),
                    )
                })
                .unwrap_or((None, None));
            ParquetColumnInfo {
                name: path,
                physical_type: elem.phys_type.map_or("GROUP", physical_type_name),
                repetition: repetition_name(elem.rep_type),
                converted_type: elem.converted_type.and_then(converted_type_name),
                min_sample: min_s,
                max_sample: max_s,
            }
        })
        .collect();

    let geo_metadata = kv_meta
        .get("geo")
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .map(|v| compact_sample_value(&v, 0));

    Ok(ParquetSummary {
        source: source_label.to_string(),
        format: "parquet",
        version,
        num_rows,
        row_groups: row_groups.len(),
        matched_row_groups: bbox_filter.map(|_| matched_rgs),
        matched_rows_upper_bound: bbox_filter.map(|_| matched_rows_ub),
        total_compressed_bytes: total_compressed,
        total_uncompressed_bytes: total_uncompressed,
        codecs: codec_set.keys().map(|s| (*s).to_string()).collect(),
        columns,
        geo_metadata,
    })
}

fn parse_row_group(r: &mut CompactReader<'_>) -> Result<RawRowGroup> {
    let mut rg = RawRowGroup::default();
    let mut prev = 0i16;
    while let Some((fid, wt)) = r.read_field_header(prev)? {
        prev = fid;
        match (fid, wt) {
            (1, 9) => {
                let (len, elem_wt) = r.read_list_header()?;
                for _ in 0..len {
                    if elem_wt != 12 {
                        r.skip_value(elem_wt, 2)?;
                        continue;
                    }
                    if let Some(col) = parse_column_chunk(r)? {
                        rg.cols.push(col);
                    }
                }
            }
            (2, 6) => rg.total_byte_size = r.read_zigzag_i64()?,
            (3, 6) => rg.num_rows = r.read_zigzag_i64()?,
            _ => r.skip_value(wt, 2)?,
        }
    }
    Ok(rg)
}

fn parse_column_chunk(r: &mut CompactReader<'_>) -> Result<Option<RawColStats>> {
    let mut out: Option<RawColStats> = None;
    let mut prev = 0i16;
    while let Some((fid, wt)) = r.read_field_header(prev)? {
        prev = fid;
        if fid == 3 && wt == 12 {
            let mut col = RawColStats::default();
            let mut mp = 0i16;
            while let Some((mfid, mwt)) = r.read_field_header(mp)? {
                mp = mfid;
                match (mfid, mwt) {
                    (1, 5) => col.phys_type = r.read_zigzag_i32()?,
                    (3, 9) => {
                        let (plen, pwt) = r.read_list_header()?;
                        let mut parts = Vec::with_capacity(plen.min(16));
                        for _ in 0..plen {
                            if pwt == 8 {
                                parts.push(r.read_string()?);
                            } else {
                                r.skip_value(pwt, 4)?;
                            }
                        }
                        col.path = parts.join(".");
                    }
                    (4, 5) => col.codec = r.read_zigzag_i32()?,
                    (6, 6) => col.uncompressed = r.read_zigzag_i64()?,
                    (7, 6) => col.compressed = r.read_zigzag_i64()?,
                    (12, 12) => {
                        let mut sp = 0i16;
                        let mut legacy_min: Option<Vec<u8>> = None;
                        let mut legacy_max: Option<Vec<u8>> = None;
                        while let Some((sfid, swt)) = r.read_field_header(sp)? {
                            sp = sfid;
                            match (sfid, swt) {
                                (1, 8) => legacy_max = Some(r.read_binary()?.to_vec()),
                                (2, 8) => legacy_min = Some(r.read_binary()?.to_vec()),
                                (5, 8) => col.max_bytes = Some(r.read_binary()?.to_vec()),
                                (6, 8) => col.min_bytes = Some(r.read_binary()?.to_vec()),
                                _ => r.skip_value(swt, 5)?,
                            }
                        }
                        if col.min_bytes.is_none() {
                            col.min_bytes = legacy_min;
                        }
                        if col.max_bytes.is_none() {
                            col.max_bytes = legacy_max;
                        }
                    }
                    _ => r.skip_value(mwt, 3)?,
                }
            }
            out = Some(col);
        } else {
            r.skip_value(wt, 3)?;
        }
    }
    Ok(out)
}

fn reconstruct_leaf_schemas(elems: &[RawSchemaElem]) -> Vec<(String, &RawSchemaElem)> {
    if elems.len() <= 1 {
        return Vec::new();
    }
    let mut leaves = Vec::new();
    let mut idx = 1usize;
    let root_children = elems[0].num_children.unwrap_or(0).max(0) as usize;
    fn walk<'a>(
        elems: &'a [RawSchemaElem],
        idx: &mut usize,
        prefix: &str,
        leaves: &mut Vec<(String, &'a RawSchemaElem)>,
        depth: usize,
    ) {
        if *idx >= elems.len() || depth > 24 {
            return;
        }
        let cur = &elems[*idx];
        *idx += 1;
        let full = if prefix.is_empty() {
            cur.name.clone()
        } else {
            format!("{prefix}.{}", cur.name)
        };
        let nchild = cur.num_children.unwrap_or(0);
        if nchild > 0 && cur.phys_type.is_none() {
            for _ in 0..(nchild as usize) {
                walk(elems, idx, &full, leaves, depth + 1);
            }
        } else {
            leaves.push((full, cur));
        }
    }
    for _ in 0..root_children {
        walk(elems, &mut idx, "", &mut leaves, 0);
    }
    if leaves.is_empty() {
        for e in &elems[1..] {
            if e.phys_type.is_some() {
                leaves.push((e.name.clone(), e));
            }
        }
    }
    leaves
}

fn row_group_intersects_bbox(rg: &RawRowGroup, qbox: &BBox) -> bool {
    let mut xmin_min: Option<f64> = None;
    let mut xmax_max: Option<f64> = None;
    let mut ymin_min: Option<f64> = None;
    let mut ymax_max: Option<f64> = None;

    for col in &rg.cols {
        let lower = col.path.to_ascii_lowercase();
        if (lower.ends_with("bbox.xmin") || lower == "xmin" || lower == "lon")
            && let Some(ref b) = col.min_bytes
        {
            xmin_min = stat_as_f64(col.phys_type, b);
        }
        if (lower.ends_with("bbox.xmax") || lower == "xmax" || lower == "lon")
            && let Some(ref b) = col.max_bytes
        {
            xmax_max = stat_as_f64(col.phys_type, b);
        }
        if (lower.ends_with("bbox.ymin") || lower == "ymin" || lower == "lat")
            && let Some(ref b) = col.min_bytes
        {
            ymin_min = stat_as_f64(col.phys_type, b);
        }
        if (lower.ends_with("bbox.ymax") || lower == "ymax" || lower == "lat")
            && let Some(ref b) = col.max_bytes
        {
            ymax_max = stat_as_f64(col.phys_type, b);
        }
    }
    match (xmin_min, ymin_min, xmax_max, ymax_max) {
        (Some(x0), Some(y0), Some(x1), Some(y1)) => qbox.intersects(&BBox {
            min_lon: x0.min(x1),
            min_lat: y0.min(y1),
            max_lon: x0.max(x1),
            max_lat: y0.max(y1),
        }),
        _ => true,
    }
}

fn inspect_local_parquet(path: &Path, bbox: Option<&BBox>) -> Result<ParquetSummary> {
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();
    if file_len < 12 {
        return Err(invalid(format!("Parquet file too small ({file_len}B)")).into());
    }
    let mut magic_head = [0u8; 4];
    f.read_exact(&mut magic_head)?;
    if &magic_head == b"PARE" {
        return Err(invalid("encrypted Parquet (PARE) is not supported without key").into());
    }
    if &magic_head != b"PAR1" {
        return Err(invalid(format!("invalid Parquet header magic in {}", path.display())).into());
    }
    f.seek(SeekFrom::End(-8))?;
    let mut tail = [0u8; 8];
    f.read_exact(&mut tail)?;
    if &tail[4..8] != b"PAR1" {
        return Err(invalid(format!("invalid Parquet footer magic in {}", path.display())).into());
    }
    let footer_len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
    if footer_len == 0 || footer_len > MAX_PARQUET_FOOTER_BYTES || (footer_len as u64) + 12 > file_len
    {
        return Err(invalid(format!("invalid Parquet footer length {footer_len}")).into());
    }
    f.seek(SeekFrom::End(-((footer_len as i64) + 8)))?;
    let mut footer = vec![0u8; footer_len];
    f.read_exact(&mut footer)?;
    parse_parquet_footer_bytes(&path.display().to_string(), &footer, bbox)
}

fn inspect_remote_parquet(
    url: &str,
    timeout: u64,
    bbox: Option<&BBox>,
) -> Result<ParquetSummary> {
    // Speculative tail range request (last 64 KiB covers 95%+ of Parquet footers in 1 RTT!).
    let init_window = 65_536usize;
    let hdr = vec![("Range".to_string(), format!("bytes=-{init_window}"))];
    let resp = http_probe::fetch_single_url(url, "GET", &hdr, None, timeout, &[])?;
    if !matches!(resp.status, 200 | 206) || resp.body.len() < 8 {
        return Err(invalid(format!(
            "remote Parquet Range fetch returned HTTP {} ({}B)",
            resp.status,
            resp.body.len()
        ))
        .into());
    }
    let body = &resp.body;
    let tail = &body[body.len() - 8..];
    if &tail[4..8] == b"PARE" {
        return Err(invalid("encrypted Parquet (PARE) is not supported without key").into());
    }
    if &tail[4..8] != b"PAR1" {
        return Err(invalid("remote target does not end with Parquet PAR1 footer magic").into());
    }
    let footer_len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
    if footer_len == 0 || footer_len > MAX_PARQUET_FOOTER_BYTES {
        return Err(invalid(format!("invalid remote Parquet footer length {footer_len}")).into());
    }
    if body.len() >= footer_len + 8 {
        let start = body.len() - 8 - footer_len;
        return parse_parquet_footer_bytes(&resp.redacted_url, &body[start..body.len() - 8], bbox);
    }
    let exact_need = footer_len + 8;
    let hdr2 = vec![("Range".to_string(), format!("bytes=-{exact_need}"))];
    let resp2 = http_probe::fetch_single_url(url, "GET", &hdr2, None, timeout, &[])?;
    if resp2.body.len() < exact_need {
        return Err(invalid("remote server did not honor exact Parquet footer Range").into());
    }
    let b2 = &resp2.body;
    let start = b2.len() - exact_need;
    parse_parquet_footer_bytes(&resp2.redacted_url, &b2[start..b2.len() - 8], bbox)
}

fn render_parquet_text(s: &ParquetSummary) -> String {
    let mut head = format!(
        "[{}] format=parquet v{} rows={} row_groups={} compressed={}B uncompressed={}B codecs={:?}",
        s.source,
        s.version,
        s.num_rows,
        s.row_groups,
        s.total_compressed_bytes,
        s.total_uncompressed_bytes,
        s.codecs
    );
    if let (Some(mrg), Some(mrows)) = (s.matched_row_groups, s.matched_rows_upper_bound) {
        head.push_str(&format!(
            " bbox_matched_row_groups={mrg}/{} (<= {mrows} rows)",
            s.row_groups
        ));
    }
    let mut lines = vec![head];
    for col in &s.columns {
        let mut line = format!("  - {}: {} ({}", col.name, col.physical_type, col.repetition);
        if let Some(ct) = col.converted_type {
            line.push_str(&format!(", {ct}"));
        }
        line.push(')');
        if col.min_sample.is_some() || col.max_sample.is_some() {
            line.push_str(&format!(
                " range=[{} .. {}]",
                col.min_sample.as_deref().unwrap_or("?"),
                col.max_sample.as_deref().unwrap_or("?")
            ));
        }
        lines.push(line);
    }
    if let Some(ref geo) = s.geo_metadata
        && let Ok(gs) = serde_json::to_string(geo)
    {
        lines.push(format!("  geo: {}", truncate_utf8_chars(&gs, 320)));
    }
    clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET)
}

// ============================================================================
// Read-Only SQLite Inspector (`sqlite3 -readonly` + Pure-Rust B-tree Fallback)
// ============================================================================

pub fn validate_readonly_sql(raw: &str) -> Result<String> {
    let stripped = strip_sql_comments(raw);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(invalid("SQL query must not be empty").into());
    }
    // Reject embedded semicolons outside string literals.
    let mut in_single = false;
    let mut in_double = false;
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut tokens: Vec<String> = Vec::new();
    let mut cur_tok = String::new();

    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                if !cur_tok.is_empty() {
                    tokens.push(std::mem::take(&mut cur_tok));
                }
                in_single = true;
            }
            b'"' => {
                if !cur_tok.is_empty() {
                    tokens.push(std::mem::take(&mut cur_tok));
                }
                in_double = true;
            }
            b';' => {
                return Err(
                    invalid("multiple SQL statements (`;`) are forbidden in probe mode").into(),
                );
            }
            c if c.is_ascii_alphanumeric() || c == b'_' => {
                cur_tok.push((c as char).to_ascii_uppercase());
            }
            _ => {
                if !cur_tok.is_empty() {
                    tokens.push(std::mem::take(&mut cur_tok));
                }
            }
        }
        i += 1;
    }
    if in_single || in_double {
        return Err(invalid("unterminated quote in SQL query").into());
    }
    if !cur_tok.is_empty() {
        tokens.push(cur_tok);
    }
    let Some(first) = tokens.first() else {
        return Err(invalid("SQL query contains no keywords").into());
    };
    if !matches!(first.as_str(), "SELECT" | "WITH" | "EXPLAIN") {
        return Err(invalid(format!(
            "only read-only SELECT / WITH / EXPLAIN queries are permitted (got `{first}`)"
        ))
        .into());
    }
    for tok in &tokens {
        if matches!(
            tok.as_str(),
            "INSERT"
                | "UPDATE"
                | "DELETE"
                | "DROP"
                | "ALTER"
                | "CREATE"
                | "REPLACE"
                | "ATTACH"
                | "DETACH"
                | "PRAGMA"
                | "VACUUM"
                | "REINDEX"
                | "BEGIN"
                | "COMMIT"
                | "ROLLBACK"
                | "SAVEPOINT"
                | "LOAD_EXTENSION"
                | "WRITEFILE"
                | "READFILE"
                | "EDIT"
                | "FTS3_TOKENIZER"
        ) {
            return Err(invalid(format!(
                "mutating or administrative SQL keyword `{tok}` is forbidden in probe mode"
            ))
            .into());
        }
    }
    Ok(trimmed.to_string())
}

fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_sq = false;
    let mut in_dq = false;
    while i < bytes.len() {
        if !in_sq && !in_dq && i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        if !in_sq && !in_dq && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or(' ');
        if ch == '\'' && !in_dq {
            in_sq = !in_sq;
        } else if ch == '"' && !in_sq {
            in_dq = !in_dq;
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn quote_sqlite_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn inspect_sqlite(path: &Path, args: &TableProbeArgs) -> Result<SqliteSummary> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 100];
    let n = file.read(&mut header)?;
    if n < 100 || &header[0..16] != b"SQLite format 3\0" {
        return Err(invalid(format!("{} is not a valid SQLite 3 database", path.display())).into());
    }
    let raw_ps = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if raw_ps == 1 { 65_536 } else { raw_ps as u32 };
    let wal_mode = header[18] == 2 || header[19] == 2;

    let validated_sql = args
        .sql
        .as_deref()
        .map(validate_readonly_sql)
        .transpose()?;

    if let Ok(summary) =
        inspect_sqlite_via_cli(path, page_size, wal_mode, args, validated_sql.as_deref())
    {
        return Ok(summary);
    }

    if validated_sql.is_some() {
        return Err(invalid(
            "`sqlite3` CLI is not installed on PATH to execute arbitrary `--sql` queries",
        )
        .into());
    }
    inspect_sqlite_pure_rust(path, page_size, wal_mode, args.table.as_deref())
}

fn sqlite_cli_json_query(db_path: &Path, sql: &str) -> Result<Vec<Value>> {
    let out = Command::new("sqlite3")
        .arg("-safe")
        .arg("-readonly")
        .arg("-json")
        .args(["-cmd", ".timeout 2000"])
        .arg("--")
        .arg(db_path)
        .arg(sql)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| invalid(format!("sqlite3 spawn failed: {e}")))?;

    if !out.status.success() {
        return Err(invalid(format!(
            "sqlite3 error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
        .into());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Vec<Value> = serde_json::from_str(trimmed)
        .map_err(|e| invalid(format!("sqlite3 JSON parse failed: {e}")))?;
    Ok(parsed)
}

fn quote_sqlite_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn inspect_sqlite_via_cli(
    path: &Path,
    page_size: u32,
    wal_mode: bool,
    args: &TableProbeArgs,
    validated_sql: Option<&str>,
) -> Result<SqliteSummary> {
    let max_samples = args.samples.min(20);
    let mut wal_os = path.as_os_str().to_os_string();
    wal_os.push("-wal");
    let has_active_wal = std::fs::metadata(wal_os).is_ok_and(|m| m.len() > 32);
    // Fast path: when no uncheckpointed WAL frames exist, discover table names in <0.1ms via pure-Rust B-tree for 1-spawn execution!
    let mut tables = if !has_active_wal {
        inspect_sqlite_pure_rust(path, page_size, wal_mode, args.table.as_deref())
            .map(|s| s.tables)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut need_cli_schema = tables.is_empty();
    if !need_cli_schema && validated_sql.is_none() {
        let mut batch_sql = String::from(
            "SELECT json_object('_master_cnt', (SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'));\n",
        );
        for (tbl_idx, t) in tables.iter().enumerate() {
            let q_ident = quote_sqlite_ident(&t.name);
            let q_lit = quote_sqlite_string(&t.name);
            let cols_expr = format!(
                "(SELECT json_group_array(CASE WHEN type='' THEN name ELSE name || ':' || type END) FROM pragma_table_info({q_lit}))"
            );
            let sample_expr = if max_samples > 0 && tbl_idx < 8 && !t.columns.is_empty() {
                let mut pairs = Vec::new();
                for col_spec in t.columns.iter().take(24) {
                    let raw_col = col_spec.split(':').next().unwrap_or(col_spec);
                    pairs.push(format!(
                        "{}, {}",
                        quote_sqlite_string(raw_col),
                        quote_sqlite_ident(raw_col)
                    ));
                }
                format!(
                    "(SELECT json_group_array(json(j)) FROM (SELECT json_object({}) AS j FROM {q_ident} LIMIT {max_samples}))",
                    pairs.join(", ")
                )
            } else {
                "'[]'".to_string()
            };
            batch_sql.push_str(&format!(
                "SELECT json_object('tbl', {q_lit}, 'cols', json({cols_expr}), 'cnt', (SELECT COUNT(*) FROM {q_ident}), 'samples', json({sample_expr}));\n"
            ));
        }

        let out = Command::new("sqlite3")
            .args(["-safe", "-readonly", "-list", "-cmd", ".timeout 2000", "--"])
            .arg(path)
            .arg(&batch_sql)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| invalid(format!("sqlite3 spawn failed: {e}")))?;

        let mut master_cnt: Option<usize> = None;
        let mut applied = 0usize;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(obj) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if let Some(mc) = obj.get("_master_cnt").and_then(Value::as_u64) {
                master_cnt = Some(mc as usize);
                continue;
            }
            if let Some(tn) = obj.get("tbl").and_then(Value::as_str)
                && let Some(t) = tables.iter_mut().find(|t| t.name == tn)
            {
                let cols: Vec<String> = obj
                    .get("cols")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if !cols.is_empty() {
                    t.columns = cols;
                }
                t.row_count = obj.get("cnt").and_then(Value::as_i64);
                t.samples = obj
                    .get("samples")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(|v| compact_sample_value(v, 0)).collect())
                    .unwrap_or_default();
                applied += 1;
            }
        }
        // Only fall back to 2-step sqlite_master discovery if WAL contains uncheckpointed DDL
        if args.table.is_none()
            && let Some(mc) = master_cnt
            && mc.min(40) != tables.len()
        {
            need_cli_schema = true;
        } else if applied > 0 {
            return Ok(SqliteSummary {
                source: path.display().to_string(),
                format: "sqlite",
                engine: "sqlite3-readonly",
                page_size,
                wal_mode,
                tables,
                query_rows: None,
            });
        } else if !out.status.success() {
            need_cli_schema = true;
        }
    }

    if need_cli_schema || validated_sql.is_some() {
        tables.clear();
        let schema_rows = sqlite_cli_json_query(
            path,
            "SELECT name, type, sql FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 40",
        )?;
        for row in schema_rows {
            let Some(name) = row.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(ref want) = args.table
                && !name.eq_ignore_ascii_case(want)
            {
                continue;
            }
            let kind = row
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("table")
                .to_string();
            let columns =
                parse_columns_from_ddl(row.get("sql").and_then(Value::as_str).unwrap_or(""));
            tables.push(SqliteTableInfo {
                name: name.to_string(),
                kind,
                row_count: None,
                columns,
                samples: Vec::new(),
            });
        }
    }

    if !tables.is_empty() && tables.iter().all(|t| t.row_count.is_none()) {
        let mut batch_sql = String::new();
        for (tbl_idx, t) in tables.iter().enumerate() {
            let q_ident = quote_sqlite_ident(&t.name);
            let q_lit = quote_sqlite_string(&t.name);
            let cols_expr = format!(
                "(SELECT json_group_array(CASE WHEN type='' THEN name ELSE name || ':' || type END) FROM pragma_table_info({q_lit}))"
            );
            let sample_expr = if max_samples > 0 && validated_sql.is_none() && tbl_idx < 8 && !t.columns.is_empty() {
                let mut pairs = Vec::new();
                for col_spec in t.columns.iter().take(24) {
                    let raw_col = col_spec.split(':').next().unwrap_or(col_spec);
                    pairs.push(format!("{}, {}", quote_sqlite_string(raw_col), quote_sqlite_ident(raw_col)));
                }
                format!(
                    "(SELECT json_group_array(json(j)) FROM (SELECT json_object({}) AS j FROM {q_ident} LIMIT {max_samples}))",
                    pairs.join(", ")
                )
            } else {
                "'[]'".to_string()
            };
            batch_sql.push_str(&format!(
                "SELECT json_object('tbl', {q_lit}, 'cols', json({cols_expr}), 'cnt', (SELECT COUNT(*) FROM {q_ident}), 'samples', json({sample_expr}));\n"
            ));
        }

        if let Ok(out) = Command::new("sqlite3")
            .args(["-safe", "-readonly", "-list", "-cmd", ".timeout 2000", "--"])
            .arg(path)
            .arg(&batch_sql)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
        {
            struct TblBatchStat {
                cols: Vec<String>,
                cnt: Option<i64>,
                samples: Vec<Value>,
            }
            let mut stats_by_name: BTreeMap<String, TblBatchStat> = BTreeMap::new();
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(obj) = serde_json::from_str::<Value>(line.trim())
                    && let Some(tn) = obj.get("tbl").and_then(Value::as_str)
                {
                    let cols = obj
                        .get("cols")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let cnt = obj.get("cnt").and_then(Value::as_i64);
                    let samples = obj
                        .get("samples")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().map(|v| compact_sample_value(v, 0)).collect())
                        .unwrap_or_default();
                    stats_by_name.insert(
                        tn.to_string(),
                        TblBatchStat {
                            cols,
                            cnt,
                            samples,
                        },
                    );
                }
            }
            for t in &mut tables {
                if let Some(st) = stats_by_name.remove(&t.name) {
                    if !st.cols.is_empty() {
                        t.columns = st.cols;
                    }
                    t.row_count = st.cnt;
                    t.samples = st.samples;
                }
            }
        }
    }

    let query_rows = if let Some(q) = validated_sql {
        let rows = sqlite_cli_json_query(path, q)?;
        Some(
            rows.into_iter()
                .take(50)
                .map(|v| compact_sample_value(&v, 0))
                .collect(),
        )
    } else {
        None
    };

    Ok(SqliteSummary {
        source: path.display().to_string(),
        format: "sqlite",
        engine: "sqlite3-readonly",
        page_size,
        wal_mode,
        tables,
        query_rows,
    })
}

/// Pure-Rust SQLite leaf (`0x0D`) & interior (`0x05`) B-tree `sqlite_schema` reader when `sqlite3` CLI is unavailable.
fn inspect_sqlite_pure_rust(
    path: &Path,
    page_size: u32,
    wal_mode: bool,
    table_filter: Option<&str>,
) -> Result<SqliteSummary> {
    let mut f = File::open(path)?;
    let mut tables = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let mut stack = vec![1u32];

    while let Some(page_num) = stack.pop() {
        if page_num == 0 || visited.len() >= 256 || !visited.insert(page_num) {
            continue;
        }
        let page_offset = ((page_num - 1) as u64).saturating_mul(page_size as u64);
        if f.seek(SeekFrom::Start(page_offset)).is_err() {
            continue;
        }
        let mut page = vec![0u8; page_size as usize];
        let n = f.read(&mut page)?;
        page.truncate(n);
        let hdr_off = if page_num == 1 { 100usize } else { 0usize };
        if page.len() < hdr_off + 8 {
            continue;
        }
        let btree_type = page[hdr_off];
        let cell_count = u16::from_be_bytes([page[hdr_off + 3], page[hdr_off + 4]]) as usize;
        if btree_type == 0x05 {
            if page.len() < hdr_off + 12 {
                continue;
            }
            let rightmost = u32::from_be_bytes([
                page[hdr_off + 8],
                page[hdr_off + 9],
                page[hdr_off + 10],
                page[hdr_off + 11],
            ]);
            stack.push(rightmost);
            let ptr_base = hdr_off + 12;
            for idx in (0..cell_count).rev() {
                let off_pos = ptr_base + idx * 2;
                if off_pos + 2 > page.len() {
                    continue;
                }
                let cell_off = u16::from_be_bytes([page[off_pos], page[off_pos + 1]]) as usize;
                if cell_off + 4 <= page.len() {
                    let left_child = u32::from_be_bytes([
                        page[cell_off],
                        page[cell_off + 1],
                        page[cell_off + 2],
                        page[cell_off + 3],
                    ]);
                    stack.push(left_child);
                }
            }
        } else if btree_type == 0x0D {
            let ptr_base = hdr_off + 8;
            for idx in 0..cell_count {
                let off_pos = ptr_base + idx * 2;
                if off_pos + 2 > page.len() {
                    break;
                }
                let cell_off = u16::from_be_bytes([page[off_pos], page[off_pos + 1]]) as usize;
                if let Some((kind, name, ddl)) = parse_sqlite_schema_cell(&page, cell_off) {
                    if !matches!(kind.as_str(), "table" | "view") || name.starts_with("sqlite_") {
                        continue;
                    }
                    if let Some(want) = table_filter
                        && !name.eq_ignore_ascii_case(want)
                    {
                        continue;
                    }
                    tables.push(SqliteTableInfo {
                        name,
                        kind,
                        row_count: None,
                        columns: parse_columns_from_ddl(&ddl),
                        samples: Vec::new(),
                    });
                }
            }
        }
    }

    Ok(SqliteSummary {
        source: path.display().to_string(),
        format: "sqlite",
        engine: "pure-rust-btree",
        page_size,
        wal_mode,
        tables,
        query_rows: None,
    })
}

fn read_sqlite_varint(buf: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *buf.get(start + i)?;
        if i == 8 {
            v = (v << 8) | (b as u64);
            return Some((v, 9));
        }
        v = (v << 7) | ((b & 0x7F) as u64);
        if (b & 0x80) == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn serial_type_len(st: u64) -> usize {
    match st {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        n if n >= 12 && n % 2 == 0 => ((n - 12) / 2) as usize,
        n if n >= 13 && n % 2 == 1 => ((n - 13) / 2) as usize,
        _ => 0,
    }
}

fn parse_sqlite_schema_cell(page: &[u8], offset: usize) -> Option<(String, String, String)> {
    let (payload_len, n1) = read_sqlite_varint(page, offset)?;
    let (_rowid, n2) = read_sqlite_varint(page, offset + n1)?;
    let rec_start = offset + n1 + n2;
    let rec_end = rec_start.checked_add(payload_len as usize)?.min(page.len());
    let rec = page.get(rec_start..rec_end)?;
    let (hdr_len, nh) = read_sqlite_varint(rec, 0)?;
    let hdr_end = (hdr_len as usize).min(rec.len());
    let mut hpos = nh;
    let mut stypes = Vec::new();
    while hpos < hdr_end && stypes.len() < 5 {
        let (st, ns) = read_sqlite_varint(rec, hpos)?;
        stypes.push(st);
        hpos += ns;
    }
    if stypes.len() < 5 {
        return None;
    }
    let mut bpos = hdr_end;
    let mut texts = Vec::with_capacity(5);
    for (col_idx, st) in stypes.into_iter().enumerate() {
        let len = serial_type_len(st);
        let desired_end = bpos.checked_add(len)?;
        let end = if col_idx == 4 && bpos <= rec.len() {
            desired_end.min(rec.len())
        } else {
            desired_end
        };
        let slice = rec.get(bpos..end)?;
        texts.push(String::from_utf8_lossy(slice).into_owned());
        bpos = end;
    }
    Some((texts[0].clone(), texts[1].clone(), texts[4].clone()))
}

fn parse_columns_from_ddl(ddl: &str) -> Vec<String> {
    let Some(open) = ddl.find('(') else {
        return Vec::new();
    };
    let Some(close) = ddl.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    let body = &ddl[open + 1..close];
    let mut cols = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (idx, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                if let Some(c) = extract_col_def(&body[start..idx]) {
                    cols.push(c);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if let Some(c) = extract_col_def(&body[start..]) {
        cols.push(c);
    }
    cols
}

fn extract_col_def(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?.trim_matches('"').trim_matches('`').trim_matches('[').trim_matches(']');
    let upper = first.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK" | "CONSTRAINT"
    ) {
        return None;
    }
    let second = parts.next().unwrap_or("");
    let sec_upper = second.to_ascii_uppercase();
    if second.is_empty()
        || matches!(
            sec_upper.as_str(),
            "PRIMARY" | "NOT" | "NULL" | "DEFAULT" | "UNIQUE" | "REFERENCES" | "CHECK"
        )
    {
        Some(first.to_string())
    } else {
        Some(format!("{first}:{second}"))
    }
}

fn render_sqlite_text(s: &SqliteSummary) -> String {
    let mut lines = vec![format!(
        "[{}] format=sqlite engine={} page_size={} wal={} tables={}",
        s.source,
        s.engine,
        s.page_size,
        s.wal_mode,
        s.tables.len()
    )];
    for t in &s.tables {
        let rc = t
            .row_count
            .map(|n| format!(" rows={n}"))
            .unwrap_or_default();
        lines.push(format!(
            "  table `{}` ({}{}): [{}]",
            t.name,
            t.kind,
            rc,
            t.columns.join(", ")
        ));
        for (i, sample) in t.samples.iter().enumerate() {
            if let Ok(txt) = serde_json::to_string(sample) {
                lines.push(format!("    sample[{i}]: {}", truncate_utf8_chars(&txt, 420)));
            }
        }
    }
    if let Some(ref qrows) = s.query_rows {
        lines.push(format!("  query_rows({}):", qrows.len()));
        for (i, r) in qrows.iter().enumerate() {
            if let Ok(txt) = serde_json::to_string(r) {
                lines.push(format!("    [{i}]: {}", truncate_utf8_chars(&txt, 500)));
            }
        }
    }
    clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET)
}

// ============================================================================
// Streaming RFC-4180 CSV / TSV Inspector
// ============================================================================

fn parse_delimited_line(line: &str, delim: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    let _ = chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(ch);
            }
        } else if ch == '"' && cur.is_empty() {
            in_quotes = true;
        } else if ch == delim {
            fields.push(std::mem::take(&mut cur));
        } else if ch != '\r' {
            cur.push(ch);
        }
    }
    fields.push(cur);
    fields
}

fn inspect_csv(path: &Path, args: &TableProbeArgs) -> Result<CsvSummary> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let f = File::open(path)?;
    let mut reader = BufReader::new(f);
    let mut hdr_buf = Vec::with_capacity(1024);
    if super::read_bounded_line(&mut reader, &mut hdr_buf, 1024 * 1024)? == 0 {
        return Err(invalid(format!("empty delimited file: {}", path.display())).into());
    }
    let header_line = String::from_utf8_lossy(&hdr_buf);
    let header_clean = header_line.trim_start_matches('\u{feff}');
    let delim = if ext == "tsv" || ext == "tab" || (header_clean.contains('\t') && !header_clean.contains(',')) {
        '\t'
    } else {
        ','
    };

    let headers: Vec<String> = parse_delimited_line(header_clean.trim_end_matches(['\r', '\n']), delim)
        .into_iter()
        .enumerate()
        .map(|(i, h)| {
            if h.trim().is_empty() {
                format!("_col_{i}")
            } else {
                h
            }
        })
        .collect();
    let mut non_empty = vec![0usize; headers.len()];
    let predicate = args
        .where_expr
        .as_deref()
        .map(Predicate::parse)
        .transpose()?;
    let has_filter = predicate.is_some();

    let mut total_rows = 0usize;
    let mut matched_rows = 0usize;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples = Vec::new();
    let mut line_buf = Vec::with_capacity(2048);
    let mut total_bytes = 0usize;

    for _line_idx in 0..MAX_CSV_LINES {
        let n = super::read_bounded_line(&mut reader, &mut line_buf, 4 * 1024 * 1024)?;
        if n == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(n);
        if total_bytes > 256 * 1024 * 1024 {
            break;
        }
        let mut record_str = String::from_utf8_lossy(&line_buf).into_owned();
        // RFC 4180 multiline quoted field assembly (join up to 16 physical lines while quotes are unbalanced)
        let mut cont = 0usize;
        while record_str.bytes().filter(|&b| b == b'"').count() % 2 == 1
            && cont < 16
            && record_str.len() < 4 * 1024 * 1024
        {
            let next_n = super::read_bounded_line(&mut reader, &mut line_buf, 1024 * 1024)?;
            if next_n == 0 {
                break;
            }
            total_bytes = total_bytes.saturating_add(next_n);
            record_str.push_str(&String::from_utf8_lossy(&line_buf));
            cont += 1;
        }
        let line = record_str.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        total_rows += 1;
        let vals = parse_delimited_line(line, delim);
        let mut obj = Map::new();
        for (i, h) in headers.iter().enumerate() {
            let v = vals.get(i).map(String::as_str).unwrap_or("");
            if !v.is_empty() {
                non_empty[i] += 1;
            }
            if super::is_sensitive_key_name(h) {
                obj.insert(h.clone(), json!("<REDACTED>"));
            } else {
                obj.insert(h.clone(), json!(v));
            }
        }
        let row_val = Value::Object(obj);
        if let Some(ref pred) = predicate {
            let target = row_val.get(pred.field());
            if !pred.matches_value(target) {
                continue;
            }
        }
        if let Some(ref cb) = args.count_by {
            let key = row_val
                .get(cb)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("<empty>");
            *counts.entry(truncate_utf8_chars(key, 60)).or_default() += 1;
        }
        matched_rows += 1;
        if samples.len() < args.samples.min(20) {
            samples.push(compact_sample_value(&row_val, 0));
        }
    }

    let mut sorted_counts: Vec<(String, usize)> = counts.into_iter().collect();
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted_counts.truncate(20);

    let columns = headers
        .into_iter()
        .zip(non_empty)
        .map(|(name, ne)| CsvColInfo {
            name,
            non_empty: ne,
        })
        .collect();

    Ok(CsvSummary {
        source: path.display().to_string(),
        format: if delim == '\t' { "tsv" } else { "csv" },
        delimiter: if delim == '\t' { "tab" } else { "comma" },
        rows: total_rows,
        matched_rows: has_filter.then_some(matched_rows),
        columns,
        counts_by: sorted_counts,
        samples,
    })
}

fn render_csv_text(s: &CsvSummary) -> String {
    let mut head = format!(
        "[{}] format={} rows={} columns={}",
        s.source,
        s.format,
        s.rows,
        s.columns.len()
    );
    if let Some(m) = s.matched_rows {
        head.push_str(&format!(" matched={m}"));
    }
    let mut lines = vec![head];
    let cols: Vec<String> = s
        .columns
        .iter()
        .take(30)
        .map(|c| format!("{}({})", c.name, c.non_empty))
        .collect();
    lines.push(format!("  columns: {}", cols.join(", ")));
    if !s.counts_by.is_empty() {
        let cs: Vec<String> = s
            .counts_by
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        lines.push(format!("  counts_by: {}", cs.join(", ")));
    }
    for (i, sample) in s.samples.iter().enumerate() {
        if let Ok(txt) = serde_json::to_string(sample) {
            lines.push(format!("  sample[{i}]: {}", truncate_utf8_chars(&txt, 480)));
        }
    }
    clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET)
}

pub fn run(args: &TableProbeArgs) -> Result<String> {
    let bbox = args.bbox.as_deref().map(BBox::parse).transpose()?;

    if args.source.starts_with("http://") || args.source.starts_with("https://") {
        let summary = inspect_remote_parquet(&args.source, args.timeout.clamp(1, 60), bbox.as_ref())?;
        return if args.json {
            clamp_json_budget(serde_json::to_value(&summary)?)
        } else {
            Ok(render_parquet_text(&summary))
        };
    }

    let path = Path::new(&args.source);
    let mut f = File::open(path)
        .map_err(|e| invalid(format!("cannot open table file {}: {e}", path.display())))?;
    let mut magic = [0u8; 16];
    let n = f.read(&mut magic)?;

    if n >= 16 && &magic == b"SQLite format 3\0" {
        let summary = inspect_sqlite(path, args)?;
        return if args.json {
            clamp_json_budget(serde_json::to_value(&summary)?)
        } else {
            Ok(render_sqlite_text(&summary))
        };
    }

    if n >= 4 && (&magic[0..4] == b"PAR1" || &magic[0..4] == b"PARE") {
        let summary = inspect_local_parquet(path, bbox.as_ref())?;
        return if args.json {
            clamp_json_budget(serde_json::to_value(&summary)?)
        } else {
            Ok(render_parquet_text(&summary))
        };
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "csv" | "tsv" | "tab") {
        let summary = inspect_csv(path, args)?;
        return if args.json {
            clamp_json_budget(serde_json::to_value(&summary)?)
        } else {
            Ok(render_csv_text(&summary))
        };
    }

    Err(invalid(format!(
        "unsupported table format for {} (expected SQLite, Parquet, CSV, or TSV)",
        path.display()
    ))
    .into())
}
