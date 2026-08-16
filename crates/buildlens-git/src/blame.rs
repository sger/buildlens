//! Running git and interpreting what it says.
//!
//! The porcelain *parsing* is a pure function over the output text, so it is
//! tested against captured fixtures rather than by running git.

use crate::GitError;
use std::path::Path;
use std::process::Command;

/// One commit's authorship, as blame reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameRecord {
    pub author: String,
    pub author_email: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
    pub commit: String,
    pub subject: String,
}

/// A repo path that is valid UTF-8, so it can be handed to `git -C`.
///
/// `to_str().unwrap_or(".")` appeared five times in an earlier version; a
/// non-UTF-8 repo path silently became the *current directory*, running git
/// against whatever repo the process happened to be in.
pub fn repo_arg(repo: &Path) -> Result<&str, GitError> {
    repo.to_str()
        .ok_or_else(|| GitError::NonUtf8Path(repo.to_path_buf()))
}

/// Runs a git subcommand, returning stdout. `Ok(None)` when git itself ran but
/// reported failure — an expected outcome for blame on an untracked file.
fn run(repo: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_arg(repo)?)
        .args(args)
        .output()
        .map_err(|error| GitError::Spawn(error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?))
}

/// Same as [`run`], but a non-zero exit is an error rather than an absence.
fn run_strict(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_arg(repo)?)
        .args(args)
        .output()
        .map_err(|error| GitError::Spawn(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Blames one line of a file, or the file's first tracked line when no line is
/// given.
pub fn blame(repo: &Path, file: &str, line: Option<u32>) -> Result<Option<BlameRecord>, GitError> {
    let range;
    let mut args = vec!["blame", "--porcelain"];
    if let Some(line) = line {
        // Git's -L is 1-based; a zero would make this call fail.
        debug_assert!(line > 0, "blame line must be 1-based");
        range = format!("{line},{line}");
        args.push("-L");
        args.push(&range);
    }
    args.push("--");
    args.push(file);

    let Some(text) = run(repo, &args)? else {
        return Ok(None);
    };
    let Some(mut record) = parse_porcelain(&text) else {
        return Ok(None);
    };
    // Prefer real ISO-8601 dates over blame's epoch-plus-offset pair.
    if let Some((authored, committed)) = commit_dates(repo, &record.commit)? {
        record.authored_at = Some(authored);
        record.committed_at = Some(committed);
    }
    Ok(Some(record))
}

/// Parses `git blame --porcelain` output.
///
/// The header is `<40-hex sha> <orig-line> <final-line>[ <count>]`, matched
/// exactly. An earlier version took "the first line with three or more
/// whitespace-separated fields", which also describes `summary Fix the thing`
/// and worked only because the header happens to come first.
pub fn parse_porcelain(text: &str) -> Option<BlameRecord> {
    let mut commit = String::new();
    let mut author = String::new();
    let mut author_email = None;
    let mut author_time = None;
    let mut author_tz = None;
    let mut committer_time = None;
    let mut committer_tz = None;
    let mut subject = String::new();

    for line in text.lines() {
        if commit.is_empty()
            && let Some(sha) = header_sha(line)
        {
            commit = sha;
            continue;
        }
        if let Some(value) = line.strip_prefix("author ") {
            author = value.to_owned();
        } else if let Some(value) = line.strip_prefix("author-mail ") {
            author_email = Some(value.trim_matches(['<', '>']).to_owned());
        } else if let Some(value) = line.strip_prefix("author-time ") {
            author_time = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("author-tz ") {
            author_tz = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("committer-time ") {
            committer_time = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("committer-tz ") {
            committer_tz = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("summary ") {
            subject = value.to_owned();
        }
    }

    if commit.is_empty() {
        return None;
    }
    Some(BlameRecord {
        author,
        author_email,
        authored_at: combine_timestamp(author_time, author_tz),
        committed_at: combine_timestamp(committer_time, committer_tz),
        commit,
        subject,
    })
}

/// Returns the SHA when a line is a porcelain header: a 40-character hex SHA
/// followed by at least two numeric fields.
fn header_sha(line: &str) -> Option<String> {
    let mut fields = line.split(' ');
    let sha = fields.next()?;
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let numeric = fields
        .take(2)
        .filter(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
        .count();
    (numeric == 2).then(|| sha.to_owned())
}

/// Asks git for a commit's author and committer dates in ISO-8601.
fn commit_dates(repo: &Path, commit: &str) -> Result<Option<(String, String)>, GitError> {
    let Some(text) = run(repo, &["show", "-s", "--format=%aI%x09%cI", commit])? else {
        return Ok(None);
    };
    let mut parts = text.trim().split('\t');
    match (parts.next(), parts.next()) {
        (Some(authored), Some(committed)) if !authored.is_empty() && !committed.is_empty() => {
            Ok(Some((authored.to_owned(), committed.to_owned())))
        }
        _ => Ok(None),
    }
}

/// Joins blame's epoch seconds and timezone offset into one string. Used only
/// when [`commit_dates`] could not supply a proper ISO-8601 date.
fn combine_timestamp(timestamp: Option<String>, timezone: Option<String>) -> Option<String> {
    let timestamp = timestamp?;
    Some(match timezone {
        Some(zone) => format!("{timestamp} {zone}"),
        None => timestamp,
    })
}

/// The files changed between two revisions.
pub fn changed_files(repo: &Path, base: &str, head: &str) -> Result<Vec<String>, GitError> {
    let range = format!("{base}...{head}");
    let text = run_strict(repo, &["diff", "--name-only", &range])?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from `git blame --porcelain -L 3,3` on a real repository.
    const PORCELAIN: &str = "\
83062b4c7c63d3dce3bdbbd5d19f2489336c7dba 3 3 1
author Spiros Gerokostas
author-mail <spiros.gerokostas@gmail.com>
author-time 1786860989
author-tz +0300
committer Spiros Gerokostas
committer-mail <spiros.gerokostas@gmail.com>
committer-time 1786860989
committer-tz +0300
summary fix: add crate targets so cargo build --workspace succeeds
filename Cargo.toml
\tresolver = \"2\"
";

    #[test]
    fn parses_a_real_porcelain_record() {
        let record = parse_porcelain(PORCELAIN).unwrap();
        assert_eq!(record.commit, "83062b4c7c63d3dce3bdbbd5d19f2489336c7dba");
        assert_eq!(record.author, "Spiros Gerokostas");
        assert_eq!(
            record.author_email.as_deref(),
            Some("spiros.gerokostas@gmail.com")
        );
        assert_eq!(
            record.subject,
            "fix: add crate targets so cargo build --workspace succeeds"
        );
        assert_eq!(record.authored_at.as_deref(), Some("1786860989 +0300"));
        assert_eq!(record.committed_at.as_deref(), Some("1786860989 +0300"));
    }

    /// The old heuristic — "first line with 3+ fields" — would pick up a
    /// summary line as a SHA if the header were ever absent or reordered.
    #[test]
    fn a_summary_line_is_not_mistaken_for_a_header() {
        let text = "summary fix the thing\nauthor Someone\n";
        assert_eq!(parse_porcelain(text), None);
    }

    #[test]
    fn a_short_or_non_hex_sha_is_not_a_header() {
        assert_eq!(header_sha("83062b4 3 3 1"), None);
        assert_eq!(
            header_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz 3 3"),
            None
        );
        assert_eq!(header_sha("author Someone Else"), None);
    }

    #[test]
    fn a_valid_header_is_recognized_with_or_without_the_group_count() {
        let sha = "83062b4c7c63d3dce3bdbbd5d19f2489336c7dba";
        assert_eq!(header_sha(&format!("{sha} 3 3 1")), Some(sha.to_owned()));
        assert_eq!(header_sha(&format!("{sha} 12 40")), Some(sha.to_owned()));
    }

    #[test]
    fn empty_output_yields_no_record() {
        assert_eq!(parse_porcelain(""), None);
    }

    #[test]
    fn a_missing_timezone_still_yields_a_timestamp() {
        assert_eq!(
            combine_timestamp(Some("123".into()), None).as_deref(),
            Some("123")
        );
        assert_eq!(combine_timestamp(None, Some("+0300".into())), None);
    }

    #[test]
    fn a_non_utf8_repo_path_is_an_error_not_the_current_directory() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe"));
        assert!(matches!(repo_arg(bad), Err(GitError::NonUtf8Path(_))));
    }
}
