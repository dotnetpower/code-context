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

fn simple_extension_suffix(glob: &str) -> Option<&str> {
    let ext = glob.strip_prefix("*.")?;
    let suffix = &glob[1..];
    if !ext.is_empty()
        && ext
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(suffix)
    } else {
        None
    }
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
    let ext_suffix = simple_extension_suffix(&request.glob);
    let glob = if ext_suffix.is_none() {
        Some(Glob::new(&request.glob)?.compile_matcher())
    } else {
        None
    };
    let mut matches: Vec<&'a str> = candidates
        .into_iter()
        .filter(|candidate| {
            let path = Path::new(candidate);
            if !path_matches(path, &prefixes) {
                return false;
            }
            if let Some(suffix) = ext_suffix {
                candidate.ends_with(suffix)
            } else {
                let glob = glob.as_ref().expect("compiled glob");
                glob.is_match(path) || path.file_name().is_some_and(|name| glob.is_match(name))
            }
        })
        .collect();
    if !matches.windows(2).all(|w| w[0] < w[1]) {
        matches.sort_unstable();
        matches.dedup();
    }
    let total_matches = matches.len();
    let paths: Vec<String> = matches
        .into_iter()
        .take(request.limit)
        .map(String::from)
        .collect();
    let bound = 64
        + paths
            .iter()
            .map(|p| 2 + crate::context::string_bound(p))
            .sum::<usize>();
    let result = FileResult {
        paths,
        total_matches,
        truncated: total_matches > request.limit,
    };
    if bound > MAX_RESPONSE.saturating_sub(64) && serde_json::to_vec(&result)?.len() > MAX_RESPONSE
    {
        return Err(invalid(
            "file result exceeds response limit; narrow glob/paths or lower limit",
        )
        .into());
    }
    Ok(result)
}
