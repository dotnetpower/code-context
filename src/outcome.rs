pub fn classify(error: Option<&str>) -> &'static str {
    let Some(message) = error else { return "success"; };
    if message.starts_with("CACHE_MISS ")
        || message.starts_with("STALE_INDEX ")
        || message.contains("CACHE_MISS ")
        || message.contains("STALE_INDEX")
        || message.contains("reason=not_yet_indexed")
    {
        "cache_not_ready"
    } else if message.starts_with("READ_POLICY_DENIED ")
        || message.contains("READ_POLICY_DENIED")
        || message.contains("reason=outside_scope")
        || message.contains("reason=excluded_by_rule")
        || message.contains("reason=unsupported_glob")
        || message.contains("reason=ignored_by_discovery")
        || message.contains("reason=symlink_not_allowed")
        || message.contains("reason=exceeds_max_file_bytes")
        || message.contains("file exceeds configured max_file_bytes")
        || message.contains("binary file or file larger than configured limit is unsupported")
        || message.contains("only non-hidden")
    {
        "policy_denied"
    } else if message.starts_with("READ_RANGE_OUT_OF_BOUNDS ")
        || message.contains("READ_RANGE_OUT_OF_BOUNDS")
        || message.contains("RESPONSE_BUDGET_EXCEEDED")
        || message.contains("reason=missing_on_disk")
        || message.contains("regex parse error")
        || message.contains("limit 1..64")
        || message.contains("invalid range")
        || message.contains("unknown field")
    {
        "invalid_request"
    } else {
        "system_error"
    }
}

/// Fine-grained failure code used by `cctx usage` to explain why a call failed.
pub fn detail(error: Option<&str>) -> &'static str {
    let Some(message) = error else { return "success"; };
    if message.contains("CACHE_LOAD_FAILED") {
        "cache_load_failed"
    } else if message.contains("CACHE_MISS ") {
        "cache_miss"
    } else if message.contains("STALE_INDEX") {
        "stale_index"
    } else if message.contains("READ_RANGE_OUT_OF_BOUNDS") {
        "range_out_of_bounds"
    } else if message.contains("RESPONSE_BUDGET_EXCEEDED") {
        "response_budget_exceeded"
    } else if message.contains("READ_UNAVAILABLE") || message.contains("reason=missing_on_disk") {
        "file_unavailable"
    } else if classify(Some(message)) == "policy_denied" {
        "policy_denied"
    } else if message.contains("TARGET_NOT_INDEXED") || message.contains("reason=not_yet_indexed") {
        "target_not_indexed"
    } else if message.contains("No matching client connected")
        || message.contains("advertised tool definition")
        || message.contains("disconnected")
        || message.contains("reconnect failed")
        || message.contains("skipped from another client")
        || message.contains("not available on this client")
        || message.contains("clientUnavailable")
    {
        "client_disconnected"
    } else if message.contains("timed out") || message.contains("timeout") || message.contains("Cancelled") {
        "timeout_or_cancelled"
    } else if classify(Some(message)) == "invalid_request" {
        "invalid_request"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_names_actionable_failure_causes() {
        assert_eq!(detail(None), "success");
        assert_eq!(detail(Some("CACHE_MISS root=/a")), "cache_miss");
        assert_eq!(detail(Some("CACHE_LOAD_FAILED root=/a")), "cache_load_failed");
        assert_eq!(detail(Some("READ_RANGE_OUT_OF_BOUNDS path=a")), "range_out_of_bounds");
        assert_eq!(
            detail(Some("RESPONSE_BUDGET_EXCEEDED: response exceeds 256 KiB")),
            "response_budget_exceeded"
        );
        assert_eq!(
            detail(Some("READ_UNAVAILABLE root=/a: path=b.rs reason=missing_on_disk")),
            "file_unavailable"
        );
        assert_eq!(
            detail(Some("binary file or file larger than configured limit is unsupported: a.bin")),
            "policy_denied"
        );
        assert_eq!(detail(Some("TARGET_NOT_INDEXED reason=excluded_by_rule")), "policy_denied");
        assert_eq!(detail(Some("TARGET_NOT_INDEXED reason=not_yet_indexed")), "target_not_indexed");
        assert_eq!(detail(Some("READ_POLICY_DENIED")), "policy_denied");
        assert_eq!(detail(Some("No matching client connected")), "client_disconnected");
        assert_eq!(
            detail(Some(
                "No client with the advertised tool definition was connected to run cctxNativeFiles"
            )),
            "client_disconnected"
        );
        assert_eq!(
            detail(Some("cctxNativeRead was skipped from another client")),
            "client_disconnected"
        );
        assert_eq!(
            detail(Some("Tool \"cctxNativeRead\" is not available on this client")),
            "client_disconnected"
        );
        assert_eq!(detail(Some("request timed out")), "timeout_or_cancelled");
        assert_eq!(detail(Some("regex parse error")), "invalid_request");
        assert_eq!(detail(Some("boom")), "other");
    }

    #[test]
    fn misses_and_refusals_are_not_success_or_system_failures() {
        assert_eq!(classify(None), "success");
        assert_eq!(classify(Some("CACHE_MISS root=/a")), "cache_not_ready");
        assert_eq!(classify(Some("TARGET_NOT_INDEXED reason=not_yet_indexed")), "cache_not_ready");
        assert_eq!(classify(Some("TARGET_NOT_INDEXED reason=excluded_by_rule")), "policy_denied");
        assert_eq!(classify(Some("READ_POLICY_DENIED reason=unsupported_glob")), "policy_denied");
        assert_eq!(classify(Some("READ_RANGE_OUT_OF_BOUNDS")), "invalid_request");
        assert_eq!(classify(Some("READ_UNAVAILABLE reason=missing_on_disk")), "invalid_request");
        assert_eq!(classify(Some("CACHE_LOAD_FAILED: cannot allocate")), "system_error");
    }
}
