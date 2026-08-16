//! Local activity-log collection: find the newest `.xcactivitylog` under
//! DerivedData, wait until Xcode finishes writing it, and hand it to the
//! normal save pipeline. Nothing is copied or uploaded — logs are read in
//! place and only derived metrics reach PostgreSQL.
//!
//! # Finding the log
//!
//! Two ways in, deliberately sharing one implementation so a build collected
//! from a scheme post-action, a terminal `xcodebuild`, and CI all resolve the
//! same log the same way:
//!
//! 1. `$BUILD_DIR`, when Xcode set it — the case that needs no guessing,
//!    because Xcode is naming the build it just ran. See [`logs_dir_for_build_dir`].
//! 2. Otherwise a scan of DerivedData, which has to guess which
//!    `<Project>-<hash>` directory is relevant.
//!
//! The first is strictly better when available: it is exact, and it follows
//! `-derivedDataPath` to wherever the caller put it.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The log must keep the same size/mtime for this long to count as finished.
const STABLE_FOR: Duration = Duration::from_secs(1);
const POLL_EVERY: Duration = Duration::from_millis(250);

/// How far up from `$BUILD_DIR` to look for a `Logs/Build` sibling.
///
/// Deliberately a search rather than a fixed number of parent hops. The depth
/// genuinely varies — `<dd>/Build/Products` for a normal build,
/// `<dd>/Build/Intermediates.noindex/ArchiveIntermediates/<Scheme>/BuildProductsPath`
/// when archiving — and hard-coding either count silently resolves to a
/// non-existent directory under the other layout, which reads as "no builds
/// yet" rather than as a bug. Six covers both with room to spare.
const MAX_ASCENT: usize = 6;

/// Resolves the directory holding build logs from Xcode's `$BUILD_DIR`.
///
/// Walks up looking for a `Logs/Build` sibling and returns the first that
/// exists, so both the normal and archive layouts resolve without knowing
/// which one produced this build.
///
/// Returns `None` when no ancestor has one. That is the signature of a
/// default-DerivedData `xcodebuild` run, which writes products under the
/// shared `DerivedData/Build/` and no build log anywhere — see
/// [`shared_derived_data_hint`].
pub fn logs_dir_for_build_dir(build_dir: &Path) -> Option<PathBuf> {
    let mut current = build_dir;
    for _ in 0..MAX_ASCENT {
        let candidate = current.join("Logs/Build");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
    None
}

/// Explains the one failure that otherwise looks exactly like "no builds yet".
///
/// `xcodebuild` without `-derivedDataPath` puts products in the *shared*
/// `DerivedData/Build/` and writes no build activity log at all — only
/// `Logs/Package` entries for dependency resolution, which are not builds.
/// Xcode.app always writes one, so this bites only terminal and CI builds,
/// and the symptom is silence: the watcher polls forever and no build appears.
///
/// Returns a message when `root` shows that shape, so the caller can say what
/// to change instead of reporting an empty result.
pub fn shared_derived_data_hint(root: &Path) -> Option<String> {
    let build = root.join("Build");
    if !build.is_dir() || logs_in(&build) {
        return None;
    }
    // A shared `Build/` is only evidence of the broken case when nothing else
    // under this root has build logs. A developer's DerivedData routinely
    // holds both: per-project directories written by Xcode.app, and a stray
    // `Build/` left by some earlier `xcodebuild`. Warning there would tell
    // someone to add a flag for a problem they do not have, on every start.
    if !collect_candidates(root, None).is_ok_and(|logs| logs.is_empty()) {
        return None;
    }
    Some(format!(
        "{} contains a shared Build/ directory but no build logs. That is what \
         `xcodebuild` produces without `-derivedDataPath`: it writes products \
         there and no .xcactivitylog anywhere. Re-run with \
         `-derivedDataPath <path>` (Xcode.app builds are unaffected).",
        root.display()
    ))
}

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
        // The shared-DerivedData case is by far the most common reason for an
        // empty result, and the least guessable, so it is named rather than
        // left as "found nothing".
        if let Some(hint) = shared_derived_data_hint(root) {
            bail!("{hint}");
        }
        bail!(
            "no .xcactivitylog found under {} (project filter: {})",
            root.display(),
            project.unwrap_or("none")
        );
    }
    Ok(logs)
}

