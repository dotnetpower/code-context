use super::{
    BBox, JsonProbeArgs, MAX_SAMPLE_STRING_CHARS, Predicate, clamp_json_budget, clamp_utf8_bytes,
    http_probe, truncate_utf8_chars, value_as_comparable_string, value_as_f64,
};
use crate::inline_budget::INLINE_BUDGET;
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const MAX_JSON_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_JSONL_LINES: usize = 500_000;

#[derive(Clone, Debug, Serialize)]
pub struct JsonSummary {
    pub source: String,
    pub format: &'static str,
    pub root_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_total_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_keys: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub item_fields: Vec<FieldSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub counts_by: Vec<(String, usize)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FieldSummary {
    pub name: String,
    pub kind: String,
    pub present: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum PathStep {
    Key(String),
    Index(usize),
    Slice(usize, Option<usize>),
    IterAll,
}

fn parse_selector_path(raw: &str) -> Result<Vec<PathStep>> {
    let trimmed = raw.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut steps = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            i += 1;
            continue;
        }
        if bytes[i] == b'[' {
            let close = trimmed[i + 1..]
                .find(']')
                .map(|off| i + 1 + off)
                .ok_or_else(|| invalid(format!("unclosed `[` in selector path: {raw}")))?;
            let inside = trimmed[i + 1..close].trim();
            if inside.is_empty() || inside == "*" {
                steps.push(PathStep::IterAll);
            } else if let Some((start_s, end_s)) = inside.split_once(':') {
                let start = if start_s.trim().is_empty() {
                    0
                } else {
                    start_s
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| invalid(format!("invalid slice start in `{raw}`")))?
                };
                let end = if end_s.trim().is_empty() {
                    None
                } else {
                    Some(
                        end_s
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| invalid(format!("invalid slice end in `{raw}`")))?,
                    )
                };
                steps.push(PathStep::Slice(start, end));
            } else if let Ok(idx) = inside.parse::<usize>() {
                steps.push(PathStep::Index(idx));
            } else {
                let unquoted = inside.trim_matches('"').trim_matches('\'');
                steps.push(PathStep::Key(unquoted.to_string()));
            }
            i = close + 1;
        } else {
            let mut end = i;
            while end < bytes.len() && bytes[end] != b'.' && bytes[end] != b'[' {
                end += 1;
            }
            let key = trimmed[i..end].trim();
            if !key.is_empty() {
                steps.push(PathStep::Key(key.to_string()));
            }
            i = end;
        }
    }
    Ok(steps)
}

fn evaluate_steps<'a>(roots: Vec<&'a Value>, steps: &[PathStep]) -> Vec<&'a Value> {
    let mut current = roots;
    for step in steps {
        let mut next = Vec::new();
        for val in current {
            match step {
                PathStep::Key(k) => {
                    if let Value::Object(map) = val
                        && let Some(child) = map.get(k)
                    {
                        next.push(child);
                    }
                }
                PathStep::Index(idx) => {
                    if let Value::Array(arr) = val
                        && let Some(child) = arr.get(*idx)
                    {
                        next.push(child);
                    }
                }
                PathStep::Slice(start, end) => {
                    if let Value::Array(arr) = val {
                        let s = (*start).min(arr.len());
                        let e = end.unwrap_or(arr.len()).min(arr.len());
                        if s < e {
                            next.extend(arr[s..e].iter());
                        }
                    }
                }
                PathStep::IterAll => match val {
                    Value::Array(arr) => next.extend(arr.iter()),
                    Value::Object(map) => next.extend(map.values()),
                    _ => {}
                },
            }
        }
        current = next;
    }
    current
}

pub fn lookup_dotted<'a>(val: &'a Value, path: &str) -> Option<&'a Value> {
    let steps = parse_selector_path(path).ok()?;
    evaluate_steps(vec![val], &steps).into_iter().next()
}

/// Extracts a bounding box from RFC 7946 `bbox` (4- or 6-element), Overture/GeoParquet object `bbox`, GeoJSON `geometry`, or geographic lon/lat fields.
pub fn extract_value_bbox(val: &Value) -> Option<BBox> {
    if let Value::Object(map) = val {
        if let Some(bbox_val) = map.get("bbox") {
            match bbox_val {
                Value::Array(arr) => {
                    if let Some(b) = parse_bbox_array(arr) {
                        return Some(b);
                    }
                }
                Value::Object(bmap) => {
                    if let Some(b) = parse_bbox_object(bmap) {
                        return Some(b);
                    }
                }
                _ => {}
            }
        }
        if let Some(b) = parse_bbox_object(map) {
            return Some(b);
        }
        if let Some(geom) = map.get("geometry")
            && let Some(b) = extract_geometry_bbox(geom)
        {
            return Some(b);
        }
        if let Some(props) = map.get("properties")
            && let Some(b) = extract_point_fields_bbox(props)
        {
            return Some(b);
        }
        if let Some(b) = extract_point_fields_bbox(val) {
            return Some(b);
        }
    } else if let Value::Array(arr) = val {
        return parse_bbox_array(arr);
    }
    None
}

