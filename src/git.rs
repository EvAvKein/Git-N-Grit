//! Every subprocess.
//! Spawning is separate from parsing so the parsers unit-test against fixture text.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, Command, Stdio};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};

/// What a path resolves to: the repo it belongs to, and which worktree of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    /// Every parentless commit. Sorted, so two resolutions of the same repo compare equal.
    /// Vector because a `gh-pages`-style orphan branch would create a second root.
    pub root_commits: Vec<String>,
    /// `--git-common-dir`: the object store, shared by every worktree of the repo.
    pub git_common_dir: PathBuf,
    /// The git dir, relative to `git_common_dir`, so joining the two reaches
    /// this worktree's own `HEAD` and `logs/HEAD`. Empty for a main worktree,
    /// `worktrees/<name>` for a linked one.
    pub head_subdir: PathBuf,
}

/// One commit of a `--numstat` stream, with its counts already summed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub author_email: String,
    pub committed_timestamp: i64,
    pub is_merge: bool,
    pub subject: String,
    pub added: u64,
    pub deleted: u64,
}

/// The repo `path` belongs to.
///
/// Spawns git against a path that may not answer, so the scan worker calls this
/// and the main thread never does — including for the add prompt.
pub fn resolve_identity(path: &Path) -> Result<RepoIdentity> {
    // One root commit per line.
    let mut root_commits: Vec<String> = capture(Command::new("git").arg("-C").arg(path).args([
        "rev-list",
        "--max-parents=0",
        "--all",
    ]))?
    .lines()
    .map(String::from)
    .collect();
    root_commits.sort();

    // Two absolute lines: the git dir, then the common dir.
    let dirs = capture(Command::new("git").arg("-C").arg(path).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
    ]))?;
    let mut lines = dirs.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
        bail!("git rev-parse returned {dirs:?}, expected two lines");
    };

    // Bail rather than fall back: an absolute suffix would silently swallow the
    // common dir on every join.
    let Ok(head_subdir) = Path::new(git_dir).strip_prefix(common) else {
        bail!("git dir {git_dir:?} is not under common dir {common:?}");
    };

    Ok(RepoIdentity {
        root_commits,
        git_common_dir: PathBuf::from(common),
        head_subdir: head_subdir.to_path_buf(),
    })
}

