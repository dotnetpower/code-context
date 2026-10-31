use crate::workspace::{
    MAX_RESPONSE, Workspace, capture, normalize, reject_symlinks, safe_relative,
};
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    200
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub matches: Vec<Hit>,
    pub total_matches: usize,
    pub truncated: bool,
    pub consistency: String,
}

pub fn search(workspace: &Workspace, request: &Search) -> Result<SearchResult> {
    if request.pattern.is_empty() || request.limit == 0 || request.limit > 1000 {
        return Err(invalid("pattern must be nonempty and limit must be 1..1000").into());
    }
    let mut command = workspace.command();
    command.arg("--json");
    if !request.regex {
        command.arg("--fixed-strings");
    }
    if request.ignore_case {
        command.arg("--ignore-case");
    }
    command
        .arg("--")
        .arg(&request.pattern)
        .args(&workspace.scopes);
    let output = capture(&mut command)?;
    let mut matches = Vec::new();
    for line in output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: Value = serde_json::from_slice(line)?;
        if event["type"] != "match" {
            continue;
        }
        let data = &event["data"];
        let path = data["path"]["text"]
            .as_str()
            .ok_or_else(|| invalid("non-UTF8 path unsupported"))?;
        safe_relative(Path::new(path))?;
        reject_symlinks(&workspace.root, Path::new(path))?;
        let text = data["lines"]["text"]
            .as_str()
            .ok_or_else(|| invalid("non-UTF8 match unsupported"))?;
        let number = data["line_number"]
            .as_u64()
            .ok_or_else(|| invalid("missing line number"))?;
        matches.push(Hit {
            path: normalize(Path::new(path)).to_string_lossy().into_owned(),
            line: usize::try_from(number)?,
            text: text.strip_suffix('\n').unwrap_or(text).to_owned(),
        });
    }
    matches.sort();
    matches.dedup();
    let total_matches = matches.len();
    matches.truncate(request.limit);
    let result = SearchResult {
        matches,
        total_matches,
        truncated: total_matches > request.limit,
        consistency: "live filesystem search; not an atomic repository snapshot".into(),
    };
    if crate::context::check_search_size(&result).is_err()
        && serde_json::to_vec(&result)?.len() > MAX_RESPONSE
    {
        return Err(invalid("response exceeds 256 KiB; narrow the search").into());
    }
    Ok(result)
}
