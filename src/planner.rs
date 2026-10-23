use crate::resident::Resident;
use regex_syntax::hir::{
    Hir, HirKind,
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
            Self::And(children) => {
                let mut parts: Vec<_> = children
                    .iter()
                    .map(|part| part.evaluate(resident))
                    .collect();
                let posting_lookups = parts.iter().map(|part| part.posting_lookups).sum();
                let mut set_operations = parts.iter().map(|part| part.set_operations).sum();
                parts.sort_by_key(|part| part.ids.len());
                let Some(first) = parts.first() else {
                    return CandidateEvaluation::default();
                };
                let mut ids = first.ids.clone();
                for part in &parts[1..] {
                    ids = intersect(&ids, &part.ids);
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
            Self::Literal(_) => 1,
            Self::And(children) | Self::Or(children) => children.iter().map(Self::atom_count).sum(),
        }
    }

    pub(crate) fn matches(&self, bytes: &[u8]) -> bool {
        match self {
            Self::Literal(literal) => memchr::memmem::find(bytes, literal).is_some(),
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
        if let Some(boundary) = extracted(hir, kind) {
            conditions.push(boundary);
        }
    }
    match conditions.len() {
        0 => None,
        1 => conditions.pop().map(CandidateExpr::simplify),
        _ => Some(CandidateExpr::And(conditions).simplify()),
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
            let conditions: Vec<_> = children
                .iter()
                .filter_map(|child| mandatory(child, depth + 1))
                .collect();
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

fn simplify_group(children: Vec<CandidateExpr>, and: bool) -> CandidateExpr {
    let mut flattened = Vec::new();
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
    if flattened.len() == 1 {
        flattened.pop().expect("one element")
    } else if and {
        CandidateExpr::And(flattened)
    } else {
        CandidateExpr::Or(flattened)
    }
}

fn intersect(left: &[usize], right: &[usize]) -> Vec<usize> {
    let (mut a, mut b) = (0, 0);
    let mut output = Vec::with_capacity(left.len().min(right.len()));
    while a < left.len() && b < right.len() {
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                output.push(left[a]);
                a += 1;
                b += 1;
            }
        }
    }
    output
}

fn union<'a>(parts: impl Iterator<Item = &'a [usize]>) -> Vec<usize> {
    let mut output: Vec<_> = parts.flatten().copied().collect();
    output.sort_unstable();
    output.dedup();
    output
}
