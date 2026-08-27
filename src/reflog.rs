//! `logs/HEAD` → the SHAs it names. Pure; the caller reads the reflogs.

use std::collections::HashSet;

/// The "new" SHA (field 2) of every reflog line, pooled, deduped, in first-seen
/// order. Each `reflogs` entry is the whole text of one worktree's `logs/HEAD`.
///
/// Lines are `<old> <new> <ident> <ts> <offset>`, usually plus a tab and a
/// message — but not the seed line `git worktree add` writes, so field 2 is
/// reached by splitting on whitespace rather than on the tab.
pub fn extract_shas(reflogs: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();

    for text in reflogs {
        for line in text.lines() {
            let Some(sha) = line.split_whitespace().nth(1) else {
                continue; // not a reflog line
            };
            // Zeros belong to the *old* field of `commit (initial)`, never here.
            if is_all_zeros(sha) {
                continue;
            }
            if seen.insert(sha) {
                out.push(sha.to_string());
            }
        }
    }

    out
}

/// 40 zeros under SHA-1, 64 under SHA-256.
fn is_all_zeros(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|b| b == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3f2a1b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6";
    const B: &str = "9c4d2e1f0a1b2c3d4e5f60718293a4b5c6d7e8f9";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    /// One reflog line, as git writes it.
    fn line(old: &str, new: &str, message: &str) -> String {
        format!("{old} {new} Eve <eve@example.com> 1787746400 +0300\t{message}\n")
    }

    #[test]
    fn takes_field_two_of_every_line() {
        let log = line(ZERO, A, "commit (initial): first") + &line(A, B, "commit: second");

        assert_eq!(extract_shas(&[log]), vec![A, B]);
    }

    #[test]
    fn message_less_line_parses() {
        let log = format!("{ZERO} {A} Eve <eve@example.com> 1787746422 +0300\n");

        assert_eq!(extract_shas(&[log]), vec![A]);
    }

    #[test]
    fn dedupes_across_reflogs_keeping_first_seen_order() {
        let main = line(ZERO, A, "commit (initial): first") + &line(A, B, "commit: second");
        let linked = line(B, B, "checkout: moving from main to feature")
            + &line(B, A, "reset: moving to HEAD~1");

        assert_eq!(extract_shas(&[main, linked]), vec![A, B]);
    }

    #[test]
    fn all_zeros_is_never_extracted() {
        let log = line(A, ZERO, "guard against a line git does not write");

        assert!(extract_shas(&[log]).is_empty());
    }

    #[test]
    fn all_zeros_is_length_agnostic() {
        let sha256_zero = "0".repeat(64);
        let log = format!("{A} {sha256_zero} Eve <eve@example.com> 1787746400 +0300\tx\n");

        assert!(extract_shas(&[log]).is_empty());
    }

    #[test]
    fn lines_without_a_second_field_are_skipped() {
        let log = format!("\n   \n{A}\n") + &line(A, B, "commit: second");

        assert_eq!(extract_shas(&[log]), vec![B]);
    }

    #[test]
    fn no_reflogs_yields_nothing() {
        assert!(extract_shas(&[]).is_empty());
    }
}
