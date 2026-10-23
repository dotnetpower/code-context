use crate::query::{path_matches, paths};
use crate::workspace::MAX_RESPONSE;
use crate::{Result, invalid};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileQuery {
    pub glob: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    200
}

#[derive(Debug, Serialize)]
pub struct FileResult {
    pub paths: Vec<String>,
    pub total_matches: usize,
    pub truncated: bool,
}

pub fn find<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
    request: &FileQuery,
) -> Result<FileResult> {
    if request.glob.is_empty() || request.glob.len() > 1024 || !(1..=1000).contains(&request.limit)
    {
        return Err(
            invalid("nonempty filename/path glob <=1024 bytes and limit 1..1000 required").into(),
        );
    }
    let prefixes = paths(&request.paths)?;
    let glob = Glob::new(&request.glob)?.compile_matcher();
    let mut matches: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| {
            let path = Path::new(candidate);
            path_matches(path, &prefixes)
                && (glob.is_match(path) || path.file_name().is_some_and(|name| glob.is_match(name)))
        })
        .collect();
    matches.sort_unstable();
    matches.dedup();
    let total_matches = matches.len();
    let result = FileResult {
        paths: matches
            .into_iter()
            .take(request.limit)
            .map(String::from)
            .collect(),
        total_matches,
        truncated: total_matches > request.limit,
    };
    if serde_json::to_vec(&result)?.len() > MAX_RESPONSE {
        return Err(invalid(
            "file result exceeds response limit; narrow glob/paths or lower limit",
        )
        .into());
    }
    Ok(result)
}
