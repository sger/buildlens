//! Which commit a build came from.
//!
//! Branch and commit are recorded verbatim, unlike the author email this crate
//! pseudonymizes. That is deliberate: a commit SHA is the join key that makes a
//! regression traceable to a change, and a pseudonymized one would be useless.
//! A branch name may incidentally carry a person's name (`alice/PROJ-123`), so
//! it goes through the same redaction as every other value — enough to catch an
//! email or a path, not enough to disguise a naming convention. Teams that do
//! not want branch names leaving the machine should not enable this plugin.

use crate::{MetricsPlugin, PluginContext, PluginError, UNKNOWN};
use std::collections::BTreeMap;

pub struct GitContextPlugin;

impl MetricsPlugin for GitContextPlugin {
    fn name(&self) -> &'static str {
        "git"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        let repo = context.repo_root.to_string_lossy();
        let git = |args: &[&str]| -> String {
            let mut argv = vec!["-C", repo.as_ref()];
            argv.extend_from_slice(args);
            context
                .probe
                .run("git", &argv)
                .map(|out| out.trim().to_owned())
                .unwrap_or_else(|_| UNKNOWN.to_owned())
        };
        let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let dirty = match context
            .probe
            .run("git", &["-C", repo.as_ref(), "status", "--porcelain"])
        {
            Ok(output) => (!output.trim().is_empty()).to_string(),
            Err(_) => UNKNOWN.to_owned(),
        };
        Ok(BTreeMap::from([
            ("git.branch".to_owned(), branch),
            ("git.commit".to_owned(), commit),
            ("git.dirty".to_owned(), dirty),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProbe;
    use buildlens_core::BuildMetadata;
    use std::path::Path;

    #[test]
    fn collects_branch_commit_and_dirty_state() {
        let probe = FakeProbe::with(&[
            (
                "git",
                &["-C", ".", "rev-parse", "--abbrev-ref", "HEAD"],
                "main\n",
            ),
            ("git", &["-C", ".", "rev-parse", "HEAD"], "abc123\n"),
            ("git", &["-C", ".", "status", "--porcelain"], " M file.rs\n"),
        ]);
        let build = BuildMetadata::default();
        let context = PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build: &build,
            environment: None,
            user_metadata_path: None,
            tags: &Default::default(),
            probe: &probe,
        };
        let entries = GitContextPlugin.collect(&context).unwrap();
        assert_eq!(entries["git.branch"], "main");
        assert_eq!(entries["git.commit"], "abc123");
        assert_eq!(entries["git.dirty"], "true");
    }

    #[test]
    fn missing_git_yields_unknown() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let context = PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build: &build,
            environment: None,
            user_metadata_path: None,
            tags: &Default::default(),
            probe: &probe,
        };
        let entries = GitContextPlugin.collect(&context).unwrap();
        assert_eq!(entries["git.branch"], UNKNOWN);
        assert_eq!(entries["git.dirty"], UNKNOWN);
    }
}
