use buildlens_core::BuildMetadata;
use regex::Regex;
use std::{path::PathBuf, sync::OnceLock};
static RES: OnceLock<Vec<Regex>> = OnceLock::new();
pub fn parse(l: &str, m: &mut BuildMetadata) -> bool {
    let r = RES.get_or_init(|| {
        [
            r"(?:-scheme|Scheme:)\s+([^\s]+)",
            r"(?:-testPlan|Test Plan:)\s+([^\s]+)",
            r"-destination\s+(.+?)(?:\s+-\w+|$)",
            r"Xcode\s+([0-9]+(?:\.[0-9]+)+)",
            r"(?:SDK|sdk)\s*[:=]\s*([^\s]+)",
            r"-project\s+([^\s]+)",
            r"-workspace\s+([^\s]+)",
            r"-resultBundlePath\s+([^\s]+)",
            r"-xcconfig\s+([^\s]+)",
            r"platform(?:Name)?[:=]\s*([A-Za-z ]+)",
        ]
        .into_iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    let mut hit = false;
    macro_rules! s {
        ($x:expr,$i:expr) => {
            if $x.is_none() {
                if let Some(c) = r[$i].captures(l).and_then(|c| c.get(1)) {
                    $x = Some(c.as_str().trim_matches('"').into());
                    hit = true;
                }
            }
        };
    }
    macro_rules! p {
        ($x:expr,$i:expr) => {
            if $x.is_none() {
                if let Some(c) = r[$i].captures(l).and_then(|c| c.get(1)) {
                    $x = Some(PathBuf::from(c.as_str().trim_matches('"')));
                    hit = true;
                }
            }
        };
    }
    s!(m.scheme, 0);
    s!(m.test_plan, 1);
    s!(m.destination, 2);
    s!(m.xcode_version, 3);
    s!(m.sdk, 4);
    p!(m.project, 5);
    p!(m.workspace, 6);
    p!(m.result_bundle_path, 7);
    p!(m.xcconfig_path, 8);
    if l.contains("-enableCodeCoverage YES") {
        m.code_coverage_enabled = Some(true);
        hit = true
    }
    if l.contains("-enableCodeCoverage NO") {
        m.code_coverage_enabled = Some(false);
        hit = true
    }
    if l.contains("-disableAutomaticPackageResolution") {
        m.disable_automatic_package_resolution = true;
        hit = true
    }
    if m.platform.is_none()
        && let Some(c) = r[9].captures(l).and_then(|c| c.get(1))
    {
        m.platform = Some(c.as_str().trim().into());
        hit = true
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(lines: &[&str]) -> BuildMetadata {
        let mut metadata = BuildMetadata::default();
        for line in lines {
            parse(line, &mut metadata);
        }
        metadata
    }

    #[test]
    fn reads_the_scheme_and_project_from_an_invocation() {
        let metadata = parse_all(&["xcodebuild -project MyApp.xcodeproj -scheme App build"]);
        assert_eq!(metadata.scheme.as_deref(), Some("App"));
        assert_eq!(
            metadata.project.as_deref().and_then(|p| p.to_str()),
            Some("MyApp.xcodeproj")
        );
    }

    #[test]
    fn reads_a_workspace_and_xcode_version() {
        let metadata = parse_all(&["xcodebuild -workspace MyApp.xcworkspace", "Xcode 16.2"]);
        assert_eq!(
            metadata.workspace.as_deref().and_then(|p| p.to_str()),
            Some("MyApp.xcworkspace")
        );
        assert_eq!(metadata.xcode_version.as_deref(), Some("16.2"));
    }

    /// The first value wins, so a later mention does not overwrite the
    /// invocation that actually ran.
    #[test]
    fn the_first_value_for_a_field_is_kept() {
        let metadata = parse_all(&["-scheme First", "-scheme Second"]);
        assert_eq!(metadata.scheme.as_deref(), Some("First"));
    }

    /// Values are whitespace-delimited, so a quoted value containing a space
    /// is truncated at the space. Recorded as a known limit rather than a
    /// silent surprise: schemes with spaces are legal in Xcode, and supporting
    /// them needs a quote-aware tokenizer rather than a wider character class.
    #[test]
    fn a_quoted_value_keeps_only_its_first_word() {
        let metadata = parse_all(&["-scheme \"My App\""]);
        assert_eq!(metadata.scheme.as_deref(), Some("My"));
    }

    #[test]
    fn a_quoted_single_word_value_loses_its_quotes() {
        let metadata = parse_all(&["-scheme \"App\""]);
        assert_eq!(metadata.scheme.as_deref(), Some("App"));
    }

    #[test]
    fn an_ordinary_line_yields_nothing() {
        let mut metadata = BuildMetadata::default();
        assert!(!parse("Compiling Foo.swift", &mut metadata));
        assert!(metadata.scheme.is_none());
    }

    #[test]
    fn parse_reports_whether_it_found_anything() {
        let mut metadata = BuildMetadata::default();
        assert!(parse("-scheme App", &mut metadata));
        // Already set, so nothing new was recorded.
        assert!(!parse("-scheme App", &mut metadata));
    }
}
