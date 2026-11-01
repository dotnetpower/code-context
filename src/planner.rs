use crate::resident::Resident;
use regex_syntax::hir::{
    Class, Hir, HirKind,
    literal::{ExtractKind, Extractor},
};
use serde::Serialize;
use std::collections::BTreeSet;

const MAX_LITERAL_ALTERNATIVES: usize = 64;
const MAX_LITERAL_BYTES: usize = 96;
const MIN_INDEXED_LITERAL_BYTES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidateExpr {
    Literal(Vec<u8>),
    ByteClasses(Vec<[u8; 2]>),
    And(Vec<CandidateExpr>),
    Or(Vec<CandidateExpr>),
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct CandidateEvaluation {
    pub ids: Vec<usize>,
    pub posting_lookups: usize,
    pub set_operations: usize,
}

impl CandidateExpr {
    fn simplify(self) -> Self {
        match self {
            Self::And(children) => simplify_group(children, true),
            Self::Or(children) => simplify_group(children, false),
            literal => literal,
        }
    }

    pub(crate) fn evaluate(&self, resident: &Resident) -> CandidateEvaluation {
        match self {
            Self::Literal(bytes) => CandidateEvaluation {
                ids: resident.candidates_bytes(bytes),
                posting_lookups: bytes.len().saturating_sub(2),
                set_operations: 0,
            },
            Self::ByteClasses(positions) => CandidateEvaluation {
                ids: resident.candidates_byte_classes(positions),
                posting_lookups: positions.len().saturating_sub(2),
                set_operations: 0,
            },
            Self::And(children) => {
                let mut parts: Vec<_> = children
                    .iter()
                    .map(|part| part.evaluate(resident))
                    .collect();
                let posting_lookups = parts.iter().map(|part| part.posting_lookups).sum();
                let mut set_operations = parts.iter().map(|part| part.set_operations).sum();
                parts.sort_unstable_by_key(|part| part.ids.len());
                let mut iter = parts.into_iter();
                let Some(first) = iter.next() else {
                    return CandidateEvaluation::default();
                };
                let mut ids = first.ids;
                for part in iter {
                    intersect_in_place(&mut ids, &part.ids);
                    set_operations += 1;
                    if ids.is_empty() {
                        break;
                    }
                }
                CandidateEvaluation {
                    ids,
                    posting_lookups,
                    set_operations,
                }
            }
            Self::Or(children) => {
                let parts: Vec<_> = children
                    .iter()
                    .map(|part| part.evaluate(resident))
                    .collect();
                let posting_lookups = parts.iter().map(|part| part.posting_lookups).sum();
                let set_operations = parts.iter().map(|part| part.set_operations).sum::<usize>()
                    + parts.len().saturating_sub(1);
                let ids = union(parts.iter().map(|part| part.ids.as_slice()));
                CandidateEvaluation {
                    ids,
                    posting_lookups,
                    set_operations,
                }
            }
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Literal(bytes) => format!("lit({})", String::from_utf8_lossy(bytes)),
            Self::ByteClasses(positions) => {
                let folded: Vec<u8> = positions
                    .iter()
                    .map(|pair| pair[0].to_ascii_lowercase())
                    .collect();
                format!("ci({})", String::from_utf8_lossy(&folded))
            }
            Self::And(children) => format!(
                "and({})",
                children
                    .iter()
                    .map(Self::describe)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Or(children) => format!(
                "or({})",
                children
                    .iter()
                    .map(Self::describe)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    pub(crate) fn atom_count(&self) -> usize {
        match self {
            Self::Literal(_) | Self::ByteClasses(_) => 1,
            Self::And(children) | Self::Or(children) => children.iter().map(Self::atom_count).sum(),
        }
    }

    pub(crate) fn matches(&self, bytes: &[u8]) -> bool {
        match self {
            Self::Literal(literal) => memchr::memmem::find(bytes, literal).is_some(),
            Self::ByteClasses(positions) => {
                let Some(&[b0, b1]) = positions.first() else {
                    return true;
                };
                if positions.len() > bytes.len() {
                    return false;
                }
                let rest = &positions[1..];
                let last_start = bytes.len() - positions.len();
                let mut pos = 0usize;
                while pos <= last_start {
                    let found = if b0 == b1 {
                        memchr::memchr(b0, &bytes[pos..=last_start])
                    } else {
                        memchr::memchr2(b0, b1, &bytes[pos..=last_start])
                    };
                    let Some(rel) = found else {
                        return false;
                    };
                    let start = pos + rel;
                    if bytes[start + 1..start + 1 + rest.len()]
                        .iter()
                        .zip(rest)
                        .all(|(&b, &[p0, p1])| b == p0 || b == p1)
                    {
                        return true;
                    }
                    pos = start + 1;
                }
                false
            }
            Self::And(children) => children.iter().all(|child| child.matches(bytes)),
            Self::Or(children) => children.iter().any(|child| child.matches(bytes)),
        }
    }
}

pub(crate) fn build(hir: &Hir) -> Option<CandidateExpr> {
    let mut conditions = Vec::new();
    if let Some(inner) = mandatory(hir, 0) {
        conditions.push(inner);
    }
    for kind in [ExtractKind::Prefix, ExtractKind::Suffix] {
        if let Some(boundary) = extracted(hir, kind)
            && (conditions.is_empty() || boundary.atom_count() <= 4)
        {
            conditions.push(boundary);
        }
    }
    match conditions.len() {
        0 => None,
        1 => conditions.pop().map(CandidateExpr::simplify),
        _ => Some(CandidateExpr::And(conditions).simplify()),
    }
}

fn class_ascii_pair(class: &Class) -> Option<[u8; 2]> {
    let mut bytes = [0_u8; 2];
    let mut count = 0usize;
    match class {
        Class::Unicode(unicode) => {
            for range in unicode.iter() {
                if !range.start().is_ascii() || !range.end().is_ascii() {
                    return None;
                }
                for ch in range.start()..=range.end() {
                    if count >= 2 {
                        return None;
                    }
                    bytes[count] = ch as u8;
                    count += 1;
                }
            }
        }
        Class::Bytes(byte_class) => {
            for range in byte_class.iter() {
                for b in range.start()..=range.end() {
                    if count >= 2 {
                        return None;
                    }
                    bytes[count] = b;
                    count += 1;
                }
            }
        }
    }
    match count {
        1 => Some([bytes[0], bytes[0]]),
        2 => Some(bytes),
        _ => None,
    }
}

fn class_ascii_small(class: &Class) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(4);
    match class {
        Class::Unicode(unicode) => {
            for range in unicode.iter() {
                if !range.start().is_ascii() || !range.end().is_ascii() {
                    return None;
                }
                for ch in range.start()..=range.end() {
                    if bytes.len() >= 4 {
                        return None;
                    }
                    bytes.push(ch as u8);
                }
            }
        }
        Class::Bytes(byte_class) => {
            for range in byte_class.iter() {
                for b in range.start()..=range.end() {
                    if bytes.len() >= 4 {
                        return None;
                    }
                    bytes.push(b);
                }
            }
        }
    }
    (!bytes.is_empty()).then_some(bytes)
}

fn class_unicode_single_utf8(class: &Class) -> Option<Vec<u8>> {
    if let Class::Unicode(unicode) = class {
        let ranges = unicode.ranges();
        if ranges.len() == 1 && ranges[0].start() == ranges[0].end() {
            let ch = ranges[0].start();
            if !ch.is_ascii() {
                let mut buf = [0_u8; 4];
                return Some(ch.encode_utf8(&mut buf).as_bytes().to_vec());
            }
        }
    }
    None
}

#[derive(Clone)]
enum RunSlot {
    Pair([u8; 2]),
    Small(Vec<u8>),
}

fn flush_run(run: &mut Vec<RunSlot>, conditions: &mut Vec<CandidateExpr>) {
    if run.len() >= MIN_INDEXED_LITERAL_BYTES && run.len() <= MAX_LITERAL_BYTES {
        let small_count = run
            .iter()
            .filter(|slot| matches!(slot, RunSlot::Small(_)))
            .count();
        if small_count == 0 {
            let pairs: Vec<[u8; 2]> = run
                .iter()
                .map(|slot| match slot {
                    RunSlot::Pair(pair) => *pair,
                    RunSlot::Small(_) => unreachable!(),
                })
                .collect();
            if pairs.iter().all(|pair| pair[0] == pair[1]) {
                conditions.push(CandidateExpr::Literal(
                    pairs.iter().map(|pair| pair[0]).collect(),
                ));
            } else {
                conditions.push(CandidateExpr::ByteClasses(pairs));
            }
        } else if small_count == 1 {
            let all_exact = run.iter().all(|slot| match slot {
                RunSlot::Pair(pair) => pair[0] == pair[1],
                RunSlot::Small(_) => true,
            });
            let mut branches = Vec::new();
            let small_idx = run
                .iter()
                .position(|slot| matches!(slot, RunSlot::Small(_)))
                .expect("one small slot");
            let choices = match &run[small_idx] {
                RunSlot::Small(choices) => choices.as_slice(),
                RunSlot::Pair(_) => &[],
            };
            for &choice in choices {
                if all_exact {
                    let mut literal = Vec::with_capacity(run.len());
                    for (idx, slot) in run.iter().enumerate() {
                        if idx == small_idx {
                            literal.push(choice);
                        } else if let RunSlot::Pair([b, _]) = slot {
                            literal.push(*b);
                        }
                    }
                    branches.push(CandidateExpr::Literal(literal));
                } else {
                    let mut pairs = Vec::with_capacity(run.len());
                    for (idx, slot) in run.iter().enumerate() {
                        if idx == small_idx {
                            pairs.push([choice, choice]);
                        } else if let RunSlot::Pair(pair) = slot {
                            pairs.push(*pair);
                        }
                    }
                    branches.push(CandidateExpr::ByteClasses(pairs));
                }
            }
            if !branches.is_empty() {
                conditions.push(CandidateExpr::Or(branches).simplify());
            }
        }
    }
    run.clear();
}

fn push_concat_item(
    child: &Hir,
    depth: usize,
    run: &mut Vec<RunSlot>,
    conditions: &mut Vec<CandidateExpr>,
) {
    if depth > 64 {
        flush_run(run, conditions);
        return;
    }
    match child.kind() {
        HirKind::Empty | HirKind::Look(_) => {}
        HirKind::Literal(literal) => {
            for &byte in &literal.0 {
                run.push(RunSlot::Pair([byte, byte]));
            }
        }
        HirKind::Capture(capture) => {
            push_concat_item(&capture.sub, depth + 1, run, conditions);
        }
        HirKind::Concat(nested) => {
            for sub in nested {
                push_concat_item(sub, depth + 1, run, conditions);
            }
        }
        HirKind::Class(class) => {
            if let Some(pair) = class_ascii_pair(class) {
                run.push(RunSlot::Pair(pair));
            } else if let Some(utf8_bytes) = class_unicode_single_utf8(class) {
                for byte in utf8_bytes {
                    run.push(RunSlot::Pair([byte, byte]));
                }
            } else if let Some(small) = class_ascii_small(class) {
                if run.iter().any(|slot| matches!(slot, RunSlot::Small(_))) {
                    flush_run(run, conditions);
                }
                run.push(RunSlot::Small(small));
            } else {
                flush_run(run, conditions);
            }
        }
        _ => {
            flush_run(run, conditions);
            if let Some(cond) = mandatory(child, depth + 1) {
                conditions.push(cond);
            }
        }
    }
}

fn mandatory(hir: &Hir, depth: usize) -> Option<CandidateExpr> {
    if depth > 64 {
        return None;
    }
    match hir.kind() {
        HirKind::Literal(literal) => atom(&literal.0),
        HirKind::Capture(capture) => mandatory(&capture.sub, depth + 1),
        HirKind::Repetition(repeat) if repeat.min > 0 => mandatory(&repeat.sub, depth + 1),
        HirKind::Concat(children) => {
            let mut conditions = Vec::new();
            let mut run: Vec<RunSlot> = Vec::new();
            for child in children {
                push_concat_item(child, depth, &mut run, &mut conditions);
            }
            flush_run(&mut run, &mut conditions);
            match conditions.len() {
                0 => None,
                1 => conditions.into_iter().next(),
                _ => Some(CandidateExpr::And(conditions).simplify()),
            }
        }
        HirKind::Alternation(children) => {
            let conditions: Option<Vec<_>> = children
                .iter()
                .map(|child| mandatory(child, depth + 1))
                .collect();
            conditions.and_then(|conditions| match conditions.len() {
                0 => None,
                1 => conditions.into_iter().next(),
                _ => Some(CandidateExpr::Or(conditions).simplify()),
            })
        }
        _ => None,
    }
}

fn extracted(hir: &Hir, kind: ExtractKind) -> Option<CandidateExpr> {
    let mut extractor = Extractor::new();
    extractor
        .kind(kind)
        .limit_class(8)
        .limit_repeat(4)
        .limit_literal_len(MAX_LITERAL_BYTES)
        .limit_total(MAX_LITERAL_ALTERNATIVES);
    let sequence = extractor.extract(hir);
    let literals = sequence.literals()?;
    if literals.is_empty()
        || literals.len() > MAX_LITERAL_ALTERNATIVES
        || literals
            .iter()
            .any(|literal| literal.len() < MIN_INDEXED_LITERAL_BYTES)
    {
        return None;
    }
    let conditions: Vec<_> = literals
        .iter()
        .filter_map(|literal| atom(literal.as_bytes()))
        .collect();
    match conditions.len() {
        0 => None,
        1 => conditions.into_iter().next(),
        _ => Some(CandidateExpr::Or(conditions).simplify()),
    }
}

fn atom(bytes: &[u8]) -> Option<CandidateExpr> {
    (bytes.len() >= MIN_INDEXED_LITERAL_BYTES && bytes.len() <= MAX_LITERAL_BYTES)
        .then(|| CandidateExpr::Literal(bytes.to_vec()))
}

fn simplify_group(mut children: Vec<CandidateExpr>, and: bool) -> CandidateExpr {
    if children.len() == 1 {
        let single = children.pop().expect("one element");
        return match (and, single) {
            (true, CandidateExpr::And(nested)) | (false, CandidateExpr::Or(nested)) => {
                simplify_group(nested, and)
            }
            (_, single) => single,
        };
    }
    let mut flattened = Vec::with_capacity(children.len());
    for child in children {
        match (and, child) {
            (true, CandidateExpr::And(nested)) | (false, CandidateExpr::Or(nested)) => {
                flattened.extend(nested);
            }
            (_, child) => flattened.push(child),
        }
    }
    let mut seen = BTreeSet::new();
    flattened.retain(|child| seen.insert(child.describe()));
    if and && flattened.len() > 1 {
        flattened.sort_by_key(|child| match child {
            CandidateExpr::Literal(bytes) => (0_u8, std::cmp::Reverse(bytes.len())),
            CandidateExpr::ByteClasses(pos) => (1_u8, std::cmp::Reverse(pos.len())),
            CandidateExpr::And(_) => (2_u8, std::cmp::Reverse(0)),
            CandidateExpr::Or(_) => (3_u8, std::cmp::Reverse(0)),
        });
    }
    if flattened.len() == 1 {
        flattened.pop().expect("one element")
    } else if and {
        CandidateExpr::And(flattened)
    } else {
        CandidateExpr::Or(flattened)
    }
}

fn intersect_in_place(left: &mut Vec<usize>, right: &[usize]) {
    let mut cursor = right;
    left.retain(|id| {
        if cursor.is_empty() {
            return false;
        }
        match cursor.binary_search(id) {
            Ok(pos) => {
                cursor = &cursor[pos + 1..];
                true
            }
            Err(pos) => {
                cursor = &cursor[pos..];
                false
            }
        }
    });
}

fn union<'a>(parts: impl Iterator<Item = &'a [usize]>) -> Vec<usize> {
    let slices: Vec<&'a [usize]> = parts.filter(|s| !s.is_empty()).collect();
    match slices.len() {
        0 => Vec::new(),
        1 => slices[0].to_vec(),
        2 => {
            let (left, right) = (slices[0], slices[1]);
            let mut out = Vec::with_capacity(left.len() + right.len());
            let (mut i, mut j) = (0, 0);
            while i < left.len() && j < right.len() {
                match left[i].cmp(&right[j]) {
                    std::cmp::Ordering::Less => {
                        out.push(left[i]);
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        out.push(right[j]);
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        out.push(left[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            out.extend_from_slice(&left[i..]);
            out.extend_from_slice(&right[j..]);
            out
        }
        _ => {
            let max_id = slices
                .iter()
                .filter_map(|s| s.last().copied())
                .max()
                .unwrap_or(0);
            if max_id < 65_536 {
                let mut words = vec![0_u64; (max_id / 64) + 1];
                for slice in &slices {
                    for &id in *slice {
                        words[id / 64] |= 1_u64 << (id % 64);
                    }
                }
                let mut out = Vec::new();
                for (word_idx, &word) in words.iter().enumerate() {
                    let mut w = word;
                    while w != 0 {
                        let bit = w.trailing_zeros() as usize;
                        out.push(word_idx * 64 + bit);
                        w &= w - 1;
                    }
                }
                out
            } else {
                let mut output: Vec<_> = slices.into_iter().flatten().copied().collect();
                output.sort_unstable();
                output.dedup();
                output
            }
        }
    }
}