/// The search root, preferring what Xcode said over what we would guess.
///
/// When `$BUILD_DIR` is set — a scheme post-action, or any build script Xcode
/// spawned — it names the build that just finished, so the log it points at is
/// the right one by construction. This is what makes a post-action, a terminal
/// `xcodebuild` and CI resolve identically: the same environment variable, the
/// same ascent, no per-caller special cases.
///
/// `explicit` is the `--build-dir` flag. A caller who named a directory means
/// it, so it always wins; `$BUILD_DIR` only fills in for the default.
pub fn search_root(explicit: Option<&Path>, default_root: &Path) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(from_xcode) = std::env::var_os("BUILD_DIR")
        .map(PathBuf::from)
        .as_deref()
        .and_then(logs_dir_for_build_dir)
    {
        return from_xcode;
    }
    default_root.to_path_buf()
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

    // --- resolving $BUILD_DIR to a logs directory ---

    /// The normal build layout: `$BUILD_DIR` is `<dd>/Build/Products`, two
    /// levels below the `Logs/Build` sibling.
    #[test]
    fn resolves_the_normal_build_layout() {
        let root = scratch("bd-normal");
        std::fs::create_dir_all(root.join("Logs/Build")).unwrap();
        let build_dir = root.join("Build/Products");
        std::fs::create_dir_all(&build_dir).unwrap();
        assert_eq!(
            logs_dir_for_build_dir(&build_dir),
            Some(root.join("Logs/Build"))
        );
    }

    /// Archiving puts `$BUILD_DIR` five levels down instead of two. A fixed
    /// number of parent hops resolves one layout and silently misses the
    /// other, which is the bug this ascent exists to avoid.
    #[test]
    fn resolves_the_archive_layout_at_a_different_depth() {
        let root = scratch("bd-archive");
        std::fs::create_dir_all(root.join("Logs/Build")).unwrap();
        let build_dir =
            root.join("Build/Intermediates.noindex/ArchiveIntermediates/App/BuildProductsPath");
        std::fs::create_dir_all(&build_dir).unwrap();
        assert_eq!(
            logs_dir_for_build_dir(&build_dir),
            Some(root.join("Logs/Build"))
        );
    }

    /// The shared-DerivedData case: products exist, no `Logs/Build` anywhere
    /// above them. Must be `None` rather than a path that does not exist.
    #[test]
    fn a_missing_logs_directory_resolves_to_nothing() {
        let root = scratch("bd-missing");
        let build_dir = root.join("Build/Products");
        std::fs::create_dir_all(&build_dir).unwrap();
        assert_eq!(logs_dir_for_build_dir(&build_dir), None);
    }

    /// The ascent must stop rather than walking to the filesystem root and
    /// matching some unrelated `Logs/Build` far above the build.
    #[test]
    fn the_ascent_is_bounded() {
        let root = scratch("bd-bounded");
        std::fs::create_dir_all(root.join("Logs/Build")).unwrap();
        // Deeper than MAX_ASCENT below the directory holding Logs/Build.
        let mut deep = root.join("Build");
        for level in 0..MAX_ASCENT + 2 {
            deep = deep.join(format!("level{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(logs_dir_for_build_dir(&deep), None);
    }

    // --- choosing the search root ---

    /// An explicitly named directory always wins: a caller who passed
    /// `--build-dir` meant that directory, whatever Xcode's environment says.
    #[test]
    fn an_explicit_build_dir_beats_the_environment() {
        let explicit = PathBuf::from("/explicit/path");
        let chosen = search_root(Some(&explicit), Path::new("/default"));
        assert_eq!(chosen, explicit);
    }

    /// With nothing explicit and no usable `$BUILD_DIR`, the DerivedData root
    /// is the fallback — the behaviour before `$BUILD_DIR` was consulted.
    #[test]
    fn the_default_root_is_used_when_nothing_else_applies() {
        // SAFETY: single-threaded test; the variable is removed before return.
        unsafe { std::env::remove_var("BUILD_DIR") };
        assert_eq!(
            search_root(None, Path::new("/default")),
            PathBuf::from("/default")
        );
    }

    // --- the shared-DerivedData diagnostic ---

    /// A `Build/` directory with no logs is the shape `xcodebuild` leaves
    /// without `-derivedDataPath`, and the one case worth naming: the symptom
    /// is silence, and the fix is a flag nobody guesses.
    #[test]
    fn the_shared_derived_data_shape_is_explained() {
        let root = scratch("hint-shared");
        std::fs::create_dir_all(root.join("Build/Products")).unwrap();
        let hint = shared_derived_data_hint(&root).expect("the shared layout must be recognised");
        assert!(hint.contains("-derivedDataPath"), "{hint}");
    }

    /// A DerivedData with no `Build/` at all is just empty, not misconfigured,
    /// and must not be blamed on `xcodebuild`.
    #[test]
    fn an_ordinary_empty_root_gets_no_hint() {
        let root = scratch("hint-empty");
        assert_eq!(shared_derived_data_hint(&root), None);
    }

    /// A developer's DerivedData routinely holds both a stray `Build/` from
    /// some earlier `xcodebuild` and per-project directories Xcode.app filled
    /// with real logs. Warning there tells someone to fix a problem they do
    /// not have — and the watcher would repeat it on every start.
    #[test]
    fn a_stray_build_directory_beside_real_logs_gets_no_hint() {
        let root = scratch("hint-mixed");
        std::fs::create_dir_all(root.join("Build/Products")).unwrap();
        let project = root.join("App-abc123/Logs/Build");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.xcactivitylog"), b"x").unwrap();
        assert_eq!(
            shared_derived_data_hint(&root),
            None,
            "a root that already yields logs is not the shared-DerivedData case"
        );
    }

    /// A `Build/` that does hold logs is a normal layout, not the broken one.
    #[test]
    fn a_build_directory_holding_logs_gets_no_hint() {
        let root = scratch("hint-haslogs");
        let logs = root.join("Build");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("a.xcactivitylog"), b"x").unwrap();
        assert_eq!(shared_derived_data_hint(&root), None);
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
