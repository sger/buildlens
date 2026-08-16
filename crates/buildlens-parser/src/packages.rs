//! Swift package resolution lines.

use buildlens_core::PackageInfo;
use regex::Regex;
use std::sync::OnceLock;

/// `Resolved package: name: url @ version` — the form `swift package resolve`
/// prints. The `: ` between the label and the name was missing from an earlier
/// pattern, so every line with this prefix silently failed to parse even
/// though the caller explicitly gated on it.
fn resolved_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^Resolved package:\s*([A-Za-z0-9_.-]+)(?:\s*:\s*(\S+))?(?:\s*@\s*(\S+))?\s*$")
            .expect("valid resolved-package regex")
    })
}

/// `+ name url version` — xcodebuild's package listing.
fn listed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\+\s*([A-Za-z0-9_.-]+)(?:\s+(https?://\S+|/\S+))?(?:\s+([0-9]+\.[0-9]+(?:\.[0-9]+)?))?\s*$",
        )
        .expect("valid listed-package regex")
    })
}

/// `name: url @ version` — the resolved-source-packages block.
fn colon_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([A-Za-z0-9_.-]+):\s+(https?://\S+|/\S+?)(?:\s+@\s+([0-9]\S*))?\s*$")
            .expect("valid colon-package regex")
    })
}

/// Parses one package line, or `None` when the line names no package.
pub fn parse(line: &str) -> Option<PackageInfo> {
    let trimmed = line.trim();
    let captures = resolved_re()
        .captures(trimmed)
        .or_else(|| listed_re().captures(trimmed))
        .or_else(|| colon_re().captures(trimmed))?;
    Some(PackageInfo {
        name: captures[1].to_owned(),
        source: captures.get(2).map(|value| value.as_str().to_owned()),
        version: captures.get(3).map(|value| value.as_str().to_owned()),
    })
}

/// True for lines worth handing to [`parse`], so the regexes are not run over
/// every line of a large log.
pub fn looks_like_package_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Resolved package:")
        || trimmed.starts_with('+')
        || trimmed.contains(": https://")
        || trimmed.contains(": /")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caller gates on this prefix, so it must parse — it did not.
    #[test]
    fn parses_a_resolved_package_line() {
        let package = parse("Resolved package: swift-argument-parser").unwrap();
        assert_eq!(package.name, "swift-argument-parser");
    }

    #[test]
    fn parses_a_resolved_package_with_source_and_version() {
        let package =
            parse("Resolved package: SwiftSyntax: https://github.com/apple/swift-syntax @ 509.0.0")
                .unwrap();
        assert_eq!(package.name, "SwiftSyntax");
        assert_eq!(
            package.source.as_deref(),
            Some("https://github.com/apple/swift-syntax")
        );
        assert_eq!(package.version.as_deref(), Some("509.0.0"));
    }

    #[test]
    fn parses_the_listed_form_with_a_remote_source() {
        let package =
            parse("+ Firebase https://github.com/firebase/firebase-ios-sdk.git 12.17.0").unwrap();
        assert_eq!(package.name, "Firebase");
        assert_eq!(
            package.source.as_deref(),
            Some("https://github.com/firebase/firebase-ios-sdk.git")
        );
        assert_eq!(package.version.as_deref(), Some("12.17.0"));
    }

    #[test]
    fn parses_the_listed_form_with_a_local_path() {
        let package = parse("+ LocalKit /Users/ci/LocalKit 1.0.0").unwrap();
        assert_eq!(package.name, "LocalKit");
        assert_eq!(package.source.as_deref(), Some("/Users/ci/LocalKit"));
        assert_eq!(package.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn parses_the_colon_form() {
        let package =
            parse("swift-collections: https://github.com/apple/swift-collections @ 1.1.0").unwrap();
        assert_eq!(package.name, "swift-collections");
        assert_eq!(package.version.as_deref(), Some("1.1.0"));
    }

    #[test]
    fn a_version_is_optional() {
        let package = parse("+ SomePackage https://example.com/pkg").unwrap();
        assert_eq!(package.name, "SomePackage");
        assert_eq!(package.version, None);
    }

    /// The section header names no package and must not become one.
    #[test]
    fn a_header_line_is_not_a_package() {
        assert!(parse("Resolved source packages:").is_none());
    }

    #[test]
    fn ordinary_log_lines_are_not_packages() {
        assert!(parse("Compiling Foo.swift").is_none());
        assert!(parse("").is_none());
        assert!(parse("error: something went wrong").is_none());
    }

    #[test]
    fn the_gate_admits_every_supported_form() {
        for line in [
            "Resolved package: Foo",
            "+ Firebase https://github.com/x 1.0.0",
            "swift-collections: https://github.com/apple/swift-collections @ 1.1.0",
            "LocalKit: /Users/ci/LocalKit",
        ] {
            assert!(looks_like_package_line(line), "gate rejected {line:?}");
        }
    }

    #[test]
    fn the_gate_rejects_ordinary_lines() {
        assert!(!looks_like_package_line("Compiling Foo.swift"));
        assert!(!looks_like_package_line("warning: deprecated"));
    }
}
