use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellLookupInfo {
    pub is_shell_tool: bool,
    pub is_source_lookup: bool,
    pub non_source_host_only: bool,
    pub inferred_cwd: Option<String>,
    pub inferred_target: Option<String>,
}

pub(crate) fn is_shell_tool_name(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "Bash"
            | "run_in_terminal"
            | "exec_command"
            | "shell"
            | "local_shell"
            | "shell_command"
            | "container.exec"
            | "mcp__acp__Bash"
    )
}

pub(crate) fn inspect_shell_call(tool_name: &str, args: &Value) -> ShellLookupInfo {
    if !is_shell_tool_name(tool_name) {
        return ShellLookupInfo::default();
    }
    let explicit_workdir = args
        .get("workdir")
        .or_else(|| args.get("cwd"))
        .or_else(|| args.get("directory"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);

    let Some(raw_cmd) = extract_command_string(args) else {
        return ShellLookupInfo {
            is_shell_tool: true,
            inferred_cwd: explicit_workdir,
            ..Default::default()
        };
    };

    classify_shell_command_str(&raw_cmd, explicit_workdir)
}

fn extract_command_string(args: &Value) -> Option<String> {
    for key in ["command", "cmd", "script"] {
        if let Some(val) = args.get(key) {
            if let Some(s) = val.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.to_owned());
                }
            } else if let Some(arr) = val.as_array() {
                let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                if !parts.is_empty() {
                    // Unwrap `["bash", "-lc", "rg foo src"]` style arrays directly.
                    if parts.len() >= 3
                        && is_shell_wrapper_bin(parts[0])
                        && parts[1].starts_with('-')
                        && parts[1].contains('c')
                    {
                        return Some(parts[2..].join(" "));
                    }
                    return Some(parts.join(" "));
                }
            }
        }
    }
    None
}

fn is_shell_wrapper_bin(tok: &str) -> bool {
    let base = tok.rsplit('/').next().unwrap_or(tok);
    matches!(base, "sh" | "bash" | "zsh" | "dash" | "ksh")
}

