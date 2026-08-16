//! Every user-facing sentence the intelligence layer produces is built from
//! these constants. The wording is deliberately non-blaming: it describes
//! historical context and correlation, never responsibility.

pub const CHANGED_FILE_BUILDS_INTO: &str = "a changed file builds into this target";
pub const CHANGED_FILE_NEAR: &str = "a changed file matches this target's sources";
pub const TARGET_REGRESSED: &str = "this target slowed down against its baseline";
pub const DIAGNOSTIC_IN_CHANGE: &str = "a diagnostic points into the changed files";
pub const FAILING_SUITE_MATCHES: &str = "a failing test suite maps to this target";
pub const ENVIRONMENT_SHIFTED: &str =
    "the build environment shifted since the baseline, which may explain the timing";
pub const BUILD_SUSPENDED: &str = "the machine slept during this build, so timings are unreliable";

pub fn chain_summary(subject: &str, confidence: u8) -> String {
    format!(
        "{subject} is the area most correlated with this build's changes and slowdowns \
         (confidence {confidence}%); the relevant implementation change and test locations \
         above are historical context, not an assignment of responsibility"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_blaming_vocabulary() {
        let all = [
            CHANGED_FILE_BUILDS_INTO,
            CHANGED_FILE_NEAR,
            TARGET_REGRESSED,
            DIAGNOSTIC_IN_CHANGE,
            FAILING_SUITE_MATCHES,
            ENVIRONMENT_SHIFTED,
            BUILD_SUSPENDED,
            &chain_summary("App", 80),
        ]
        .join(" ")
        .to_lowercase();
        for banned in ["blame", "fault", "responsible", "culprit", "caused by"] {
            assert!(!all.contains(banned), "found banned word '{banned}'");
        }
    }
}