/// `user.email` as it resolves at `path`. Unset exits 1 with no output, which is
/// a `None` rather than an error; every other exit is an error, so a path that has
/// gone away (128) reads as a failed scan rather than a missing identity.
pub fn user_email(path: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "user.email"])
        .output()
        .context("running git config user.email")?;

    if !out.status.success() && out.status.code() != Some(1) {
        bail!(
            "git config user.email failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!email.is_empty()).then_some(email))
}

/// Seconds east of UTC, from the trailing field of `git var GIT_COMMITTER_IDENT`.
/// Anything that does not resolve is UTC.
pub fn local_utc_offset() -> i32 {
    let Ok(out) = Command::new("git")
        .args(["var", "GIT_COMMITTER_IDENT"])
        .output()
    else {
        return 0;
    };
    parse_utc_offset(&String::from_utf8_lossy(&out.stdout)).unwrap_or(0)
}

/// Author, timestamp, subject, merge-ness and line counts for each of `shas`.
///
/// SHAs naming objects that are gone are dropped rather than aborting the call,
/// so the result can be shorter than `shas`.
pub fn numstat(path: &Path, shas: &[String]) -> Result<Vec<CommitInfo>> {
    if shas.is_empty() {
        return Ok(Vec::new()); // git would fall back to HEAD
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "log",
            "--ignore-missing", // must precede --stdin; after it, accepted and ignored
            "--no-walk=unsorted",
            "--stdin",
            "--numstat",
            // C our marker for a header line. The rest is NUL-separated (`%x00`):
            // sha, author-email, committer-timestamp, parents, subject.
            "--format=C%x00%H%x00%ae%x00%ct%x00%p%x00%s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git log --numstat")?;

    let feeder = feed_shas(take_stdin(&mut child), take_stderr(&mut child), shas);
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_string(&mut out)
        .context("reading git log --numstat")?;

    let status = child.wait()?;
    let errs = join(feeder)?;
    if !status.success() {
        bail!("git log --numstat failed: {}", errs.trim());
    }

    Ok(parse_numstat(&out))
}

/// Patch-id per SHA. Merges and empty commits have none, so the result can be
/// shorter than `shas` for that reason as well as for missing objects.
pub fn patch_ids(path: &Path, shas: &[String]) -> Result<HashMap<String, String>> {
    if shas.is_empty() {
        return Ok(HashMap::new()); // git would fall back to HEAD
    }

    // No --format: patch-id keys off the `commit <sha>` line of the default one.
    let mut log = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["log", "--ignore-missing", "--no-walk", "--stdin", "-p"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git log -p")?;

    let ids = Command::new("git")
        // --stable makes the id survive file reordering and whitespace. Its ids
        // differ from --unstable's, so changing this voids every stored id.
        .args(["patch-id", "--stable"])
        .stdin(Stdio::from(log.stdout.take().expect("stdout is piped")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git patch-id")?;

    let feeder = feed_shas(take_stdin(&mut log), take_stderr(&mut log), shas);
    let out = ids.wait_with_output().context("reading git patch-id")?;
    let log_status = log.wait()?;
    let log_errs = join(feeder)?;

    if !log_status.success() {
        bail!("git log -p failed: {}", log_errs.trim());
    }
    if !out.status.success() {
        bail!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(parse_patch_ids(&String::from_utf8_lossy(&out.stdout)))
}

/// Writes `shas` one per line into `stdin`, closes it, then drains `stderr`.
///
/// Off the caller's thread, which drains stdout meanwhile: filling one pipe and
/// draining another from a single thread deadlocks once either buffer fills.
fn feed_shas(
    mut stdin: ChildStdin,
    mut stderr: ChildStderr,
    shas: &[String],
) -> JoinHandle<String> {
    let mut payload = shas.join("\n");
    payload.push('\n');

    thread::spawn(move || {
        // A write error here means git exited first; its status says why.
        let _ = stdin.write_all(payload.as_bytes());
        drop(stdin); // git reads --stdin to EOF, so close before waiting on stderr

        let mut errs = String::new();
        let _ = stderr.read_to_string(&mut errs);
        errs
    })
}

fn take_stdin(child: &mut std::process::Child) -> ChildStdin {
    child.stdin.take().expect("stdin is piped")
}

fn take_stderr(child: &mut std::process::Child) -> ChildStderr {
    child.stderr.take().expect("stderr is piped")
}

fn join(feeder: JoinHandle<String>) -> Result<String> {
    feeder
        .join()
        .map_err(|_| anyhow::anyhow!("stdin writer panicked"))
}

/// Captured stdout. Non-UTF-8 output — a path, in practice — degrades rather
/// than failing the call.
fn capture(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `Eve <e@x> 1787766661 +0300` → 10800.
fn parse_utc_offset(ident: &str) -> Option<i32> {
    let field = ident.split_whitespace().next_back()?;
    let sign = match field.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &field[1..];
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// A `C\0`-prefixed header line opens each commit; the `added\tdeleted\tpath`
/// rows until the next header belong to it. Binary files count `-` and add zero.
fn parse_numstat(text: &str) -> Vec<CommitInfo> {
    let mut out: Vec<CommitInfo> = Vec::new();

    for line in text.lines() {
        if let Some(header) = line.strip_prefix("C\0") {
            let mut fields = header.splitn(5, '\0');
            let sha = fields.next().unwrap_or_default();
            let author_email = fields.next().unwrap_or_default();
            let committed_timestamp = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let parents = fields.next().unwrap_or_default();
            let subject = fields.next().unwrap_or_default();

            out.push(CommitInfo {
                sha: sha.to_string(),
                author_email: author_email.to_string(),
                committed_timestamp,
                is_merge: parents.split_whitespace().count() > 1,
                subject: subject.to_string(),
                added: 0,
                deleted: 0,
            });
        } else if let Some(commit) = out.last_mut() {
            let mut fields = line.split('\t');
            let (Some(added), Some(deleted)) = (fields.next(), fields.next()) else {
                continue; // the blank line git puts after the header
            };
            commit.added += added.parse().unwrap_or(0);
            commit.deleted += deleted.parse().unwrap_or(0);
        }
    }

    out
}

/// `<patch-id> <sha>` lines, keyed by SHA — the output is not in input order.
fn parse_patch_ids(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let patch_id = fields.next()?;
            let sha = fields.next()?;
            Some((sha.to_string(), patch_id.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3f2a1b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6";
    const B: &str = "9c4d2e1f0a1b2c3d4e5f60718293a4b5c6d7e8f9";

    /// One `--numstat` block, as git writes it: header, blank line, rows.
    fn block(sha: &str, parents: &str, subject: &str, rows: &[&str]) -> String {
        let mut text = format!("C\0{sha}\0eve@example.com\01787746400\0{parents}\0{subject}\n\n");
        for row in rows {
            text.push_str(row);
            text.push('\n');
        }
        text
    }

    #[test]
    fn sums_counts_across_a_commits_files() {
        let text = block(A, "", "first", &["3\t1\tsrc/a.rs", "10\t0\tsrc/b.rs"]);

        let [commit] = &parse_numstat(&text)[..] else {
            panic!("expected one commit");
        };
        assert_eq!((commit.added, commit.deleted), (13, 1));
    }

    #[test]
    fn binary_counts_contribute_nothing() {
        let text = block(A, "", "add art", &["-\t-\tlogo.png", "2\t0\tREADME.md"]);

        assert_eq!(
            (
                parse_numstat(&text)[0].added,
                parse_numstat(&text)[0].deleted
            ),
            (2, 0)
        );
    }

    #[test]
    fn header_fields_land_in_order() {
        let text = block(A, "", "the subject", &[]);

        let commit = &parse_numstat(&text)[0];
        assert_eq!(commit.sha, A);
        assert_eq!(commit.author_email, "eve@example.com");
        assert_eq!(commit.committed_timestamp, 1787746400);
        assert_eq!(commit.subject, "the subject");
    }

    #[test]
    fn one_parent_or_none_is_not_a_merge() {
        let root = block(A, "", "first", &[]);
        let child = block(B, A, "second", &[]);

        assert!(parse_numstat(&(root + &child)).iter().all(|c| !c.is_merge));
    }

    #[test]
    fn two_parents_is_a_merge() {
        let text = block(B, &format!("{A} {B}"), "Merge branch 'feature'", &[]);

        assert!(parse_numstat(&text)[0].is_merge);
    }

    #[test]
    fn rows_attach_to_the_commit_above_them() {
        let text = block(A, "", "first", &["1\t0\ta"]) + &block(B, A, "second", &["5\t5\tb"]);

        let commits = parse_numstat(&text);
        assert_eq!((commits[0].added, commits[0].deleted), (1, 0));
        assert_eq!((commits[1].added, commits[1].deleted), (5, 5));
    }

    #[test]
    fn a_merge_carries_no_rows_and_stays_zero() {
        let text = block(B, &format!("{A} {B}"), "Merge", &[]);

        assert_eq!(
            (
                parse_numstat(&text)[0].added,
                parse_numstat(&text)[0].deleted
            ),
            (0, 0)
        );
    }

    #[test]
    fn no_output_yields_no_commits() {
        assert!(parse_numstat("").is_empty());
    }

    #[test]
    fn patch_ids_key_by_sha() {
        let text = format!(
            "84b8408eb0d5cd0e1f2a3b4c5d6e7f8091a2b3c4 {A}\n\
                            1c3d5e7f9012345678901234567890abcdef0123 {B}\n"
        );

        let ids = parse_patch_ids(&text);
        assert_eq!(ids[A], "84b8408eb0d5cd0e1f2a3b4c5d6e7f8091a2b3c4");
        assert_eq!(ids[B], "1c3d5e7f9012345678901234567890abcdef0123");
    }

    #[test]
    fn patch_id_lines_that_are_not_pairs_are_skipped() {
        let text = format!("\n{A}\n84b8408eb0d5cd0e1f2a3b4c5d6e7f8091a2b3c4 {B}\n");

        assert_eq!(parse_patch_ids(&text).len(), 1);
    }

    /// Exit 128, not the 1 an unset key exits with, so it must not read as `None`.
    #[test]
    fn a_path_that_is_gone_is_an_error_not_an_absent_email() {
        assert!(user_email(Path::new("/no/such/dir")).is_err());
    }

    #[test]
    fn utc_offset_reads_the_trailing_field() {
        assert_eq!(
            parse_utc_offset("Eve <e@x> 1787766661 +0300\n"),
            Some(10800)
        );
        assert_eq!(parse_utc_offset("Eve <e@x> 1787766661 -0930"), Some(-34200));
        assert_eq!(parse_utc_offset("Eve <e@x> 1787766661 +0000"), Some(0));
    }

    #[test]
    fn unparseable_offsets_are_rejected() {
        assert_eq!(parse_utc_offset(""), None);
        assert_eq!(parse_utc_offset("Eve <e@x> 1787766661"), None);
        assert_eq!(parse_utc_offset("Eve <e@x> 1787766661 +03:00"), None);
    }
}