pub(crate) fn classify_shell_command_str(
    raw_cmd: &str,
    mut inferred_cwd: Option<String>,
) -> ShellLookupInfo {
    let unwrapped = unwrap_outer_shell(raw_cmd.trim());
    if unwrapped.is_empty() || has_write_redirection_or_subshell(unwrapped) {
        return ShellLookupInfo {
            is_shell_tool: true,
            inferred_cwd,
            ..Default::default()
        };
    }

    let segments = split_shell_segments(unwrapped);
    if segments.is_empty() {
        return ShellLookupInfo {
            is_shell_tool: true,
            inferred_cwd,
            ..Default::default()
        };
    }

    let mut saw_primary_lookup = false;
    let mut non_source_host_only = false;
    let mut inferred_target: Option<String> = None;

    for seg in segments {
        let tokens = tokenize_shell_words(seg);
        if tokens.is_empty() {
            continue;
        }
        let mut idx = 0usize;
        while idx < tokens.len() && is_env_assignment(&tokens[idx]) {
            idx += 1;
        }
        if idx < tokens.len()
            && matches!(
                tokens[idx].as_str(),
                "env" | "time" | "timeout" | "nice" | "command"
            )
        {
            idx += 1;
            while idx < tokens.len()
                && (tokens[idx].starts_with('-')
                    || is_env_assignment(&tokens[idx])
                    || tokens[idx].chars().all(|c| c.is_ascii_digit() || c == 's'))
            {
                idx += 1;
            }
        }
        if idx >= tokens.len() {
            continue;
        }

        let cmd_raw = tokens[idx].as_str();
        let cmd = cmd_raw.rsplit('/').next().unwrap_or(cmd_raw);
        let rest = &tokens[idx + 1..];

        if cmd == "cd" {
            if let Some(dir) = rest.iter().find(|t| !t.starts_with('-'))
                && *dir != "-"
            {
                inferred_cwd = Some(dir.clone());
            }
            continue;
        }
        if matches!(cmd, "true" | "echo" | "printf" | "pwd") {
            continue;
        }

        if is_disqualifying_command(cmd, rest) {
            return ShellLookupInfo {
                is_shell_tool: true,
                inferred_cwd,
                ..Default::default()
            };
        }

        if is_primary_lookup_command(cmd, rest) {
            saw_primary_lookup = true;
            let (positional, has_explicit_expr) = collect_positional_lookup_args(cmd, rest);
            let path_slice = if matches!(
                cmd,
                "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "ag" | "ack" | "sed" | "awk" | "gawk"
            ) && !has_explicit_expr
            {
                positional.get(1..).unwrap_or(&[])
            } else {
                positional.as_slice()
            };
            for arg in path_slice {
                if is_non_source_target(arg) {
                    non_source_host_only = true;
                } else if inferred_target.is_none() && looks_like_path_token(cmd, arg) {
                    inferred_target = Some((*arg).clone());
                }
            }
            if let Some(last) = path_slice.last() {
                if is_non_source_target(last) {
                    non_source_host_only = true;
                } else if inferred_target.is_none() {
                    inferred_target = Some((*last).clone());
                }
            }
            continue;
        }

        if is_benign_pipe_filter(cmd, rest) {
            for arg in rest {
                if !arg.starts_with('-') && is_non_source_target(arg) {
                    non_source_host_only = true;
                }
            }
            continue;
        }

        // Unknown command in pipeline/chain -> conservative: not a pure source lookup.
        return ShellLookupInfo {
            is_shell_tool: true,
            inferred_cwd,
            ..Default::default()
        };
    }

    if let Some(ref cwd) = inferred_cwd
        && is_non_source_target(cwd)
    {
        non_source_host_only = true;
    }

    ShellLookupInfo {
        is_shell_tool: true,
        is_source_lookup: saw_primary_lookup,
        non_source_host_only: saw_primary_lookup && non_source_host_only,
        inferred_cwd,
        inferred_target,
    }
}

fn unwrap_outer_shell(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim();
        let mut iter = trimmed.split_whitespace();
        let Some(first) = iter.next() else {
            return trimmed;
        };
        if !is_shell_wrapper_bin(first) {
            return trimmed;
        }
        let Some(flag) = iter.next() else {
            return trimmed;
        };
        if !flag.starts_with('-') || !flag.contains('c') {
            return trimmed;
        }
        let after_first = trimmed[first.len()..].trim_start();
        let rest = after_first[flag.len()..].trim();
        if is_single_wrapped_quoted_literal(rest) {
            s = &rest[1..rest.len() - 1];
        } else {
            return rest;
        }
    }
}

fn is_single_wrapped_quoted_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    let quote = bytes[0];
    if (quote != b'\'' && quote != b'"') || bytes[bytes.len() - 1] != quote {
        return false;
    }
    let mut i = 1usize;
    while i + 1 < bytes.len() {
        if quote == b'"' && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            // Quote closed before the final character (e.g. `'rg foo' && rm 'bar'`)!
            return false;
        }
        i += 1;
    }
    i == bytes.len() - 1
}

fn collect_positional_lookup_args<'a>(cmd: &str, args: &'a [String]) -> (Vec<&'a String>, bool) {
    let mut out = Vec::new();
    let mut skip_next = false;
    let mut has_explicit_expr = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') {
            if matches!(
                arg.as_str(),
                "-e" | "--regexp" | "-f" | "--file"
            ) || arg.starts_with("--regexp=")
                || arg.starts_with("--file=")
            {
                has_explicit_expr = true;
            }
            if flag_takes_separate_value(cmd, arg) {
                skip_next = true;
            }
            continue;
        }
        out.push(arg);
    }
    (out, has_explicit_expr)
}

