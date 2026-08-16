//! Local activity-log collection: find the newest `.xcactivitylog` under
//! DerivedData, wait until Xcode finishes writing it, and hand it to the
//! normal save pipeline. Nothing is copied or uploaded — logs are read in
//! place and only derived metrics reach PostgreSQL.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The log must keep the same size/mtime for this long to count as finished.
const STABLE_FOR: Duration = Duration::from_secs(1);
const POLL_EVERY: Duration = Duration::from_millis(250);

/// Every activity log under `root`, newest first. Unlike [`find_activity_logs`]
/// an empty result is not an error: a watcher polling a DerivedData that has
/// no builds yet is in a normal state, not a failed one.
pub fn list_activity_logs(root: &Path, project: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut logs = collect_candidates(root, project)?;
    logs.sort_by_key(|path| {
        std::cmp::Reverse(
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        )
    });
    Ok(logs)
}

/// Every build activity log under `root`, newest first; an empty result is an
/// error, which is what the one-shot commands want.
pub fn find_activity_logs(root: &Path, project: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut logs = collect_candidates(root, project)?;
    logs.sort_by_key(|path| {
        std::cmp::Reverse(
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        )
    });
    if logs.is_empty() {
        bail!(
            "no .xcactivitylog found under {} (project filter: {})",
            root.display(),
            project.unwrap_or("none")
        );
    }
    Ok(logs)
}

/// Newest build activity log under `root`.
///
/// `root` may be DerivedData itself, one `<Project>-<hash>` directory inside
/// it, a `Logs/Build` directory, or anything else containing activity logs at
/// those depths. `project` filters DerivedData entries by name prefix.
pub fn find_newest_activity_log(root: &Path, project: Option<&str>) -> Result<PathBuf> {
    Ok(find_activity_logs(root, project)?.remove(0))
}

/// Where a `<Project>-<hash>` directory keeps activity logs.
///
/// Two directories are read: the normal build logs, and the logs an
/// `xcodebuild archive` writes under
/// `Build/Intermediates.noindex/ArchiveIntermediates/<Scheme>/`. Archive
/// builds are real builds and are usually the slowest ones a team runs, so
/// omitting them hides exactly the builds worth measuring.
///
/// `Logs/Package` is deliberately not searched. Those logs are SPM dependency
/// resolution ("Resolve Package Graph") with no targets and no compiled files;
/// recording them would put a phantom entry next to real builds, which is what
/// `BuildMetrics::is_usable` guards against.
fn log_dirs_for(project_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_dir.join("Logs/Build")];
    let archives = project_dir.join("Build/Intermediates.noindex/ArchiveIntermediates");
    if let Ok(entries) = std::fs::read_dir(&archives) {
        dirs.extend(
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .map(|scheme| scheme.join("Logs/Build")),
        );
    }
    dirs.retain(|dir| dir.is_dir());
    dirs
}

fn collect_candidates(root: &Path, project: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let own = log_dirs_for(root);
    let search_dirs: Vec<PathBuf> = if !own.is_empty() {
        own
    } else if logs_in(root) {
        // Already a Logs/Build-style directory.
        vec![root.to_path_buf()]
    } else {
        // Treat as DerivedData: <Project>-<hash>/Logs/Build (plus archives).
        std::fs::read_dir(root)
            .with_context(|| format!("cannot read {}", root.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && project.is_none_or(|prefix| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with(prefix))
                    })
            })
            .flat_map(|path| log_dirs_for(&path))
            .collect()
    };
    for dir in search_dirs {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            candidates.extend(
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension().is_some_and(|ext| ext == "xcactivitylog")
                    }),
            );
        }
    }
    Ok(candidates)
}

fn logs_in(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.filter_map(|entry| entry.ok()).any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "xcactivitylog")
        })
    })
}

/// Waits until the log stops changing (Xcode writes it in one pass after the
/// build) and its gzip stream decompresses completely, or the timeout passes.
pub fn wait_until_stable(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<(u64, std::time::SystemTime)> = None;
    let mut stable_since: Option<Instant> = None;
    loop {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("cannot stat {}", path.display()))?;
        let current = (metadata.len(), metadata.modified()?);
        if last == Some(current) {
            let since = *stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= STABLE_FOR && gzip_complete(path)? {
                return Ok(());
            }
        } else {
            stable_since = None;
            last = Some(current);
        }
        if Instant::now() >= deadline {
            bail!(
                "log {} did not become stable within {:?} (Xcode may still be writing it)",
                path.display(),
                timeout
            );
        }
        std::thread::sleep(POLL_EVERY);
    }
}

