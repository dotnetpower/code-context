//! Keeps MCP tool responses inside the host's inline output budget.
//!
//! Hosts offload tool output above ~8 KiB to a temporary file, which costs an extra
//! read turn. Trimming tails server-side with an explicit continuation hint avoids that.

use serde_json::{Value, json};

/// Matches the Native formatter budget (`MAX_INLINE_CHARS`), safely below 8 KiB.
pub const INLINE_BUDGET: usize = 7_400;

const NOTICE: &str = "inline budget reached (<8KiB): tail omitted. Narrow with paths/glob, lower limit/radius, set max_matches_per_file, or continue with next_ranges.";

fn item_cost(item: &Value) -> usize {
    serde_json::to_string(item).map_or(0, |s| s.len() + 1)
}

/// Drops tail items of `items` until `excess` bytes are removed; keeps at least one item.
fn trim_tail(items: &mut Vec<Value>, mut excess: usize) -> Vec<Value> {
    let mut dropped = Vec::new();
    while excess > 0 && items.len() > 1 {
        let item = items.pop().expect("length checked");
        excess = excess.saturating_sub(item_cost(&item));
        dropped.push(item);
    }
    dropped.reverse();
    dropped
}

fn trim_matches(data: &mut Value, pointer: &str, excess: usize) -> usize {
    let Some(items) = data.pointer_mut(pointer).and_then(Value::as_array_mut) else {
        return excess;
    };
    let dropped = trim_tail(items, excess);
    let removed: usize = dropped.iter().map(item_cost).sum();
    if !dropped.is_empty() {
        let parent = pointer.rsplit_once('/').map_or("", |(parent, _)| parent);
        if let Some(object) = data.pointer_mut(parent).and_then(Value::as_object_mut) {
            object.insert("truncated".into(), json!(true));
            object.insert("omitted".into(), json!(dropped.len()));
        }
    }
    excess.saturating_sub(removed)
}

/// Moves overflowing snippets (and surplus lines of a lone snippet) into `next_ranges`.
fn trim_snippets(data: &mut Value, excess: usize) -> usize {
    let Some(items) = data.get_mut("snippets").and_then(Value::as_array_mut) else {
        return excess;
    };
    let mut dropped = trim_tail(items, excess);
    let mut removed: usize = dropped.iter().map(item_cost).sum();
    if removed < excess
        && let Some(last) = items.last_mut()
    {
        let original_end = last.get("end").and_then(Value::as_u64);
        let start = last.get("start").and_then(Value::as_u64).unwrap_or(1);
        let mut kept = 0usize;
        if let Some(lines) = last.get_mut("lines").and_then(Value::as_array_mut) {
            let mut need = excess - removed;
            while need > 0 && lines.len() > 1 {
                let line = lines.pop().expect("length checked");
                let cost = line.as_str().map_or(2, |s| s.len() + 3);
                need = need.saturating_sub(cost);
                removed += cost;
            }
            kept = lines.len();
        }
        if kept > 0 && original_end.is_some_and(|end| start + kept as u64 - 1 < end) {
            let kept_end = start + kept as u64 - 1;
            last["end"] = json!(kept_end);
            if let Some(path) = last.get("path").cloned() {
                dropped.push(json!({"path": path, "start": kept_end + 1, "end": original_end}));
            }
        }
    }
    let next: Vec<Value> = dropped
        .iter()
        .filter_map(|s| Some(json!({"path": s.get("path")?, "start": s.get("start")?, "end": s.get("end")?})))
        .collect();
    if !next.is_empty() {
        let object = data.as_object_mut().expect("data object");
        let mut merged = next;
        if let Some(existing) = object.get("next_ranges").and_then(Value::as_array) {
            merged.extend(existing.iter().cloned());
        }
        object.insert("next_ranges".into(), Value::Array(merged));
        object.insert("complete".into(), json!(false));
        object.insert("truncated".into(), json!(true));
    }
    excess.saturating_sub(removed)
}

/// MCP responses are bare objects (`code_files`/`code_grep`/`code_context`/paged reads) or,
/// for a plain `code_read`, a bare array of snippets. Arrays become the documented paged
/// shape so the continuation can be reported.
fn paged_from_snippets(snippets: Vec<Value>, root: Option<&str>) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("snippets".into(), Value::Array(snippets));
    object.insert("complete".into(), json!(true));
    object.insert("page".into(), json!(true));
    if let Some(root) = root {
        object.insert("root".into(), json!(root));
    }
    Value::Object(object)
}

fn trim_value(name: &str, data: &mut Value, excess: usize) -> usize {
    match name {
        "code_files" => trim_matches(data, "/paths", excess),
        "code_grep" => trim_matches(data, "/matches", excess),
        "code_read" => trim_snippets(data, excess),
        _ => {
            let left = trim_snippets(data, excess);
            if left > 0 { trim_matches(data, "/search/matches", left) } else { left }
        }
    }
}

