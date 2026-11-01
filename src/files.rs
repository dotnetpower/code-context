use crate::query::{
    brace_extension_suffixes, escape_dynamic_route_brackets, paths, scoped_extension_glob,
    simple_extension_suffix, trim_leading_dot_slash,
};
use crate::workspace::MAX_RESPONSE;
use crate::{Result, invalid};
use globset::{Glob, GlobSetBuilder};
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
    let normalized_glob = trim_leading_dot_slash(&request.glob);
    let ext_suffix = simple_extension_suffix(normalized_glob);
    let brace_suffixes = if ext_suffix.is_none() {
        brace_extension_suffixes(normalized_glob)
    } else {
        None
    };
    let scoped_ext = if ext_suffix.is_none() && brace_suffixes.is_none() {
        scoped_extension_glob(normalized_glob)
    } else {
        None
    };
    let glob = if ext_suffix.is_none() && brace_suffixes.is_none() && scoped_ext.is_none() {
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new(normalized_glob)?);
        if let Some(escaped) = escape_dynamic_route_brackets(normalized_glob)
            && let Ok(route_glob) = Glob::new(&escaped)
        {
            builder.add(route_glob);
        }
        Some(builder.build()?)
    } else {
        None
    };
    let candidates: Vec<&'a str> = candidates.into_iter().collect();
    let mut matches: Vec<&'a str> = Vec::with_capacity(candidates.len().min(request.limit));
    matches.extend(candidates.iter().copied().filter(|candidate| {
            if !crate::query::path_matches_str(candidate, &prefixes) {
                return false;
            }
            if let Some(suffix) = ext_suffix {
                candidate.ends_with(suffix)
                    || crate::document::matches_extension_case_insensitive(candidate, suffix)
            } else if let Some(suffixes) = &brace_suffixes {
                suffixes.iter().any(|s| {
                    candidate.ends_with(s.as_str())
                        || crate::document::matches_extension_case_insensitive(
                            candidate,
                            s.as_str(),
                        )
                })
            } else if let Some((dir_prefix, suffixes)) = &scoped_ext {
                candidate.starts_with(*dir_prefix)
                    && candidate.as_bytes().get(dir_prefix.len()) == Some(&b'/')
                    && suffixes
                        .iter()
                        .any(|s| candidate[dir_prefix.len() + 1..].ends_with(s.as_str()))
            } else {
                let path = Path::new(candidate);
                let glob = glob.as_ref().expect("compiled glob");
                glob.is_match(path) || path.file_name().is_some_and(|name| glob.is_match(name))
            }
        }));
    if matches.is_empty()
        && normalized_glob.len() >= 2
        && !normalized_glob.contains(['*', '?', '[', ']', '{', '}', '/', '\\', '.'])
    {
        let stem_lower = normalized_glob.to_ascii_lowercase();
        let stem_alnum: String = stem_lower
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        matches = candidates
            .into_iter()
            .filter(|candidate| {
                if !crate::query::path_matches_str(candidate, &prefixes) {
                    return false;
                }
                Path::new(candidate)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        let lower = name.to_ascii_lowercase();
                        if lower.contains(&stem_lower) {
                            return true;
                        }
                        if stem_alnum.len() >= 3 {
                            let name_alnum: String = lower
                                .chars()
                                .filter(|c| c.is_ascii_alphanumeric())
                                .collect();
                            return name_alnum.contains(&stem_alnum);
                        }
                        false
                    })
            })
            .collect();
    }
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