/// A truncated gzip stream means Xcode has not finished the log yet.
fn gzip_complete(path: &Path) -> Result<bool> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut decoder = GzDecoder::new(std::io::BufReader::new(file));
    let mut sink = [0u8; 64 * 1024];
    loop {
        match decoder.read(&mut sink) {
            Ok(0) => return Ok(true),
            Ok(_) => {}
            Err(_) => return Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buildlens-collect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn finds_newest_log_across_derived_data_layout() {
        let root = scratch("newest");
        for (project, name, age_seconds) in [
            ("App-abc123", "old.xcactivitylog", 100),
            ("App-abc123", "new.xcactivitylog", 0),
            ("Other-zzz", "other.xcactivitylog", 50),
        ] {
            let dir = root.join(project).join("Logs/Build");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            let mtime = std::time::SystemTime::now() - Duration::from_secs(age_seconds);
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(mtime).unwrap();
        }
        let newest = find_newest_activity_log(&root, None).unwrap();
        assert!(newest.ends_with("App-abc123/Logs/Build/new.xcactivitylog"));
        let filtered = find_newest_activity_log(&root, Some("Other")).unwrap();
        assert!(filtered.ends_with("Other-zzz/Logs/Build/other.xcactivitylog"));
        assert!(find_newest_activity_log(&root, Some("Missing")).is_err());
    }

    /// `xcodebuild archive` writes its logs under ArchiveIntermediates rather
    /// than the project's own `Logs/Build`. Both directories must be scanned:
    /// reading only the first hides release builds, which are usually the
    /// slowest ones a team runs.
    #[test]
    fn finds_logs_from_archive_builds() {
        let root = scratch("archive");
        let normal = root.join("App-abc123/Logs/Build");
        std::fs::create_dir_all(&normal).unwrap();
        std::fs::write(normal.join("debug.xcactivitylog"), b"x").unwrap();

        let archived = root
            .join("App-abc123/Build/Intermediates.noindex/ArchiveIntermediates/AppScheme/Logs/Build");
        std::fs::create_dir_all(&archived).unwrap();
        let archive_log = archived.join("release.xcactivitylog");
        std::fs::write(&archive_log, b"x").unwrap();
        // Make the archive log the newest so it must win.
        std::fs::File::options()
            .write(true)
            .open(&archive_log)
            .unwrap()
            .set_modified(std::time::SystemTime::now())
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(normal.join("debug.xcactivitylog"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();

        let all = find_activity_logs(&root, None).unwrap();
        assert_eq!(all.len(), 2, "expected both the debug and archive logs: {all:?}");
        assert!(find_newest_activity_log(&root, None).unwrap().ends_with("release.xcactivitylog"));
    }

    /// `Logs/Package` holds SPM dependency-resolution logs — no targets, no
    /// compiled files. They must be skipped, or every package resolve would
    /// become a phantom build in history.
    /// A watcher polls before the first build exists. `find_activity_logs`
    /// treats that as an error, which made every scan report a failure; the
    /// listing variant must return an empty list instead.
    #[test]
    fn listing_an_empty_build_dir_is_not_an_error() {
        let dir = scratch("empty-listing");
        std::fs::create_dir_all(dir.join("App-abc/Logs/Build")).unwrap();
        assert!(list_activity_logs(&dir, None).unwrap().is_empty());
        assert!(find_activity_logs(&dir, None).is_err());

        let log = dir.join("App-abc/Logs/Build/a.xcactivitylog");
        std::fs::write(&log, gz(b"SLF0")).unwrap();
        assert_eq!(list_activity_logs(&dir, None).unwrap(), vec![log]);
    }

    #[test]
    fn package_resolution_logs_are_not_collected() {
        let root = scratch("packages");
        let packages = root.join("App-abc123/Logs/Package");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(packages.join("resolve.xcactivitylog"), b"x").unwrap();
        assert!(
            find_activity_logs(&root, None).is_err(),
            "package logs must not be treated as builds"
        );
    }

    #[test]
    fn stable_complete_gzip_passes_quickly() {
        let root = scratch("stable");
        let path = root.join("done.xcactivitylog");
        std::fs::write(&path, gz(b"SLF0 pretend")).unwrap();
        wait_until_stable(&path, Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn truncated_gzip_times_out() {
        let root = scratch("truncated");
        let path = root.join("partial.xcactivitylog");
        let full = gz(b"SLF0 pretend this is a longer body so truncation matters");
        std::fs::write(&path, &full[..full.len() / 2]).unwrap();
        let error = wait_until_stable(&path, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("did not become stable"));
    }
}