fn parse_bbox_object(map: &Map<String, Value>) -> Option<BBox> {
    let pairs = [
        (["xmin", "ymin", "xmax", "ymax"]),
        (["min_lon", "min_lat", "max_lon", "max_lat"]),
        (["minLon", "minLat", "maxLon", "maxLat"]),
        (["west", "south", "east", "north"]),
    ];
    for [k0, k1, k2, k3] in pairs {
        if let (Some(x0), Some(y0), Some(x1), Some(y1)) = (
            value_as_f64(map.get(k0)),
            value_as_f64(map.get(k1)),
            value_as_f64(map.get(k2)),
            value_as_f64(map.get(k3)),
        ) {
            return Some(BBox {
                min_lon: x0.min(x1),
                min_lat: y0.min(y1),
                max_lon: x0.max(x1),
                max_lat: y0.max(y1),
            });
        }
    }
    None
}

fn parse_bbox_array(arr: &[Value]) -> Option<BBox> {
    let nums: Option<Vec<f64>> = arr.iter().map(|v| v.as_f64()).collect();
    let nums = nums?;
    if nums.iter().any(|n| !n.is_finite()) {
        return None;
    }
    match nums.len() {
        4 => Some(BBox {
            min_lon: nums[0].min(nums[2]),
            min_lat: nums[1].min(nums[3]),
            max_lon: nums[0].max(nums[2]),
            max_lat: nums[1].max(nums[3]),
        }),
        6 => Some(BBox {
            min_lon: nums[0].min(nums[3]),
            min_lat: nums[1].min(nums[4]),
            max_lon: nums[0].max(nums[3]),
            max_lat: nums[1].max(nums[4]),
        }),
        _ => None,
    }
}

fn extract_geometry_bbox(geom: &Value) -> Option<BBox> {
    let mut bounds: Option<BBox> = None;
    if let Some(coords) = geom.get("coordinates") {
        collect_coord_bounds(coords, &mut bounds);
    } else if let Some(Value::Array(geoms)) = geom.get("geometries") {
        for child in geoms {
            if let Some(cb) = extract_geometry_bbox(child) {
                match bounds {
                    Some(ref mut b) => {
                        b.min_lon = b.min_lon.min(cb.min_lon);
                        b.min_lat = b.min_lat.min(cb.min_lat);
                        b.max_lon = b.max_lon.max(cb.max_lon);
                        b.max_lat = b.max_lat.max(cb.max_lat);
                    }
                    None => bounds = Some(cb),
                }
            }
        }
    }
    bounds
}

fn collect_coord_bounds(val: &Value, acc: &mut Option<BBox>) {
    let Value::Array(arr) = val else {
        return;
    };
    if arr.len() >= 2
        && let (Some(lon), Some(lat)) = (arr[0].as_f64(), arr[1].as_f64())
        && lon.is_finite()
        && lat.is_finite()
    {
        match acc {
            Some(b) => {
                b.min_lon = b.min_lon.min(lon);
                b.min_lat = b.min_lat.min(lat);
                b.max_lon = b.max_lon.max(lon);
                b.max_lat = b.max_lat.max(lat);
            }
            None => {
                *acc = Some(BBox {
                    min_lon: lon,
                    min_lat: lat,
                    max_lon: lon,
                    max_lat: lat,
                });
            }
        }
        return;
    }
    for child in arr {
        collect_coord_bounds(child, acc);
    }
}

fn extract_point_fields_bbox(val: &Value) -> Option<BBox> {
    let Value::Object(map) = val else {
        return None;
    };
    let lon_keys = [
        "lon",
        "lng",
        "longitude",
        "long",
        "wgs84_lon",
        "REFINE_WGS84_LOGT",
        "refine_wgs84_logt",
    ];
    let lat_keys = [
        "lat",
        "latitude",
        "wgs84_lat",
        "REFINE_WGS84_LAT",
        "refine_wgs84_lat",
    ];
    let lon = lon_keys
        .iter()
        .find_map(|k| value_as_f64(map.get(*k)))
        .filter(|v| (-180.0..=180.0).contains(v))?;
    let lat = lat_keys
        .iter()
        .find_map(|k| value_as_f64(map.get(*k)))
        .filter(|v| (-90.0..=90.0).contains(v))?;
    Some(BBox {
        min_lon: lon,
        min_lat: lat,
        max_lon: lon,
        max_lat: lat,
    })
}

