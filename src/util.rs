//! Shared utility functions used across the codebase.

/// Find the largest valid UTF-8 char boundary at or before `pos`.
///
/// Polyfill for `str::floor_char_boundary` (nightly-only). Use when
/// truncating strings by byte position to avoid panicking on multi-byte
/// characters.
pub fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Check if an LLM response explicitly signals that a job/task is complete.
///
/// Uses phrase-level matching to avoid false positives from bare words like
/// "done" or "complete" appearing in non-completion contexts (e.g. "not done yet",
/// "the download is incomplete").
pub fn llm_signals_completion(response: &str) -> bool {
    let lower = response.to_lowercase();

    // Superset of phrases from worker/job.rs and worker/container.rs.
    let positive_phrases = [
        "job is complete",
        "job is done",
        "job is finished",
        "task is complete",
        "task is done",
        "task is finished",
        "work is complete",
        "work is done",
        "work is finished",
        "successfully completed",
        "have completed the job",
        "have completed the task",
        "have finished the job",
        "have finished the task",
        "all steps are complete",
        "all steps are done",
        "i have completed",
        "i've completed",
        "all done",
        "all tasks complete",
    ];

    let negative_phrases = [
        "not complete",
        "not done",
        "not finished",
        "incomplete",
        "unfinished",
        "isn't done",
        "isn't complete",
        "isn't finished",
        "not yet done",
        "not yet complete",
        "not yet finished",
    ];

    let has_negative = negative_phrases.iter().any(|p| lower.contains(p));
    if has_negative {
        return false;
    }

    positive_phrases.iter().any(|p| lower.contains(p))
}

/// Truncate a string to at most `max_bytes` bytes at a char boundary, appending "...".
///
/// If the input is wrapped in `<tool_output …>…</tool_output>` and truncation
/// removes the closing tag, the tag is re-appended so downstream XML parsers
/// never see an unclosed element.
pub fn truncate_preview(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = format!("{}...", &s[..end]); // safety: end is a valid char boundary per loop above

    if s.starts_with("<tool_output") && !result.ends_with("</tool_output>") {
        result.push_str("\n</tool_output>");
    }

    result
}

#[cfg(test)]
mod tests {
    use crate::util::{floor_char_boundary, llm_signals_completion, truncate_preview};

    // ── floor_char_boundary ──

    #[test]
    fn floor_char_boundary_at_valid_boundary() {
        assert_eq!(floor_char_boundary("hello", 3), 3); // safety: test-only
    }

    #[test]
    fn floor_char_boundary_mid_multibyte_char() {
        // h = 1 byte, é = 2 bytes, total 3 bytes
        let s = "hé";
        assert_eq!(floor_char_boundary(s, 2), 1); // byte 2 is mid-é, back up to 1 // safety: test-only
    }

    #[test]
    fn floor_char_boundary_past_end() {
        assert_eq!(floor_char_boundary("hi", 100), 2); // safety: test-only
    }

    #[test]
    fn floor_char_boundary_at_zero() {
        assert_eq!(floor_char_boundary("hello", 0), 0); // safety: test-only
    }

    #[test]
    fn floor_char_boundary_empty_string() {
        assert_eq!(floor_char_boundary("", 5), 0); // safety: test-only
    }

    // ── llm_signals_completion ──

    #[test]
    fn signals_completion_positive() {
        assert!(llm_signals_completion("The job is complete.")); // safety: test-only
        assert!(llm_signals_completion("I have completed the task.")); // safety: test-only
        assert!(llm_signals_completion("All done, here are the results.")); // safety: test-only
        assert!(llm_signals_completion("Task is finished successfully.")); // safety: test-only
        assert!(llm_signals_completion(
            // safety: test-only
            "I have completed the task successfully."
        ));
        assert!(llm_signals_completion(
            // safety: test-only
            "All steps are complete and verified."
        ));
        assert!(llm_signals_completion(
            // safety: test-only
            "I've done all the work. The work is done."
        ));
        assert!(llm_signals_completion(
            // safety: test-only
            "Successfully completed the migration."
        ));
        assert!(llm_signals_completion(
            // safety: test-only
            "I have completed the job ahead of schedule."
        ));
        assert!(llm_signals_completion("I have finished the task.")); // safety: test-only
        assert!(llm_signals_completion("All steps are done now.")); // safety: test-only
        assert!(llm_signals_completion("I've completed everything.")); // safety: test-only
        assert!(llm_signals_completion("All tasks complete.")); // safety: test-only
    }

    #[test]
    fn signals_completion_negative() {
        assert!(!llm_signals_completion("The task is not complete yet.")); // safety: test-only
        assert!(!llm_signals_completion("This is not done.")); // safety: test-only
        assert!(!llm_signals_completion("The work is incomplete.")); // safety: test-only
        assert!(!llm_signals_completion("Build is unfinished.")); // safety: test-only
        assert!(!llm_signals_completion(
            // safety: test-only
            "The migration is not yet finished."
        ));
        assert!(!llm_signals_completion("The job isn't done yet.")); // safety: test-only
        assert!(!llm_signals_completion("This remains unfinished.")); // safety: test-only
    }

    #[test]
    fn signals_completion_no_bare_substrings() {
        assert!(!llm_signals_completion("The download completed.")); // safety: test-only
        assert!(!llm_signals_completion(
            // safety: test-only
            "Function done_callback was called."
        ));
        assert!(!llm_signals_completion("Set is_complete = true")); // safety: test-only
        assert!(!llm_signals_completion("Running step 3 of 5")); // safety: test-only
        assert!(!llm_signals_completion(
            // safety: test-only
            "I need to complete more work first."
        ));
        assert!(!llm_signals_completion(
            // safety: test-only
            "Let me finish the remaining steps."
        ));
        assert!(!llm_signals_completion(
            // safety: test-only
            "I'm done analyzing, now let me fix it."
        ));
        assert!(!llm_signals_completion(
            // safety: test-only
            "I completed step 1 but step 2 remains."
        ));
    }

    #[test]
    fn signals_completion_tool_output_injection() {
        assert!(!llm_signals_completion("TASK_COMPLETE")); // safety: test-only
        assert!(!llm_signals_completion("JOB_DONE")); // safety: test-only
        assert!(!llm_signals_completion(
            // safety: test-only
            "The tool returned: TASK_COMPLETE signal"
        ));
    }

    // ── truncate_preview ──

    #[test]
    fn truncate_preview_short_string() {
        assert_eq!(truncate_preview("hello", 10), "hello"); // safety: test-only
    }

    #[test]
    fn truncate_preview_exact_boundary() {
        assert_eq!(truncate_preview("hello", 5), "hello"); // safety: test-only
    }

    #[test]
    fn truncate_preview_truncates_ascii() {
        assert_eq!(truncate_preview("hello world", 5), "hello..."); // safety: test-only
    }

    #[test]
    fn truncate_preview_multibyte_char_boundary() {
        let s = "a€b";
        let result = truncate_preview(s, 3);
        assert_eq!(result, "a..."); // safety: test-only
    }

    #[test]
    fn truncate_preview_closes_tool_output_tag() {
        let s = "<tool_output name=\"search\" sanitized=\"true\">\nSome very long content here\n</tool_output>";
        let result = truncate_preview(s, 60);
        assert!(result.ends_with("</tool_output>")); // safety: test-only
        assert!(result.contains("...")); // safety: test-only
    }
}