/// Returns `text` unchanged when it fits or cannot be safely trimmed.
pub fn fit_inline(name: &str, root: Option<&str>, text: String) -> String {
    if text.len() <= INLINE_BUDGET || !matches!(name, "code_files" | "code_grep" | "code_context" | "code_read") {
        return text;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return text;
    };
    let original = match parsed {
        Value::Array(snippets) if name == "code_read" => paged_from_snippets(snippets, root),
        Value::Object(_) => parsed,
        _ => return text,
    };
    // Continuation metadata grows the output, so widen the trim until it fits.
    let mut extra = NOTICE.len() + 120;
    let mut best = text.clone();
    for _ in 0..5 {
        let mut value = original.clone();
        let excess = text.len().saturating_sub(INLINE_BUDGET) + extra;
        if trim_value(name, &mut value, excess) == excess {
            break;
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("inline_notice".into(), json!(NOTICE));
        }
        let Ok(out) = serde_json::to_string(&value) else {
            break;
        };
        let size = out.len();
        if size < best.len() {
            best = out;
        }
        if size <= INLINE_BUDGET {
            break;
        }
        extra += size - INLINE_BUDGET + 16;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: Value) -> String {
        serde_json::to_string(&value).unwrap()
    }

    fn snippet(path: &str, start: u64, end: u64, width: usize) -> Value {
        json!({"path": path, "start": start, "end": end, "sha256": "h",
            "lines": vec!["a".repeat(width); (end - start + 1) as usize]})
    }

    #[test]
    fn small_and_unknown_responses_are_untouched() {
        let small = text(json!({"matches": [{"line": 1}]}));
        assert_eq!(fit_inline("code_grep", None, small.clone()), small);
        let big = "x".repeat(INLINE_BUDGET * 2);
        assert_eq!(fit_inline("code_status", None, big.clone()), big);
        assert_eq!(fit_inline("code_grep", None, big.clone()), big);
        let big_scalar = text(json!("y".repeat(INLINE_BUDGET * 2)));
        assert_eq!(fit_inline("code_read", None, big_scalar.clone()), big_scalar);
    }

    #[test]
    fn grep_matches_are_trimmed_with_notice() {
        let matches: Vec<Value> = (0..400).map(|i| json!({"line": i, "path": "src/a.rs", "text": "let value = compute();"})).collect();
        let out = fit_inline("code_grep", None, text(json!({"matches": matches, "total_matches": 400, "truncated": false, "freshness": {"basis": "x"}})));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["truncated"], true);
        assert!(value["omitted"].as_u64().unwrap() > 0);
        assert_eq!(value["total_matches"], 400);
        assert_eq!(value["freshness"]["basis"], "x");
        assert!(value["inline_notice"].is_string());
        assert_eq!(value["matches"][0]["line"], 0);
    }

    #[test]
    fn files_paths_are_trimmed() {
        let paths: Vec<String> = (0..600).map(|i| format!("src/module_{i}/file.rs")).collect();
        let out = fit_inline("code_files", None, text(json!({"paths": paths, "total_matches": 600, "truncated": false})));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["paths"][0], "src/module_0/file.rs");
    }

    #[test]
    fn plain_read_array_becomes_a_paged_shape_with_next_ranges() {
        let snippets: Vec<Value> = (0..6).map(|i| snippet(&format!("f{i}.rs"), 1, 20, 80)).collect();
        let out = fit_inline("code_read", Some("/r"), text(Value::Array(snippets)));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["page"], true);
        assert_eq!(value["root"], "/r");
        let kept = value["snippets"].as_array().unwrap().len();
        let next = value["next_ranges"].as_array().unwrap().len();
        assert_eq!(kept + next, 6);
        assert_eq!(value["next_ranges"][0]["start"], 1);
        assert!(value["inline_notice"].is_string());
    }

    #[test]
    fn fitting_plain_read_array_keeps_its_original_shape() {
        let small = text(Value::Array(vec![snippet("a.rs", 1, 3, 10)]));
        assert_eq!(fit_inline("code_read", None, small.clone()), small);
    }

    #[test]
    fn paged_read_overflow_becomes_next_ranges() {
        let snippets: Vec<Value> = (0..6).map(|i| snippet(&format!("f{i}.rs"), 1, 20, 80)).collect();
        let out = fit_inline("code_read", None, text(json!({"snippets": snippets, "complete": true, "page": true, "next_ranges": []})));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["snippets"].as_array().unwrap().len() + value["next_ranges"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn lone_oversized_snippet_is_split_with_continuation() {
        let out = fit_inline("code_read", None, text(Value::Array(vec![snippet("big.rs", 1, 200, 100)])));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        let end = value["snippets"][0]["end"].as_u64().unwrap();
        let lines = value["snippets"][0]["lines"].as_array().unwrap().len() as u64;
        assert_eq!(end, lines);
        assert_eq!(value["next_ranges"][0]["start"].as_u64().unwrap(), end + 1);
        assert_eq!(value["next_ranges"][0]["end"], 200);
    }

    #[test]
    fn context_trims_snippets_before_search_matches() {
        let snippets: Vec<Value> = (0..30).map(|i| snippet("a.rs", i * 10 + 1, i * 10 + 10, 60)).collect();
        let out = fit_inline("code_context", None, text(json!({"search": {"matches": [{"line": 1}]}, "snippets": snippets})));
        assert!(out.len() <= INLINE_BUDGET, "{}", out.len());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["search"]["matches"][0]["line"], 1);
        assert!(!value["next_ranges"].as_array().unwrap().is_empty());
    }
}