fn value_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Compacts a JSON value recursively so sample rows never explode inline token budgets.
pub fn compact_sample_value(val: &Value, depth: usize) -> Value {
    if depth > 4 {
        return match val {
            Value::Array(a) => json!(format!("<array len={}>", a.len())),
            Value::Object(m) => json!(format!("<object keys={}>", m.len())),
            Value::String(s) => json!(truncate_utf8_chars(s, MAX_SAMPLE_STRING_CHARS)),
            other => other.clone(),
        };
    }
    match val {
        Value::String(s) => Value::String(truncate_utf8_chars(s, MAX_SAMPLE_STRING_CHARS)),
        Value::Array(arr) => {
            if arr.len() > 6 && arr.iter().all(Value::is_number) {
                let head: Vec<Value> = arr.iter().take(4).cloned().collect();
                json!({ "_numeric_array_len": arr.len(), "head": head })
            } else {
                let mut out: Vec<Value> = arr
                    .iter()
                    .take(4)
                    .map(|item| compact_sample_value(item, depth + 1))
                    .collect();
                if arr.len() > 4 {
                    out.push(json!(format!("…[+{} more items]", arr.len() - 4)));
                }
                Value::Array(out)
            }
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx >= 24 {
                    out.insert("_omitted_keys".into(), json!(map.len() - 24));
                    break;
                }
                if super::is_sensitive_key_name(k) {
                    out.insert(k.clone(), json!("<REDACTED>"));
                } else {
                    out.insert(k.clone(), compact_sample_value(v, depth + 1));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Detects total counts and API result status envelopes across OpenAPI, Overture STAC, Hydra, and REST JSON.
fn detect_envelope_metadata<'a>(root: &'a Value) -> (Option<u64>, Option<String>, Option<&'a Vec<Value>>) {
    let mut total_count: Option<u64> = None;
    let mut status: Option<String> = None;
    let mut primary_rows: Option<&'a Vec<Value>> = None;

    let count_keys = [
        "list_total_count",
        "totalCount",
        "total_count",
        "table:row_count",
        "numberMatched",
        "totalResults",
        "total",
        "count",
        "features",
    ];

    if let Value::Object(map) = root {
        for ck in count_keys {
            if let Some(n) = map.get(ck).and_then(value_to_u64) {
                total_count = Some(n);
                break;
            }
        }
        status = extract_status_from_map(map);

        // Standard collection arrays (`features` for GeoJSON/STAC, `links` for STAC item catalogs, `items`, `results`, `rows`, `data`)
        for arr_key in [
            "features",
            "row",
            "rows",
            "items",
            "results",
            "records",
            "data",
            "documents",
            "links",
        ] {
            if let Some(Value::Array(arr)) = map.get(arr_key) {
                primary_rows = Some(arr);
                break;
            }
        }

        // Korean Gyeonggi / Data.go.kr OpenAPI envelope: `{ "<ServiceName>": [ {"head": [...]}, {"row": [...]} ] }`
        if primary_rows.is_none() && map.len() <= 3 {
            for (_svc_key, svc_val) in map {
                if let Value::Array(sections) = svc_val {
                    for sec in sections {
                        if let Some(Value::Array(heads)) = sec.get("head") {
                            for h in heads {
                                if total_count.is_none() {
                                    for ck in count_keys {
                                        if let Some(n) = h.get(ck).and_then(value_to_u64) {
                                            total_count = Some(n);
                                            break;
                                        }
                                    }
                                }
                                if status.is_none()
                                    && let Some(hmap) = h.as_object()
                                {
                                    status = extract_status_from_map(hmap);
                                }
                            }
                        }
                        if primary_rows.is_none()
                            && let Some(Value::Array(rows)) = sec.get("row")
                        {
                            primary_rows = Some(rows);
                        }
                    }
                } else if let Value::Object(inner) = svc_val {
                    // Data.go.kr `{ "response": { "header": {...}, "body": { "items": { "item": [...] }, "totalCount": N } } }`
                    if let Some(hdr) = inner.get("header").and_then(Value::as_object)
                        && status.is_none()
                    {
                        status = extract_status_from_map(hdr);
                    }
                    if let Some(body) = inner.get("body").and_then(Value::as_object) {
                        if total_count.is_none() {
                            for ck in count_keys {
                                if let Some(n) = body.get(ck).and_then(value_to_u64) {
                                    total_count = Some(n);
                                    break;
                                }
                            }
                        }
                        if let Some(Value::Array(arr)) = body.get("items") {
                            primary_rows = Some(arr);
                        } else if let Some(Value::Array(arr)) =
                            body.get("items").and_then(|it| it.get("item"))
                        {
                            primary_rows = Some(arr);
                        }
                    }
                }
            }
        }
    } else if let Value::Array(arr) = root {
        primary_rows = Some(arr);
    }

    (total_count, status, primary_rows)
}

fn value_to_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn extract_status_from_map(map: &Map<String, Value>) -> Option<String> {
    if let Some(res) = map.get("RESULT").or_else(|| map.get("result")).and_then(Value::as_object) {
        let code = res
            .get("CODE")
            .or_else(|| res.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg = res
            .get("MESSAGE")
            .or_else(|| res.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !code.is_empty() || !msg.is_empty() {
            return Some(format!("{code} {}", truncate_utf8_chars(msg, 80)).trim().to_string());
        }
    }
    let code = map
        .get("resultCode")
        .or_else(|| map.get("code"))
        .or_else(|| map.get("status"))
        .and_then(|v| value_as_comparable_string(Some(v)));
    let msg = map
        .get("resultMsg")
        .or_else(|| map.get("error"))
        .or_else(|| map.get("message"))
        .and_then(Value::as_str);
    match (code, msg) {
        (Some(c), Some(m)) => Some(format!("{c} {}", truncate_utf8_chars(m, 80))),
        (None, Some(m)) if map.contains_key("error") || map.contains_key("resultMsg") => {
            Some(truncate_utf8_chars(m, 80))
        }
        (Some(c), None) if map.contains_key("resultCode") => Some(c),
        _ => None,
    }
}

fn summarize_item_fields(items: &[&Value]) -> Vec<FieldSummary> {
    let mut kinds: BTreeMap<String, (BTreeMap<&'static str, usize>, usize)> = BTreeMap::new();
    for item in items.iter().take(200) {
        if let Value::Object(map) = item {
            for (k, v) in map {
                let entry = kinds.entry(k.clone()).or_default();
                if !v.is_null() {
                    *entry.0.entry(value_kind_name(v)).or_default() += 1;
                    entry.1 += 1;
                }
            }
        }
    }
    kinds
        .into_iter()
        .take(28)
        .map(|(name, (type_counts, present))| {
            let kind = if type_counts.is_empty() {
                "null".to_string()
            } else {
                let names: Vec<&str> = type_counts.keys().copied().collect();
                names.join("|")
            };
            FieldSummary {
                name,
                kind,
                present,
            }
        })
        .collect()
}

fn extract_item_id(val: &Value) -> Option<String> {
    let Value::Object(map) = val else {
        return None;
    };
    for k in [
        "id",
        "ID",
        "name",
        "title",
        "href",
        "BIZPLC_NM",
        "PARK_NM",
        "PBCTLT_PLC_NM",
    ] {
        if let Some(s) = map.get(k).and_then(|v| value_as_comparable_string(Some(v)))
            && !s.is_empty()
        {
            return Some(truncate_utf8_chars(&s, 64));
        }
    }
    if let Some(props) = map.get("properties").and_then(Value::as_object) {
        for k in ["id", "name", "title"] {
            if let Some(s) = props.get(k).and_then(|v| value_as_comparable_string(Some(v)))
                && !s.is_empty()
            {
                return Some(truncate_utf8_chars(&s, 64));
            }
        }
    }
    None
}

#[derive(Default)]
pub struct QueryFilters {
    pub path: Option<String>,
    pub bbox: Option<BBox>,
    pub predicate: Option<Predicate>,
    pub count_by: Option<String>,
    pub max_samples: usize,
}

pub fn summarize_parsed_value(
    source_label: &str,
    format_label: &'static str,
    root: &Value,
    filters: &QueryFilters,
) -> Result<JsonSummary> {
    let (detected_total_count, api_status, envelope_rows) = detect_envelope_metadata(root);

    let selected: Vec<&Value> = if let Some(ref raw_path) = filters.path {
        let steps = parse_selector_path(raw_path)?;
        evaluate_steps(vec![root], &steps)
    } else if let Some(rows) = envelope_rows {
        rows.iter().collect()
    } else {
        vec![root]
    };

    // Unwrap single selected array when `--path features` points to an array.
    let candidate_items: Vec<&Value> = if selected.len() == 1
        && let Value::Array(arr) = selected[0]
    {
        arr.iter().collect()
    } else {
        selected
    };

    let has_row_collection =
        envelope_rows.is_some() || matches!(root, Value::Array(_)) || filters.path.is_some();
    let item_count = if has_row_collection {
        Some(candidate_items.len())
    } else {
        None
    };

    let has_filter = filters.bbox.is_some() || filters.predicate.is_some();
    let mut matched: Vec<&Value> = Vec::new();
    let mut matched_ids: Vec<String> = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for item in &candidate_items {
        if let Some(ref qbox) = filters.bbox {
            let Some(ibox) = extract_value_bbox(item) else {
                continue;
            };
            if !qbox.intersects(&ibox) {
                continue;
            }
        }
        if let Some(ref pred) = filters.predicate {
            let target = lookup_dotted(item, pred.field());
            if !pred.matches_value(target) {
                continue;
            }
        }
        if let Some(ref group_field) = filters.count_by {
            let raw_key = lookup_dotted(item, group_field)
                .and_then(|v| value_as_comparable_string(Some(v)));
            let key = match raw_key {
                Some(_) if super::is_sensitive_field_path(group_field) => "<REDACTED>".to_string(),
                Some(v) => v,
                None => "<null>".to_string(),
            };
            *counts.entry(truncate_utf8_chars(&key, 60)).or_default() += 1;
        }
        if has_filter
            && matched_ids.len() < 15
            && let Some(id) = extract_item_id(item)
        {
            matched_ids.push(id);
        }
        matched.push(*item);
    }

    let mut sorted_counts: Vec<(String, usize)> = counts.into_iter().collect();
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted_counts.truncate(20);

    let top_keys = if let Value::Object(map) = root {
        map.keys().take(24).cloned().collect()
    } else {
        Vec::new()
    };

    let field_source = if matched.is_empty() {
        &candidate_items
    } else {
        &matched
    };
    let item_fields = summarize_item_fields(field_source);

    let samples: Vec<Value> = matched
        .iter()
        .take(filters.max_samples.min(25))
        .map(|v| compact_sample_value(v, 0))
        .collect();

    Ok(JsonSummary {
        source: source_label.to_string(),
        format: format_label,
        root_type: value_kind_name(root),
        detected_total_count,
        api_status,
        item_count,
        matched_count: has_filter.then_some(matched.len()),
        matched_ids,
        top_keys,
        item_fields,
        counts_by: sorted_counts,
        samples,
    })
}

/// Cleans Copilot host offload headers or trailing `CCTX_TRACE` footers so `/tmp/*-copilot-tool-output-*.txt` parses cleanly.
pub fn parse_json_tolerant(raw: &str) -> std::result::Result<Value, serde_json::Error> {
    let trimmed = strip_trace_footer(raw.trim().trim_start_matches('\u{feff}'));
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        return Ok(val);
    }
    // Prioritize line-initial `{` or `[` (e.g. after "Preview (first 500 chars):\n"), then inline candidates up to 4 KiB.
    let window = &trimmed[..trimmed.floor_char_boundary(trimmed.len().min(4_096))];
    let mut candidates = Vec::new();
    for (idx, ch) in window.char_indices() {
        if matches!(ch, '{' | '[') {
            let line_initial = idx == 0 || trimmed.as_bytes().get(idx.wrapping_sub(1)) == Some(&b'\n');
            candidates.push((!line_initial, idx));
        }
    }
    candidates.sort_unstable();
    for (_prio, idx) in candidates.into_iter().take(10) {
        if let Ok(val) = serde_json::from_str::<Value>(&trimmed[idx..]) {
            return Ok(val);
        }
    }
    serde_json::from_str::<Value>(trimmed)
}

fn strip_trace_footer(text: &str) -> &str {
    if let Some(pos) = text.rfind("\nCCTX_TRACE {") {
        text[..pos].trim_end()
    } else {
        text
    }
}

fn summarize_jsonl_reader<R: BufRead>(
    source_label: &str,
    mut reader: R,
    filters: &QueryFilters,
) -> Result<JsonSummary> {
    const MAX_JSONL_LINE_BYTES: usize = 8 * 1024 * 1024;
    const MAX_JSONL_STREAM_BYTES: usize = 256 * 1024 * 1024;
    let has_filter = filters.bbox.is_some() || filters.predicate.is_some();
    let steps = match filters.path {
        Some(ref p) => Some(parse_selector_path(p)?),
        None => None,
    };

    let mut total_records = 0usize;
    let mut matched_count = 0usize;
    let mut matched_ids = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut field_kinds: BTreeMap<String, (BTreeMap<&'static str, usize>, usize)> = BTreeMap::new();
    let mut samples = Vec::new();
    let mut line_buf = Vec::with_capacity(4096);
    let mut total_bytes = 0usize;

    for _line_idx in 0..MAX_JSONL_LINES {
        let n = super::read_bounded_line(&mut reader, &mut line_buf, MAX_JSONL_LINE_BYTES)?;
        if n == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(n);
        if total_bytes > MAX_JSONL_STREAM_BYTES {
            break;
        }
        let trimmed = line_buf.trim_ascii();
        if trimmed.is_empty() || trimmed.starts_with(b"CCTX_TRACE {") {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<Value>(trimmed) else {
            continue;
        };
        total_records += 1;

        let targets: Vec<&Value> = match steps {
            Some(ref s) => evaluate_steps(vec![&parsed], s),
            None => vec![&parsed],
        };

        for item in targets {
            if let Value::Object(map) = item
                && total_records <= 300
            {
                for (k, v) in map {
                    let entry = field_kinds.entry(k.clone()).or_default();
                    if !v.is_null() {
                        *entry.0.entry(value_kind_name(v)).or_default() += 1;
                        entry.1 += 1;
                    }
                }
            }
            if let Some(ref qbox) = filters.bbox {
                let Some(ibox) = extract_value_bbox(item) else {
                    continue;
                };
                if !qbox.intersects(&ibox) {
                    continue;
                }
            }
            if let Some(ref pred) = filters.predicate {
                let target = lookup_dotted(item, pred.field());
                if !pred.matches_value(target) {
                    continue;
                }
            }
            if let Some(ref group_field) = filters.count_by {
                let raw_key = lookup_dotted(item, group_field)
                    .and_then(|v| value_as_comparable_string(Some(v)));
                let key = match raw_key {
                    Some(_) if super::is_sensitive_field_path(group_field) => {
                        "<REDACTED>".to_string()
                    }
                    Some(v) => v,
                    None => "<null>".to_string(),
                };
                *counts.entry(truncate_utf8_chars(&key, 60)).or_default() += 1;
            }
            matched_count += 1;
            if has_filter
                && matched_ids.len() < 15
                && let Some(id) = extract_item_id(item)
            {
                matched_ids.push(id);
            }
            if samples.len() < filters.max_samples.min(25) {
                samples.push(compact_sample_value(item, 0));
            }
        }
    }

    if total_records == 0 {
        return Err(invalid(format!("no valid JSON or JSONL records found in {source_label}")).into());
    }

    let mut sorted_counts: Vec<(String, usize)> = counts.into_iter().collect();
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted_counts.truncate(20);

    let item_fields = field_kinds
        .into_iter()
        .take(28)
        .map(|(name, (tc, present))| {
            let names: Vec<&str> = tc.keys().copied().collect();
            FieldSummary {
                name,
                kind: if names.is_empty() {
                    "null".into()
                } else {
                    names.join("|")
                },
                present,
            }
        })
        .collect();

    Ok(JsonSummary {
        source: source_label.to_string(),
        format: "jsonl",
        root_type: "stream",
        detected_total_count: Some(total_records as u64),
        api_status: None,
        item_count: Some(total_records),
        matched_count: has_filter.then_some(matched_count),
        matched_ids,
        top_keys: Vec::new(),
        item_fields,
        counts_by: sorted_counts,
        samples,
    })
}

#[derive(Default)]
struct JsonlShardAcc {
    records: usize,
    matched: usize,
    matched_ids: Vec<String>,
    counts: BTreeMap<String, usize>,
    field_kinds: BTreeMap<String, (BTreeMap<&'static str, usize>, usize)>,
    samples: Vec<Value>,
}

fn split_newline_shards(buf: &[u8], num_shards: usize) -> Vec<&[u8]> {
    if num_shards <= 1 || buf.len() < 65_536 {
        return vec![buf];
    }
    let mut shards = Vec::with_capacity(num_shards);
    let target = buf.len() / num_shards;
    let mut start = 0usize;
    for s in 1..num_shards {
        let approx = (s * target).min(buf.len());
        if approx <= start {
            continue;
        }
        if let Some(rel_nl) = memchr::memchr(b'\n', &buf[approx..]) {
            let end = approx + rel_nl + 1;
            shards.push(&buf[start..end]);
            start = end;
        }
    }
    if start < buf.len() {
        shards.push(&buf[start..]);
    }
    shards
}

fn summarize_jsonl_bytes_parallel(
    source_label: &str,
    raw: &[u8],
    filters: &QueryFilters,
) -> Result<JsonSummary> {
    let has_filter = filters.bbox.is_some() || filters.predicate.is_some();
    let steps = match filters.path {
        Some(ref p) => Some(parse_selector_path(p)?),
        None => None,
    };
    let fast_key_needle = match (&filters.predicate, &steps) {
        (Some(pred), None) if !matches!(pred, Predicate::Ne(_, _)) => {
            let leaf = pred.field().split(['.', '[']).next().unwrap_or("");
            (!leaf.is_empty()).then(|| format!("\"{leaf}\""))
        }
        _ => None,
    };
    let finder = fast_key_needle
        .as_deref()
        .map(|pat| memchr::memmem::Finder::new(pat.as_bytes()));

    let shard_slices = split_newline_shards(raw, 8);
    let mut shard_accs: Vec<JsonlShardAcc> = (0..shard_slices.len())
        .map(|_| JsonlShardAcc::default())
        .collect();

    std::thread::scope(|scope| {
        for (shard_idx, (&slice, acc)) in shard_slices
            .iter()
            .zip(shard_accs.iter_mut())
            .enumerate()
        {
            let steps_ref = steps.as_deref();
            let finder_ref = finder.as_ref();
            scope.spawn(move || {
                let sample_schema = shard_idx == 0;
                let mut pos = 0usize;
                while pos < slice.len() {
                    let line_end = memchr::memchr(b'\n', &slice[pos..])
                        .map(|off| pos + off)
                        .unwrap_or(slice.len());
                    let trimmed = slice[pos..line_end].trim_ascii();
                    pos = line_end.saturating_add(1);
                    if trimmed.is_empty() || trimmed.starts_with(b"CCTX_TRACE {") {
                        continue;
                    }
                    // Fast path: once schema is sampled, skip full JSON DOM parse if required predicate key is absent!
                    if (!sample_schema || acc.records >= 300)
                        && let Some(f) = finder_ref
                        && f.find(trimmed).is_none()
                    {
                        if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
                            acc.records += 1;
                        }
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_slice::<Value>(trimmed) else {
                        continue;
                    };
                    acc.records += 1;
                    let targets: Vec<&Value> = match steps_ref {
                        Some(s) => evaluate_steps(vec![&parsed], s),
                        None => vec![&parsed],
                    };
                    for item in targets {
                        if sample_schema
                            && acc.records <= 300
                            && let Value::Object(map) = item
                        {
                            for (k, v) in map {
                                let entry = acc.field_kinds.entry(k.clone()).or_default();
                                if !v.is_null() {
                                    *entry.0.entry(value_kind_name(v)).or_default() += 1;
                                    entry.1 += 1;
                                }
                            }
                        }
                        if let Some(ref qbox) = filters.bbox {
                            let Some(ibox) = extract_value_bbox(item) else {
                                continue;
                            };
                            if !qbox.intersects(&ibox) {
                                continue;
                            }
                        }
                        if let Some(ref pred) = filters.predicate {
                            let target = lookup_dotted(item, pred.field());
                            if !pred.matches_value(target) {
                                continue;
                            }
                        }
                        if let Some(ref group_field) = filters.count_by {
                            let raw_key = lookup_dotted(item, group_field)
                                .and_then(|v| value_as_comparable_string(Some(v)));
                            let key = match raw_key {
                                Some(_) if super::is_sensitive_field_path(group_field) => {
                                    "<REDACTED>".to_string()
                                }
                                Some(v) => v,
                                None => "<null>".to_string(),
                            };
                            *acc.counts.entry(truncate_utf8_chars(&key, 60)).or_default() += 1;
                        }
                        acc.matched += 1;
                        if has_filter
                            && acc.matched_ids.len() < 15
                            && let Some(id) = extract_item_id(item)
                        {
                            acc.matched_ids.push(id);
                        }
                        if acc.samples.len() < filters.max_samples.min(25) {
                            acc.samples.push(compact_sample_value(item, 0));
                        }
                    }
                }
            });
        }
    });

    let mut total_records = 0usize;
    let mut matched_count = 0usize;
    let mut matched_ids = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut field_kinds: BTreeMap<String, (BTreeMap<&'static str, usize>, usize)> = BTreeMap::new();
    let mut samples = Vec::new();

    for shard in shard_accs {
        total_records += shard.records;
        matched_count += shard.matched;
        for (k, v) in shard.counts {
            *counts.entry(k).or_default() += v;
        }
        for (k, (tc, pr)) in shard.field_kinds {
            let entry = field_kinds.entry(k).or_default();
            entry.1 += pr;
            for (tk, tv) in tc {
                *entry.0.entry(tk).or_default() += tv;
            }
        }
        for id in shard.matched_ids {
            if matched_ids.len() < 15 {
                matched_ids.push(id);
            }
        }
        for s in shard.samples {
            if samples.len() < filters.max_samples.min(25) {
                samples.push(s);
            }
        }
    }

    if total_records == 0 {
        return Err(invalid(format!("no valid JSON or JSONL records found in {source_label}")).into());
    }

    let mut sorted_counts: Vec<(String, usize)> = counts.into_iter().collect();
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted_counts.truncate(20);

    let item_fields = field_kinds
        .into_iter()
        .take(28)
        .map(|(name, (tc, present))| {
            let names: Vec<&str> = tc.keys().copied().collect();
            FieldSummary {
                name,
                kind: if names.is_empty() {
                    "null".into()
                } else {
                    names.join("|")
                },
                present,
            }
        })
        .collect();

    Ok(JsonSummary {
        source: source_label.to_string(),
        format: "jsonl",
        root_type: "stream",
        detected_total_count: Some(total_records as u64),
        api_status: None,
        item_count: Some(total_records),
        matched_count: has_filter.then_some(matched_count),
        matched_ids,
        top_keys: Vec::new(),
        item_fields,
        counts_by: sorted_counts,
        samples,
    })
}

pub fn render_json_summary_text(summary: &JsonSummary) -> String {
    let mut lines = Vec::new();
    let mut header = format!(
        "[{}] format={} root={}",
        summary.source, summary.format, summary.root_type
    );
    if let Some(total) = summary.detected_total_count {
        header.push_str(&format!(" total_count={total}"));
    }
    if let Some(items) = summary.item_count {
        header.push_str(&format!(" items={items}"));
    }
    if let Some(matched) = summary.matched_count {
        header.push_str(&format!(" matched={matched}"));
    }
    if let Some(ref status) = summary.api_status {
        header.push_str(&format!(" status=\"{status}\""));
    }
    lines.push(header);

    if !summary.top_keys.is_empty() {
        lines.push(format!("  top_keys: [{}]", summary.top_keys.join(", ")));
    }
    if !summary.item_fields.is_empty() {
        let fields_str: Vec<String> = summary
            .item_fields
            .iter()
            .map(|f| format!("{}:{}", f.name, f.kind))
            .collect();
        lines.push(format!("  fields({}): {}", fields_str.len(), fields_str.join(", ")));
    }
    if !summary.matched_ids.is_empty() {
        lines.push(format!("  matched_ids: {:?}", summary.matched_ids));
    }
    if !summary.counts_by.is_empty() {
        let pairs: Vec<String> = summary
            .counts_by
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        lines.push(format!("  counts_by: {}", pairs.join(", ")));
    }
    for (idx, sample) in summary.samples.iter().enumerate() {
        let rendered = serde_json::to_string(sample).unwrap_or_default();
        lines.push(format!(
            "  sample[{idx}]: {}",
            truncate_utf8_chars(&rendered, 600)
        ));
    }
    clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET)
}

pub fn run(args: &JsonProbeArgs) -> Result<String> {
    let bbox = args.bbox.as_deref().map(BBox::parse).transpose()?;
    let predicate = args
        .where_expr
        .as_deref()
        .map(Predicate::parse)
        .transpose()?;
    let filters = QueryFilters {
        path: args.path.clone(),
        bbox,
        predicate,
        count_by: args.count_by.clone(),
        max_samples: args.samples.min(25),
    };

    let summary = if args.source.starts_with("http://") || args.source.starts_with("https://") {
        let resp = http_probe::fetch_single_url(&args.source, "GET", &[], None, args.timeout.clamp(1, 60), &[])?;
        if !(200..300).contains(&resp.status) {
            return Err(invalid(format!(
                "HTTP {} fetching {}: {}",
                resp.status,
                resp.redacted_url,
                truncate_utf8_chars(&String::from_utf8_lossy(&resp.body), 200)
            ))
            .into());
        }
        let text = String::from_utf8_lossy(&resp.body);
        match parse_json_tolerant(&text) {
            Ok(val) => summarize_parsed_value(&resp.redacted_url, "json", &val, &filters)?,
            Err(_) => summarize_jsonl_reader(
                &resp.redacted_url,
                BufReader::new(resp.body.as_slice()),
                &filters,
            )?,
        }
    } else if args.source == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(MAX_JSON_FILE_BYTES)
            .read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        match parse_json_tolerant(&text) {
            Ok(val) => summarize_parsed_value("stdin", "json", &val, &filters)?,
            Err(_) => summarize_jsonl_reader("stdin", BufReader::new(buf.as_slice()), &filters)?,
        }
    } else {
        let path = Path::new(&args.source);
        let meta = std::fs::metadata(path)
            .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if matches!(ext.as_str(), "jsonl" | "ndjson") {
            if meta.len() <= 64 * 1024 * 1024 {
                let raw_bytes = std::fs::read(path)?;
                summarize_jsonl_bytes_parallel(&args.source, &raw_bytes, &filters)?
            } else {
                let file = File::open(path)?;
                summarize_jsonl_reader(&args.source, BufReader::new(file), &filters)?
            }
        } else {
            if meta.len() > MAX_JSON_FILE_BYTES {
                return Err(invalid(format!(
                    "JSON file {} is {} MiB (limit 128 MiB); convert to .jsonl for streaming",
                    path.display(),
                    meta.len() / (1024 * 1024)
                ))
                .into());
            }
            let raw_bytes = std::fs::read(path)?;
            let text = String::from_utf8_lossy(&raw_bytes);
            let fmt = if ext == "geojson" { "geojson" } else { "json" };
            match parse_json_tolerant(&text) {
                Ok(val) => summarize_parsed_value(&args.source, fmt, &val, &filters)?,
                Err(_) => {
                    let file = File::open(path)?;
                    summarize_jsonl_reader(&args.source, BufReader::new(file), &filters)?
                }
            }
        }
    };

    if args.json {
        clamp_json_budget(serde_json::to_value(&summary)?)
    } else {
        Ok(render_json_summary_text(&summary))
    }
}
