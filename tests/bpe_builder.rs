use code_context::bpe_builder::{
    AuditSeverity, BpeBuildConfig, BpeProfile, BpeSource, LearnedReplacement, audit_bpe_profile,
    build_bpe_profile, detect_sensitive_patterns, normalize_cross_platform_dir,
    normalize_cross_platform_dir_checked, profile_path_in, read_profile_file, reset_bpe_profile,
    rollback_bpe_profile, sanitize_user_prompt,
};
use code_context::prompt_formula::{compress_task_prompt_with_profile, estimate_bpe_tokens};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn write_copilot_session(dir: &Path, sid: &str, lines: &[Value], crlf: bool) {
    let sdir = dir.join(sid);
    std::fs::create_dir_all(&sdir).unwrap();
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push_str(sep);
    }
    std::fs::write(sdir.join("events.jsonl"), body).unwrap();
}

#[test]
fn cli_build_bpe_status_compress_if_needed_reset_and_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let state = tmp.path().join("state");
    let sessions = home.join(".copilot").join("session-state");
    std::fs::create_dir_all(&sessions).unwrap();

    let now_ts = current_unix_secs().saturating_sub(600);
    for sid in ["sess-a", "sess-b", "sess-c"] {
        // Use CRLF in one session to exercise Windows-created JSONL logs.
        let crlf = sid == "sess-b";
        write_copilot_session(
            &sessions,
            sid,
            &[
                json!({
                    "type": "user.message",
                    "timestamp": now_ts,
                    "data": {
                        "content": "혹시 백엔드 지연 원인을 분석해서 정리해줘 부탁해"
                    }
                }),
                json!({
                    "type": "user.message",
                    "timestamp": now_ts * 1000,
                    "data": {
                        "content": [{"type": "text", "text": "전반적으로 성능 병목을 조사해서 보강하고 보고해줘"}]
                    }
                }),
                json!({
                    "type": "user.message",
                    "timestamp": now_ts,
                    "data": {
                        "content": "can you take a look at the cache layer and then check whether retries stall"
                    }
                }),
                json!({
                    "type": "assistant.usage",
                    "timestamp": now_ts,
                    "data": { "model": "claude-sonnet-4-7", "inputTokens": 400 }
                }),
            ],
            crlf,
        );
    }

    let bin = env!("CARGO_BIN_EXE_cctx");

    // 1. Dry-run preview does not create bpe-profile.json.
    let dry = Command::new(bin)
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .args([
            "prompt",
            "build-bpe",
            "--window",
            "7d",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["action"], "dry-run");
    assert_eq!(dry_json["applied"], false);
    assert!(!profile_path_in(&state).exists());

    // 2. Manual build persists bpe-profile.json.
    let built = Command::new(bin)
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .args([
            "prompt",
            "build-bpe",
            "--window",
            "7d",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let built_json: Value = serde_json::from_slice(&built.stdout).unwrap();
    assert_eq!(built_json["action"], "built");
    assert_eq!(built_json["applied"], true);
    assert!(profile_path_in(&state).is_file());

    // 3. Status reflects active profile and reports rebuild not immediately needed.
    let status = Command::new(bin)
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .args([
            "prompt",
            "bpe-status",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["exists"], true);
    assert_eq!(status_json["active"], true);
    assert_eq!(status_json["rebuild_check"]["needed"], false);

    // 4. `cctx prompt --compress` picks up the active learned profile and saves tokens.
    let comp = Command::new(bin)
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .args([
            "prompt",
            "--compress",
            "혹시 백엔드 지연 원인을 분석해서 정리해줘 부탁해",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(comp.status.success());
    let comp_json: Value = serde_json::from_slice(&comp.stdout).unwrap();
    assert_eq!(comp_json["bpe_profile_applied"], true);
    assert!(comp_json["output_estimated_tokens"].as_u64().unwrap() < comp_json["input_estimated_tokens"].as_u64().unwrap());

    // 5. Conditional `--if-needed` skips immediately after build.
    let skipped = Command::new(bin)
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .args([
            "prompt",
            "build-bpe",
            "--if-needed",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(skipped.status.success());
    let skipped_json: Value = serde_json::from_slice(&skipped.stdout).unwrap();
    assert_eq!(skipped_json["action"], "skipped");
    assert_eq!(skipped_json["applied"], false);

    // 6. Reset + rollback preserve recoverability.
    let reset = Command::new(bin)
        .env("CCTX_HOME", &state)
        .args(["prompt", "build-bpe", "--reset", "--json"])
        .output()
        .unwrap();
    assert!(reset.status.success());
    assert!(!profile_path_in(&state).exists());

    let rollback = Command::new(bin)
        .env("CCTX_HOME", &state)
        .args(["prompt", "build-bpe", "--rollback", "--json"])
        .output()
        .unwrap();
    assert!(rollback.status.success());
    assert!(profile_path_in(&state).is_file());
}

#[test]
fn tampered_profile_on_disk_is_sanitized_and_boundary_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    let now_ts = current_unix_secs().saturating_sub(300);
    for sid in ["s1", "s2"] {
        write_copilot_session(
            &sessions,
            sid,
            &[json!({
                "type": "user.message",
                "timestamp": now_ts,
                "data": { "content": "혹시 테스트 코드를 분석해서 정리해줘 부탁해" }
            })],
            false,
        );
    }

    let cfg = BpeBuildConfig {
        session_dirs: vec![sessions],
        state_dir_override: Some(state.clone()),
        min_freq: 2,
        min_sessions: 2,
        ..BpeBuildConfig::default()
    };
    let report = build_bpe_profile(&cfg).unwrap();
    let mut profile: BpeProfile = report.profile.unwrap();

    // Inject malicious/unsafe rules into persisted JSON on disk.
    profile.learned_fillers.push("do not".to_string());
    profile.learned_fillers.push("ghp_1234567890abcdef".to_string());
    profile.learned_fillers.push("한번".to_string());
    profile.learned_replacements.push(LearnedReplacement {
        from: "do not modify production".to_string(),
        to: "modify".to_string(),
        frequency: 10,
        session_count: 5,
        tokens_saved: 3,
    });
    profile.learned_replacements.push(LearnedReplacement {
        from: "check whether".to_string(),
        to: "check".to_string(),
        frequency: 5,
        session_count: 3,
        tokens_saved: 1,
    });
    let path = profile_path_in(&state);
    std::fs::write(&path, serde_json::to_vec_pretty(&profile).unwrap()).unwrap();

    let sanitized = read_profile_file(&path).unwrap().unwrap();
    assert!(!sanitized.learned_fillers.iter().any(|f| f == "do not"));
    assert!(!sanitized.learned_fillers.iter().any(|f| f.contains("ghp_")));
    assert!(!sanitized.learned_replacements.iter().any(|r| r.from.contains("do not")));

    // Boundary safety: `"한번"` must NOT corrupt `"제한번"` and `"check whether"` must NOT corrupt `"recheck whether"`.
    let out_ko = compress_task_prompt_with_profile("제한번 값을 확인해", None, Some(&sanitized));
    assert!(out_ko.contains("제한번"), "unexpected mid-word corruption: {out_ko}");

    let out_en = compress_task_prompt_with_profile("recheck whether cache hits", None, Some(&sanitized));
    assert!(out_en.contains("recheck whether"), "unexpected word prefix match: {out_en}");
    assert!(estimate_bpe_tokens(&out_en) <= estimate_bpe_tokens("recheck whether cache hits"));

    // Reset + rollback on missing backup fails cleanly.
    let _ = reset_bpe_profile(Some(&state)).unwrap();
    let _ = std::fs::remove_file(state.join("bpe-profile.prev.json"));
    assert!(rollback_bpe_profile(Some(&state)).is_err());
}

#[test]
fn cross_platform_windows_drive_path_normalization_and_concurrent_builds() {
    #[cfg(not(windows))]
    {
        let norm = normalize_cross_platform_dir(Path::new(r"C:\Users\Alice\.copilot\session-state"));
        assert_eq!(
            norm.to_string_lossy(),
            "/mnt/c/Users/Alice/.copilot/session-state"
        );
    }
    #[cfg(windows)]
    {
        let norm = normalize_cross_platform_dir(Path::new(r"C:\Users\Alice\.copilot\session-state"));
        assert_eq!(
            norm.to_string_lossy(),
            r"C:\Users\Alice\.copilot\session-state"
        );
    }

    // Concurrent multi-thread builds against the same state_dir must never corrupt JSON.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let now_ts = current_unix_secs().saturating_sub(300);
    for sid in ["c1", "c2", "c3"] {
        write_copilot_session(
            &sessions,
            sid,
            &[json!({
                "type": "user.message",
                "timestamp": now_ts,
                "data": { "content": "혹시 병목을 분석해서 정리해줘 부탁해" }
            })],
            false,
        );
    }

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let s_dir = sessions.clone();
            let st_dir = state.clone();
            std::thread::spawn(move || {
                let cfg = BpeBuildConfig {
                    source: BpeSource::Copilot,
                    session_dirs: vec![s_dir],
                    state_dir_override: Some(st_dir),
                    if_needed: i % 2 == 1,
                    min_freq: 2,
                    min_sessions: 2,
                    ..BpeBuildConfig::default()
                };
                build_bpe_profile(&cfg).unwrap()
            })
        })
        .collect();
    for h in handles {
        let rep = h.join().unwrap();
        assert!(matches!(rep.action.as_str(), "built" | "skipped"));
    }
    let final_prof = read_profile_file(&profile_path_in(&state)).unwrap();
    assert!(final_prof.is_some());
}

#[cfg(unix)]
#[test]
fn daemon_bpe_status_build_and_validation_rpc() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let home = tmp.path().join("home");
    let sessions = home.join(".copilot").join("session-state");
    std::fs::create_dir_all(&sessions).unwrap();

    let now_ts = current_unix_secs().saturating_sub(120);
    for sid in ["d1", "d2", "d3"] {
        write_copilot_session(
            &sessions,
            sid,
            &[json!({
                "type": "user.message",
                "timestamp": now_ts,
                "data": { "content": "혹시 데몬 지연 원인을 분석해서 정리해줘 부탁해" }
            })],
            false,
        );
    }

    let child = Command::new(env!("CARGO_BIN_EXE_cctx"))
        .env("CCTX_HOME", &state)
        .env("HOME", &home)
        .env("CCTX_BPE_AUTO_BUILD", "0")
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = Guard(child);

    let sock_path = state.join("daemon.sock");
    let rpc = |name: &str, arguments: Value| -> Result<Value, String> {
        let mut stream = UnixStream::connect(&sock_path).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let req = json!({ "root": null, "name": name, "arguments": arguments, "compact": true });
        writeln!(stream, "{req}").map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let parsed: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if let Some(err) = parsed.get("error").and_then(Value::as_str) {
            Err(err.to_string())
        } else {
            Ok(parsed["result"].clone())
        }
    };

    let start = Instant::now();
    while rpc("ping", json!({})).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }

    // Status before build
    let st = rpc("code_bpe_status", json!({ "window": "7d" })).unwrap();
    assert_eq!(st["exists"], false);

    // Invalid arguments are rejected strictly
    assert!(rpc("code_bpe_build", json!({ "hours": -1 })).is_err());
    assert!(rpc("code_bpe_build", json!({ "reset": true, "rollback": true })).is_err());
    assert!(rpc("code_bpe_build", json!({ "min_freq": 1 })).is_err());
    assert!(rpc("code_bpe_build", json!({ "if_needed": "yes" })).is_err());

    // Manual build through daemon RPC
    let built = rpc("code_bpe_build", json!({ "window": "7d" })).unwrap();
    assert_eq!(built["action"], "built");
    assert_eq!(built["applied"], true);

    // Subsequent conditional build skips
    let skipped = rpc("code_bpe_build", json!({ "window": "7d", "if_needed": true })).unwrap();
    assert_eq!(skipped["action"], "skipped");
    assert_eq!(skipped["applied"], false);

    // Self-audit RPC returns clean pass (Critical=0, High=0, Medium=0)
    let audit = rpc("code_bpe_audit", json!({ "window": "7d" })).unwrap();
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["digest_valid"], true);
    assert_eq!(audit["counts"]["critical"], 0);
    assert_eq!(audit["counts"]["high"], 0);
    assert_eq!(audit["counts"]["medium"], 0);
}

#[test]
fn self_audit_detects_tampering_secrets_cycles_perms_and_symlinks_then_passes_after_remediation() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    let now_ts = current_unix_secs().saturating_sub(180);
    for sid in ["a1", "a2", "a3"] {
        write_copilot_session(
            &sessions,
            sid,
            &[json!({
                "type": "user.message",
                "timestamp": now_ts,
                "data": { "content": "혹시 병목 원인을 분석해서 정리해줘 부탁해" }
            })],
            false,
        );
    }

    let cfg = BpeBuildConfig {
        session_dirs: vec![sessions.clone()],
        state_dir_override: Some(state.clone()),
        min_freq: 2,
        min_sessions: 2,
        ..BpeBuildConfig::default()
    };

    // 1. Clean build passes self-audit with zero Critical/High/Medium findings.
    let rep = build_bpe_profile(&cfg).unwrap();
    assert!(rep.applied);
    let clean_audit = audit_bpe_profile(&cfg).unwrap();
    assert!(clean_audit.passed, "expected clean audit pass: {clean_audit:?}");
    assert_eq!(clean_audit.digest_valid, Some(true));
    assert_eq!(clean_audit.counts.critical, 0);
    assert_eq!(clean_audit.counts.high, 0);
    assert_eq!(clean_audit.counts.medium, 0);

    // 2. Tamper on disk with protected keyword violation, secret leak, cyclic rule, and unsafe auto_policy.
    let path = profile_path_in(&state);
    let mut prof: BpeProfile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    prof.learned_fillers.push("do not".to_string());
    prof.learned_fillers
        .push("postgres://admin:secret@db.internal:5432/prod".to_string());
    prof.learned_replacements.push(LearnedReplacement {
        from: "alpha beta".to_string(),
        to: "gamma delta".to_string(),
        frequency: 5,
        session_count: 3,
        tokens_saved: 1,
    });
    prof.learned_replacements.push(LearnedReplacement {
        from: "gamma delta".to_string(),
        to: "alpha beta".to_string(),
        frequency: 5,
        session_count: 3,
        tokens_saved: 1,
    });
    prof.auto_policy.min_interval_hours = 0.01;
    std::fs::write(&path, serde_json::to_vec_pretty(&prof).unwrap()).unwrap();

    let tampered_audit = audit_bpe_profile(&cfg).unwrap();
    assert!(!tampered_audit.passed);
    assert_eq!(tampered_audit.digest_valid, Some(false));
    let codes: Vec<&str> = tampered_audit
        .findings
        .iter()
        .map(|f| f.code.as_str())
        .collect();
    assert!(codes.contains(&"BPE_DIGEST_MISMATCH"), "codes={codes:?}");
    assert!(
        codes.contains(&"BPE_PROTECTED_KEYWORD_VIOLATION"),
        "codes={codes:?}"
    );
    assert!(codes.contains(&"BPE_SENSITIVE_DATA_LEAK"), "codes={codes:?}");
    assert!(
        codes.contains(&"BPE_CYCLIC_OR_DUPLICATE_RULE"),
        "codes={codes:?}"
    );
    assert!(codes.contains(&"BPE_INVALID_AUTO_POLICY"), "codes={codes:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        // Unsafe group/world-writable permissions detection.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let perm_audit = audit_bpe_profile(&cfg).unwrap();
        assert!(
            perm_audit
                .findings
                .iter()
                .any(|f| f.code == "BPE_UNSAFE_PERMISSIONS" && f.severity == AuditSeverity::Critical)
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Symlink attack detection & refusal.
        let decoy = tmp.path().join("decoy.json");
        std::fs::write(&decoy, b"{}").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&decoy, &path).unwrap();
        let sym_audit = audit_bpe_profile(&cfg).unwrap();
        assert!(
            sym_audit
                .findings
                .iter()
                .any(|f| f.code == "BPE_SYMLINK_FILE" && f.severity == AuditSeverity::Critical)
        );
        assert!(read_profile_file(&path).is_err());
        assert!(build_bpe_profile(&cfg).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    // 3. Rebuilding remediates all findings back to PASS (Critical=0, High=0, Medium=0).
    let rebuilt = build_bpe_profile(&cfg).unwrap();
    assert!(rebuilt.applied);
    let final_audit = audit_bpe_profile(&cfg).unwrap();
    assert!(final_audit.passed, "post-remediation audit failed: {final_audit:?}");
    assert_eq!(final_audit.counts.critical, 0);
    assert_eq!(final_audit.counts.high, 0);
    assert_eq!(final_audit.counts.medium, 0);
}

#[test]
fn windows_bom_uppercase_jsonl_unc_traversal_sensitive_and_xml_dos_hardening() {
    // 1. Path traversal rejection & Windows WSL UNC normalization.
    assert!(normalize_cross_platform_dir_checked(Path::new(r"C:\Users\..\..\etc\passwd")).is_none());
    #[cfg(not(windows))]
    {
        let unc = normalize_cross_platform_dir_checked(Path::new(
            r"\\wsl.localhost\Ubuntu-22.04\home\alice\.copilot\session-state",
        ))
        .unwrap();
        assert_eq!(unc.to_string_lossy(), "/home/alice/.copilot/session-state");
    }

    // 2. Expanded cloud secrets, DB URIs, Azure SAS, and Korean PII detection.
    for sample in [
        "use AIzaSyD1234567890abcdef1234567890abcd for maps",
        "key ASIAIOSFODNN7EXAMPLE here",
        "connect postgres://user:pass@localhost:5432/db",
        "url ?sv=2022&sig=abc123&se=2027-01-01",
        "token eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        "contact 010-1234-5678 please",
        "id 900101-1234567 check",
    ] {
        assert!(
            detect_sensitive_patterns(sample).is_some(),
            "missed sensitive sample: {sample}"
        );
        assert!(sanitize_user_prompt(sample).is_none());
    }
    // Normal word "Asia" must NOT trigger false positive AWS STS detection.
    assert!(detect_sensitive_patterns("deploy to asia northeast region").is_none());

    // 3. Adversarial case-varied nested XML tags finish quickly in O(N) without leaking inner text.
    let mut evil_xml = String::new();
    for _ in 0..400 {
        evil_xml.push_str("<SYSTEM_REMINDER>hidden_payload</system_reminder>");
    }
    evil_xml.push_str(" 백엔드 성능을 분석해서 보고해줘");
    let cleaned = sanitize_user_prompt(&evil_xml);
    if let Some(c) = cleaned {
        assert!(!c.contains("hidden_payload"));
    }

    // 4. Windows UTF-8 BOM + CRLF + uppercase `EVENTS.JSONL` ingestion.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let sessions = tmp.path().join("sessions");
    let now_ts = current_unix_secs().saturating_sub(60);
    for sid in ["win1", "win2"] {
        let sdir = sessions.join(sid);
        std::fs::create_dir_all(&sdir).unwrap();
        let line = json!({
            "type": "user.message",
            "timestamp": now_ts,
            "data": { "content": "혹시 윈도우 로그를 확인해서 보고해줘 부탁해" }
        })
        .to_string();
        let body = format!("\u{FEFF}{line}\r\n");
        std::fs::write(sdir.join("EVENTS.JSONL"), body).unwrap();
    }

    let cfg = BpeBuildConfig {
        session_dirs: vec![sessions],
        state_dir_override: Some(state.clone()),
        min_freq: 2,
        min_sessions: 2,
        ..BpeBuildConfig::default()
    };
    let rep = build_bpe_profile(&cfg).unwrap();
    assert!(rep.applied);
    let prof = rep.profile.unwrap();
    assert_eq!(prof.stats.messages_kept, 2);
    assert!(audit_bpe_profile(&cfg).unwrap().passed);
}
