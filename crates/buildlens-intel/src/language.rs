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

/// The remedy is splitting, not annotating.
///
/// An earlier version of this said an explicit type annotation "usually
/// removes most of it". Measured against swiftc, that is false: annotating a
/// mixed-type arithmetic chain took it from ~354ms to ~590ms, and annotating a
/// slow collection literal turned a 3.2s warning into a hard "unable to
/// type-check in reasonable time" error. The annotation is one more constraint
/// to reconcile against every operator overload, so it widens the solver's
/// search rather than pruning it. Breaking the expression into named
/// sub-expressions is what the compiler's own error text recommends, and it
/// took that same 3.2s case to zero warnings.
pub fn expression_hotspot(milliseconds: f64) -> String {
    format!(
        "this expression took {milliseconds:.0}ms to type-check on its own; splitting it into \
         named sub-expressions is what usually removes the cost"
    )
}

pub fn large_function_body(symbol: &str, milliseconds: f64) -> String {
    format!(
        "'{symbol}' took {milliseconds:.0}ms to type-check, more than the expressions inside it \
         account for; the cost is the size of the body rather than any one expression"
    )
}

pub fn concentrated_file(milliseconds: f64, share: u8) -> String {
    format!(
        "this file holds {share}% of its target's type-checking time ({milliseconds:.0}ms across \
         all sites)"
    )
}

pub fn target_dominated(share: u8, milliseconds: f64) -> String {
    format!(
        "type-checking is {share}% of this target's build time ({milliseconds:.0}ms); the \
         hotspots above are where it is spent"
    )
}

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
            &expression_hotspot(1200.0),
            &large_function_body("body", 900.0),
            &concentrated_file(4200.0, 61),
            &target_dominated(38, 4200.0),
        ]
        .join(" ")
        .to_lowercase();
        for banned in ["blame", "fault", "responsible", "culprit", "caused by"] {
            assert!(!all.contains(banned), "found banned word '{banned}'");
        }
    }
}
