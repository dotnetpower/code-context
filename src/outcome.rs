pub fn classify(error: Option<&str>) -> &'static str {
    let Some(message) = error else { return "success"; };
    if message.contains("CACHE_MISS ") || message.contains("STALE_INDEX")
        || message.contains("reason=not_yet_indexed") {
        "cache_not_ready"
    } else if message.contains("READ_POLICY_DENIED") || message.contains("reason=outside_scope")
        || message.contains("reason=excluded_by_rule") || message.contains("reason=unsupported_glob")
        || message.contains("reason=ignored_by_discovery") || message.contains("reason=symlink_not_allowed")
        || message.contains("only non-hidden") {
        "policy_denied"
    } else if message.contains("READ_RANGE_OUT_OF_BOUNDS") || message.contains("reason=missing_on_disk")
        || message.contains("regex parse error") || message.contains("limit 1..64")
        || message.contains("invalid range") || message.contains("unknown field") {
        "invalid_request"
    } else {
        "system_error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
