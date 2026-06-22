//! Tool one-line summaries: take the upstream description's first line, or
//! optionally condense via an LLM (cached on disk).

use rmcp::model::Tool;

/// Produce a one-line summary from a tool's description (no LLM).
pub fn from_description(tool: &Tool) -> String {
    let desc = tool.description.as_deref().unwrap_or("").trim();
    let first = desc.lines().next().unwrap_or("").trim();
    truncate(first, 160)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long() {
        let s = "a".repeat(200);
        let t = truncate(&s, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
    }
}
