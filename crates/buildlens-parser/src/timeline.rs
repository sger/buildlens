//! Classifying a log line into the build phase it belongs to.
//!
//! The timeline records one event per *transition*, so a build that compiles
//! for ten thousand lines contributes a single "compilation" entry rather than
//! ten thousand.

use buildlens_core::TimelineEvent;

/// The phases a log line can announce, in no particular order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    BuildTiming,
    PackageResolution,
    DependencyGraph,
    Compilation,
    Linking,
    TestExecution,
    TestRestart,
    BuildMetadata,
    Crash,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BuildTiming => "build_timing",
            Self::PackageResolution => "package_resolution",
            Self::DependencyGraph => "dependency_graph",
            Self::Compilation => "compilation",
            Self::Linking => "linking",
            Self::TestExecution => "test_execution",
            Self::TestRestart => "test_restart",
            Self::BuildMetadata => "build_metadata",
            Self::Crash => "crash",
        }
    }
}

/// Which phase a line belongs to, judged by its content alone.
///
/// Ordered most-specific-first, and returns at most one phase: a line
/// mentioning both a package and a target belongs to whichever is checked
/// first, deliberately, rather than emitting two events.
pub fn classify(line: &str) -> Option<Phase> {
    if line.contains("Resolve Package Graph") || line.contains("Resolved source packages") {
        Some(Phase::PackageResolution)
    } else if line.contains("ComputeTargetDependencyGraph") || line.contains("dependency graph") {
        Some(Phase::DependencyGraph)
    } else if line.contains("SwiftCompile") || line.contains("CompileSwift") {
        Some(Phase::Compilation)
    } else if line.contains("Ld ") || line.contains("Linker") {
        Some(Phase::Linking)
    } else if line.contains("Test Suite") || line.contains("Test Case") {
        Some(Phase::TestExecution)
    } else {
        None
    }
}

/// True for lines worth handing to the metadata parser.
pub fn looks_like_metadata(line: &str) -> bool {
    line.contains("-scheme")
        || line.contains("-workspace")
        || line.contains("-project")
        || line.contains("Xcode ")
        || line.contains("platform")
        || line.contains("Code Coverage")
}

/// Accumulates timeline events, emitting one only when the phase changes.
#[derive(Default)]
pub struct Timeline {
    events: Vec<TimelineEvent>,
    last: Option<Phase>,
}

impl Timeline {
    /// Records that `line` belongs to `phase`. A repeat of the current phase
    /// is ignored.
    pub fn record(&mut self, phase: Phase, line_number: u64, message: &str) {
        if self.last == Some(phase) {
            return;
        }
        self.events.push(TimelineEvent {
            phase: phase.as_str().to_owned(),
            line: line_number,
            message: message.to_owned(),
        });
        self.last = Some(phase);
    }

    pub fn into_events(self) -> Vec<TimelineEvent> {
        self.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_each_phase_by_its_marker() {
        assert_eq!(
            classify("Resolve Package Graph"),
            Some(Phase::PackageResolution)
        );
        assert_eq!(
            classify("ComputeTargetDependencyGraph"),
            Some(Phase::DependencyGraph)
        );
        assert_eq!(
            classify("SwiftCompile normal arm64"),
            Some(Phase::Compilation)
        );
        assert_eq!(classify("Ld /path/to/binary"), Some(Phase::Linking));
        assert_eq!(
            classify("Test Suite 'All tests' started"),
            Some(Phase::TestExecution)
        );
    }

    #[test]
    fn an_ordinary_line_belongs_to_no_phase() {
        assert_eq!(classify("just some output"), None);
        assert_eq!(classify(""), None);
    }

    /// One event per transition, not one per line — a long compile must not
    /// flood the timeline.
    #[test]
    fn repeated_lines_in_one_phase_record_a_single_event() {
        let mut timeline = Timeline::default();
        for line in 1..=100 {
            timeline.record(Phase::Compilation, line, "SwiftCompile");
        }
        let events = timeline.into_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].line, 1, "the first line of the phase is recorded");
    }

    #[test]
    fn a_phase_change_records_a_new_event() {
        let mut timeline = Timeline::default();
        timeline.record(Phase::Compilation, 1, "SwiftCompile");
        timeline.record(Phase::Linking, 2, "Ld");
        timeline.record(Phase::Compilation, 3, "SwiftCompile");
        let events = timeline.into_events();
        assert_eq!(events.len(), 3, "returning to a phase records it again");
        assert_eq!(events[2].phase, "compilation");
    }

    #[test]
    fn phase_names_are_stable_strings() {
        assert_eq!(Phase::BuildTiming.as_str(), "build_timing");
        assert_eq!(Phase::Crash.as_str(), "crash");
    }

    #[test]
    fn metadata_lines_are_recognized() {
        assert!(looks_like_metadata("xcodebuild -scheme App"));
        assert!(looks_like_metadata("Xcode 16.0"));
        assert!(!looks_like_metadata("Compiling Foo.swift"));
    }
}