fn flag_takes_separate_value(cmd: &str, flag: &str) -> bool {
    if flag.contains('=') {
        return false;
    }
    match cmd {
        "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "ag" | "ack" => matches!(
            flag,
            "-e" | "--regexp"
                | "-f"
                | "--file"
                | "-g"
                | "--glob"
                | "-t"
                | "--type"
                | "-T"
                | "--type-not"
                | "-m"
                | "--max-count"
                | "-A"
                | "--after-context"
                | "-B"
                | "--before-context"
                | "-C"
                | "--context"
                | "--max-depth"
                | "--iglob"
        ),
        "head" | "tail" => matches!(flag, "-n" | "--lines" | "-c" | "--bytes"),
        "find" => matches!(
            flag,
            "-name" | "-iname" | "-path" | "-ipath" | "-maxdepth" | "-mindepth" | "-type"
        ),
        "sed" => matches!(flag, "-e" | "-f"),
        _ => false,
    }
}

fn has_write_redirection_or_subshell(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && (b == b'`' || (b == b'$' && bytes.get(i + 1) == Some(&b'('))) {
            return true;
        }
        if !in_single && !in_double && b == b'>' {
            let mut j = i + 1;
            if bytes.get(j) == Some(&b'>') {
                j += 1;
            }
            if bytes.get(j) == Some(&b'&') {
                i = j + 1;
                continue;
            }
            let after = cmd[j..].trim_start();
            if after.starts_with("/dev/null") {
                i = j + "/dev/null".len();
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

fn split_shell_segments(cmd: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = cmd.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
        } else if b == b'"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double {
            if b == b';' || b == b'\n' {
                let seg = cmd[start..i].trim();
                if !seg.is_empty() {
                    out.push(seg);
                }
                start = i + 1;
            } else if (b == b'&' && bytes.get(i + 1) == Some(&b'&'))
                || (b == b'|' && bytes.get(i + 1) == Some(&b'|'))
            {
                let seg = cmd[start..i].trim();
                if !seg.is_empty() {
                    out.push(seg);
                }
                i += 2;
                start = i;
                continue;
            } else if b == b'|' {
                let seg = cmd[start..i].trim();
                if !seg.is_empty() {
                    out.push(seg);
                }
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail = cmd[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn tokenize_shell_words(seg: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = seg.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && !in_single {
            if let Some(next) = chars.next() {
                cur.push(next);
            }
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !cur.is_empty() {
                if !is_fd_redirect_token(&cur) {
                    words.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() && !is_fd_redirect_token(&cur) {
        words.push(cur);
    }
    words
}

fn is_fd_redirect_token(tok: &str) -> bool {
    tok == "2>&1"
        || tok == "1>&2"
        || tok.starts_with("2>/dev/null")
        || tok.starts_with(">/dev/null")
        || tok == "</dev/null"
}

fn is_env_assignment(tok: &str) -> bool {
    let Some((k, _)) = tok.split_once('=') else {
        return false;
    };
    !k.is_empty()
        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_disqualifying_command(cmd: &str, args: &[String]) -> bool {
    if matches!(
        cmd,
        "cargo"
            | "rustc"
            | "rustup"
            | "pytest"
            | "python"
            | "python3"
            | "uv"
            | "pip"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "node"
            | "deno"
            | "tsc"
            | "eslint"
            | "ruff"
            | "vitest"
            | "jest"
            | "go"
            | "make"
            | "cmake"
            | "ninja"
            | "gradle"
            | "mvn"
            | "dotnet"
            | "git"
            | "gh"
            | "curl"
            | "wget"
            | "ssh"
            | "scp"
            | "rsync"
            | "docker"
            | "podman"
            | "kubectl"
            | "helm"
            | "systemctl"
            | "journalctl"
            | "ps"
            | "lsof"
            | "ss"
            | "netstat"
            | "kill"
            | "cctx"
            | "tctx"
            | "pctx"
            | "rm"
            | "mv"
            | "cp"
            | "mkdir"
            | "rmdir"
            | "touch"
            | "chmod"
            | "chown"
            | "ln"
            | "tee"
            | "patch"
            | "perl"
            | "sd"
    ) {
        return true;
    }
    if cmd == "sed"
        && args.iter().any(|a| {
            a == "-i" || a.starts_with("-i") || a == "--in-place" || a.starts_with("--in-place=")
        })
    {
        return true;
    }
    if cmd == "awk" && args.iter().any(|a| a == "-i" || a == "inplace") {
        return true;
    }
    if cmd == "find"
        && args
            .iter()
            .any(|a| matches!(a.as_str(), "-delete" | "-exec" | "-execdir" | "-ok"))
    {
        return true;
    }
    false
}

fn is_primary_lookup_command(cmd: &str, args: &[String]) -> bool {
    match cmd {
        "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "ag" | "ack" | "cat" | "nl" | "bat"
        | "head" | "tail" | "find" | "fd" | "fdfind" | "tree" => !args.is_empty(),
        "sed" => {
            // `sed -n '10,50p' path/to/file` requires a file target (not just stdin filter).
            args.iter().filter(|a| !a.starts_with('-')).count() >= 2
        }
        "awk" | "gawk" => args.iter().filter(|a| !a.starts_with('-')).count() >= 2,
        "ls" => true,
        _ => false,
    }
}

fn is_benign_pipe_filter(cmd: &str, args: &[String]) -> bool {
    if cmd == "xargs" {
        let mut sub_iter = args.iter().skip_while(|a| a.starts_with('-'));
        let Some(sub_raw) = sub_iter.next() else {
            return false;
        };
        let sub_cmd = sub_raw.rsplit('/').next().unwrap_or(sub_raw.as_str());
        let sub_rest: Vec<String> = sub_iter.cloned().collect();
        if is_disqualifying_command(sub_cmd, &sub_rest) {
            return false;
        }
        return matches!(
            sub_cmd,
            "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "cat" | "head" | "tail" | "nl" | "wc"
                | "ls" | "stat"
        );
    }
    if cmd == "sed"
        && args.iter().any(|a| {
            a == "-i" || a.starts_with("-i") || a == "--in-place" || a.starts_with("--in-place=")
        })
    {
        return false;
    }
    if matches!(cmd, "awk" | "gawk") && args.iter().any(|a| a == "-i" || a == "inplace") {
        return false;
    }
    matches!(
        cmd,
        "wc" | "sort" | "uniq" | "cut" | "tr" | "sed" | "awk" | "gawk" | "column"
    )
}

pub(crate) fn is_non_source_target(arg: &str) -> bool {
    let trimmed = arg.trim_matches('"').trim_matches('\'');
    let norm = trimmed.replace('\\', "/");
    let clean = norm.split('#').next().unwrap_or(&norm);
    if clean.starts_with("/tmp")
        || clean.starts_with("/var/tmp")
        || clean.starts_with("/dev/shm")
        || clean.starts_with("/proc")
        || clean.starts_with("/sys")
        || clean.starts_with("/etc")
        || clean.contains("copilot-tool-output")
    {
        return true;
    }
    if clean
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .any(|seg| {
            matches!(
                seg,
                ".github"
                    | ".vscode"
                    | ".copilot"
                    | ".claude"
                    | ".codex"
                    | ".git"
                    | "node_modules"
                    | "target"
            )
        })
    {
        return true;
    }
    let lower = clean.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".webp", ".gif", ".pdf", ".vsix", ".jsonl", ".zst",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn looks_like_path_token(cmd: &str, arg: &str) -> bool {
    if matches!(cmd, "cat" | "nl" | "bat" | "head" | "tail" | "find" | "ls" | "tree") {
        return true;
    }
    arg.contains('/') || arg.contains('.')
}
