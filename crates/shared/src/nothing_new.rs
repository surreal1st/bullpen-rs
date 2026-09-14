//! A tool telling the server, not the model, that a run found nothing. Port
//! of `src/shared/nothingToReport.ts`.
//!
//! The marker goes in the tool RESULT, where the model also sees it - that is
//! fine, it reads as part of the instruction it is already being given.

/// A tool result carries this to say, structurally, that it found nothing
/// worth reporting - decided from the run record, never by reading prose.
pub const NOTHING_NEW: &str = "[[bullpen:nothing-new]]";

/// True when a tool result declared it had nothing worth reporting.
pub fn declared_nothing_new(tool_result: &str) -> bool {
    tool_result.contains(NOTHING_NEW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_nothing_new_reads_the_marker() {
        assert!(declared_nothing_new(
            "no new matches [[bullpen:nothing-new]]"
        ));
        assert!(!declared_nothing_new("3 new matches found"));
    }
}
