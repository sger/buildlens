//! User-supplied metadata from `--metadata-file`.

use crate::{MetricsPlugin, PluginContext, PluginError};
use std::collections::BTreeMap;

/// Caps on the metadata file, mirroring the ones [`crate::tags`] applies to
/// `--tag`. Both are user-supplied string maps bound for the same storage, so
/// they get the same limits — the file previously had none at all and would
/// accept tens of megabytes.
pub const MAX_ENTRIES: usize = 64;
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_KEY: usize = crate::MAX_TAG_KEY;
pub const MAX_VALUE: usize = crate::MAX_TAG_VALUE;

/// Reads a flat string-to-string JSON object supplied via `--metadata-file`.
pub struct UserMetadataPlugin;

impl MetricsPlugin for UserMetadataPlugin {
    fn name(&self) -> &'static str {
        "user-file"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        let Some(path) = context.user_metadata_path else {
            return Ok(BTreeMap::new());
        };

        // Checked before reading, so an oversized file is never loaded.
        // Errors deliberately omit the path: they land in `warnings`, which
        // reaches the same output as everything else.
        let size = std::fs::metadata(path)
            .map_err(|error| PluginError::UserFile(format!("could not be read: {error}")))?
            .len();
        if size > MAX_FILE_BYTES {
            return Err(PluginError::UserFile(format!(
                "is {size} bytes, over the {MAX_FILE_BYTES}-byte limit"
            )));
        }

        let text = std::fs::read_to_string(path)
            .map_err(|error| PluginError::UserFile(format!("could not be read: {error}")))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| PluginError::UserFile(format!("is not valid JSON: {error}")))?;
        let serde_json::Value::Object(object) = value else {
            return Err(PluginError::UserFile("expected a JSON object".to_owned()));
        };
        if object.len() > MAX_ENTRIES {
            return Err(PluginError::UserFile(format!(
                "has {} entries, over the {MAX_ENTRIES} limit",
                object.len()
            )));
        }

        let mut entries = BTreeMap::new();
        for (key, value) in object {
            let serde_json::Value::String(text) = value else {
                return Err(PluginError::UserFile(format!(
                    "key '{key}' must be a string"
                )));
            };
            validate_key(&key)?;
            if text.len() > MAX_VALUE {
                return Err(PluginError::UserFile(format!(
                    "value for '{key}' exceeds {MAX_VALUE} characters"
                )));
            }
            entries.insert(format!("user.{key}"), text);
        }
        Ok(entries)
    }
}

/// Keys are restricted like tag keys: they become dashboard filters, and an
/// unconstrained key could carry a path or an email into a position that is
/// not redacted the way values are.
fn validate_key(key: &str) -> Result<(), PluginError> {
    if key.is_empty() {
        return Err(PluginError::UserFile("has an empty key".to_owned()));
    }
    if key.len() > MAX_KEY {
        return Err(PluginError::UserFile(format!(
            "key '{key}' exceeds {MAX_KEY} characters"
        )));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(PluginError::UserFile(format!(
            "key '{key}' may only contain letters, digits, '_' and '-'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProbe;
    use buildlens_core::BuildMetadata;
    use std::path::{Path, PathBuf};

    /// Writes a metadata file into a uniquely named scratch directory.
    fn with_file(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buildlens-user-file-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn collect(path: Option<&Path>) -> Result<BTreeMap<String, String>, PluginError> {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let tags = BTreeMap::new();
        UserMetadataPlugin.collect(&PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build: &build,
            environment: None,
            user_metadata_path: path,
            tags: &tags,
            probe: &probe,
        })
    }

    #[test]
    fn reads_a_flat_string_object_under_a_user_prefix() {
        let path = with_file("ok", r#"{"team":"payments","ticket":"PROJ-1"}"#);
        let entries = collect(Some(&path)).unwrap();
        assert_eq!(entries["user.team"], "payments");
        assert_eq!(entries["user.ticket"], "PROJ-1");
    }

    #[test]
    fn no_file_yields_no_entries() {
        assert!(collect(None).unwrap().is_empty());
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let error = collect(Some(Path::new("/definitely/not/here.json"))).unwrap_err();
        assert!(error.to_string().contains("could not be read"));
    }

    #[test]
    fn a_non_object_is_rejected() {
        let path = with_file("array", r#"["a","b"]"#);
        assert!(
            collect(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("expected a JSON object")
        );
    }

    #[test]
    fn invalid_json_is_rejected() {
        let path = with_file("bad-json", "{not json");
        assert!(
            collect(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("not valid JSON")
        );
    }

    #[test]
    fn a_non_string_value_is_rejected() {
        let path = with_file("number", r#"{"count":3}"#);
        assert!(
            collect(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("must be a string")
        );
    }

    /// The file used to accept 50 MB across 500 entries while `--tag` rejected
    /// a 65-character key.
    #[test]
    fn an_oversized_file_is_rejected_before_being_read() {
        let padding: String = std::iter::repeat_n('x', MAX_FILE_BYTES as usize).collect();
        let path = with_file("huge", &format!(r#"{{"k":"{padding}"}}"#));
        let error = collect(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("over the"), "{error}");
    }

    #[test]
    fn too_many_entries_are_rejected() {
        let entries: Vec<String> = (0..=MAX_ENTRIES)
            .map(|index| format!(r#""k{index}":"v""#))
            .collect();
        let path = with_file("many", &format!("{{{}}}", entries.join(",")));
        let error = collect(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("over the"), "{error}");
    }

    #[test]
    fn an_oversized_value_is_rejected() {
        let padding: String = std::iter::repeat_n('x', MAX_VALUE + 1).collect();
        let path = with_file("long-value", &format!(r#"{{"k":"{padding}"}}"#));
        let error = collect(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    /// Keys become dashboard filters and are not redacted as values are, so a
    /// key carrying a path or an email must be rejected outright.
    #[test]
    fn a_key_with_unexpected_characters_is_rejected() {
        for bad in [
            r#"{"/Users/alice":"x"}"#,
            r#"{"a@b.com":"x"}"#,
            r#"{"a b":"x"}"#,
        ] {
            let path = with_file("bad-key", bad);
            let error = collect(Some(&path)).unwrap_err();
            assert!(error.to_string().contains("may only contain"), "{error}");
        }
    }

    #[test]
    fn an_empty_key_is_rejected() {
        let path = with_file("empty-key", r#"{"":"x"}"#);
        assert!(
            collect(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("empty key")
        );
    }

    /// The path reaches `warnings`, which is user-visible output.
    #[test]
    fn errors_do_not_repeat_the_file_path() {
        let path = with_file("no-path-leak", "{not json");
        let error = collect(Some(&path)).unwrap_err().to_string();
        assert!(
            !error.contains("buildlens-user-file"),
            "error leaked the path: {error}"
        );
    }
}
