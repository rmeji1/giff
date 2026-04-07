use regex::Regex;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

pub enum DiffSource {
    Uncommitted,
    ToRef(String),
    Between(String, String),
    WithArgs(String),
}

static DIFF_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^diff --git a/(.+) b/(.+)$").unwrap());
static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -(\d+),?\d* \+(\d+),?\d* @@").unwrap());
static ANSI_ESCAPE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[.*?m").unwrap());

pub type LineChange = (usize, String);
pub type FileDiff = (Vec<LineChange>, Vec<LineChange>);
pub type FileChanges = HashMap<String, FileDiff>;

// Get changes with completely custom diff args
pub fn get_changes_with_args(args: &str) -> Result<(FileChanges, String, String), Box<dyn Error>> {
    let args_vec: Vec<&str> = args.split_whitespace().collect();
    let diff_output = get_diff_output_with_args(&args_vec)?;

    // Try to extract meaningful labels from the args
    let left_label = extract_left_label(args);
    let right_label = extract_right_label(args);

    Ok((parse_diff_output(&diff_output)?, left_label, right_label))
}

// Compare uncommitted changes (git diff), including untracked files
pub fn get_uncommitted_changes() -> Result<(FileChanges, String, String), Box<dyn Error>> {
    // Use "git diff HEAD" to capture both staged and unstaged changes.
    // Plain "git diff" only shows unstaged changes, missing anything already staged.
    let diff_output = get_diff_output_with_args(&["HEAD"])?;
    let mut file_changes = parse_diff_output(&diff_output)?;
    merge_untracked_files(&mut file_changes)?;
    Ok((file_changes, "HEAD".to_string(), "Working Tree".to_string()))
}

// Compare a specific reference to working tree (git diff <ref>), including untracked files
pub fn get_changes_to_ref(
    reference: &str,
) -> Result<(FileChanges, String, String), Box<dyn Error>> {
    let diff_output = get_diff_output_with_args(&[reference])?;
    let mut file_changes = parse_diff_output(&diff_output)?;
    merge_untracked_files(&mut file_changes)?;
    Ok((
        file_changes,
        reference.to_string(),
        "Working Tree".to_string(),
    ))
}

// Compare two references (git diff <from>..<to>)
pub fn get_changes_between(
    from: &str,
    to: &str,
) -> Result<(FileChanges, String, String), Box<dyn Error>> {
    let diff_output = get_diff_output_with_args(&[&format!("{}..{}", from, to)])?;
    Ok((
        parse_diff_output(&diff_output)?,
        from.to_string(),
        to.to_string(),
    ))
}

/// Re-fetch the diff using the same source that was used at startup.
pub fn refresh_diff(source: &DiffSource) -> Result<(FileChanges, String, String), Box<dyn Error>> {
    match source {
        DiffSource::Uncommitted => get_uncommitted_changes(),
        DiffSource::ToRef(reference) => get_changes_to_ref(reference),
        DiffSource::Between(from, to) => get_changes_between(from, to),
        DiffSource::WithArgs(args) => get_changes_with_args(args),
    }
}

pub fn get_upstream_branch() -> Result<Option<String>, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD@{u}"])
        .output()?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

fn get_diff_output_with_args(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let mut cmd_args = vec!["diff", "--no-color"];
    cmd_args.extend_from_slice(args);

    let output = Command::new("git").args(&cmd_args).output()?;

    if !output.status.success() {
        return Err(format!(
            "Failed to execute git diff command: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    Ok(String::from_utf8(output.stdout)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

fn extract_left_label(args: &str) -> String {
    args.split("..")
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Base".to_string())
}

fn extract_right_label(args: &str) -> String {
    args.split("..")
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Target".to_string())
}

fn parse_diff_output(diff_output: &str) -> Result<FileChanges, Box<dyn Error>> {
    let mut file_changes = HashMap::new();
    let mut current_file = String::new();
    let mut base_lines = Vec::new();
    let mut head_lines = Vec::new();
    let mut base_line_number = 1;
    let mut head_line_number = 1;

    for line in diff_output.lines() {
        let trimmed = line.trim_end();

        // Skip the "no newline at end of file" marker — it is not
        // content and should not be rendered or counted.
        if trimmed.starts_with("\\ ") {
            continue;
        }

        let trimmed_line = ANSI_ESCAPE_RE.replace_all(trimmed, "");

        // Handle file header
        if let Some(caps) = DIFF_FILE_RE.captures(trimmed_line.as_ref()) {
            if !current_file.is_empty() {
                file_changes.insert(
                    std::mem::take(&mut current_file),
                    (
                        std::mem::take(&mut base_lines),
                        std::mem::take(&mut head_lines),
                    ),
                );
            }

            // Use second capture group as file path in most cases (the "b/" file)
            current_file = match caps.get(2) {
                Some(m) => m.as_str().to_string(),
                None => continue,
            };
            base_line_number = 1;
            head_line_number = 1;
            continue;
        }

        // Handle hunk header
        if let Some(caps) = HUNK_HEADER_RE.captures(trimmed_line.as_ref()) {
            base_line_number = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            head_line_number = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            continue;
        }

        // Skip metadata lines
        if trimmed_line.starts_with("index")
            || trimmed_line.starts_with("---")
            || trimmed_line.starts_with("+++")
            || trimmed_line.starts_with("@@")
            || trimmed_line.starts_with("new file mode")
            || trimmed_line.starts_with("new mode")
            || trimmed_line.starts_with("old mode")
            || trimmed_line.starts_with("deleted file mode")
            || trimmed_line.starts_with("rename from")
            || trimmed_line.starts_with("rename to")
            || trimmed_line.starts_with("copy from")
            || trimmed_line.starts_with("copy to")
            || trimmed_line.starts_with("similarity index")
            || trimmed_line.starts_with("dissimilarity index")
            || trimmed_line.starts_with("Binary files")
        {
            continue;
        }

        // Process diff lines
        if trimmed_line.starts_with('-') {
            base_lines.push((base_line_number, trimmed_line.to_string()));
            base_line_number += 1;
        } else if trimmed_line.starts_with('+') {
            head_lines.push((head_line_number, trimmed_line.to_string()));
            head_line_number += 1;
        } else {
            base_lines.push((base_line_number, trimmed_line.to_string()));
            head_lines.push((head_line_number, trimmed_line.to_string()));
            base_line_number += 1;
            head_line_number += 1;
        }
    }

    // Add last file changes
    if !current_file.is_empty() {
        file_changes.insert(current_file, (base_lines, head_lines));
    }

    Ok(file_changes)
}

#[derive(Clone)]
pub enum ChangeOp {
    /// Replace line at the given 1-indexed base position with new content
    Replace(usize, String),
    /// Delete line at the given 1-indexed base position
    Delete(usize),
    /// Insert content at the given 1-indexed base position.
    /// `order` is the original head line number, used to keep multiple
    /// insertions at the same base position in the correct order.
    Insert {
        base_pos: usize,
        order: usize,
        content: String,
    },
}

impl ChangeOp {
    fn line_num(&self) -> usize {
        match self {
            ChangeOp::Replace(n, _) | ChangeOp::Delete(n) => *n,
            ChangeOp::Insert { base_pos, .. } => *base_pos,
        }
    }
}

pub fn git_repo_root() -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("Not in a git repository".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn has_uncommitted_changes() -> Result<bool, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()?;
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Return the list of untracked file paths (relative to repo root).
fn get_untracked_files() -> Result<Vec<String>, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|line| line.starts_with("?? "))
        .filter_map(|line| line.get(3..))
        .map(|s| s.trim().to_string())
        .collect())
}

/// Read an untracked file and build a synthetic all-additions diff entry.
/// Returns `None` for binary / unreadable files.
fn build_untracked_entry(
    relative_path: &str,
) -> Result<Option<FileDiff>, Box<dyn Error>> {
    let repo_root = git_repo_root()?;
    let full_path = Path::new(&repo_root).join(relative_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    let head_lines: Vec<LineChange> = content
        .lines()
        .enumerate()
        .map(|(i, line)| (i + 1, format!("+{}", line)))
        .collect();
    Ok(Some((vec![], head_lines)))
}

/// Merge untracked (new) files into an existing `FileChanges` map.
fn merge_untracked_files(file_changes: &mut FileChanges) -> Result<(), Box<dyn Error>> {
    for path in get_untracked_files()? {
        if file_changes.contains_key(&path) {
            continue;
        }
        if let Some(entry) = build_untracked_entry(&path)? {
            file_changes.insert(path, entry);
        }
    }
    Ok(())
}

/// Resolve a diff file path (relative to repo root) to an absolute path.
fn resolve_diff_path(file_path: &str) -> Result<PathBuf, Box<dyn Error>> {
    let repo_root = git_repo_root()?;
    Ok(Path::new(&repo_root).join(file_path))
}

/// Apply change operations to file content lines and return the result.
/// This is the core logic extracted for testability.
pub fn apply_operations(lines: &[String], operations: &[ChangeOp]) -> Vec<String> {
    let mut lines: Vec<String> = lines.to_vec();

    // Phase 1: Apply Delete and Replace operations (already in base coordinates).
    // Process in descending line-number order so that removals at higher
    // positions don't shift indices for lower positions.
    let mut base_ops: Vec<&ChangeOp> = operations
        .iter()
        .filter(|op| matches!(op, ChangeOp::Replace(..) | ChangeOp::Delete(..)))
        .collect();
    base_ops.sort_by_key(|op| std::cmp::Reverse(op.line_num()));

    let mut deleted_positions: Vec<usize> = Vec::new();

    for op in &base_ops {
        match op {
            ChangeOp::Replace(line_num, content) => {
                if *line_num == 0 {
                    continue;
                }
                let idx = line_num - 1;
                if idx < lines.len() {
                    lines[idx] = content.clone();
                }
            }
            ChangeOp::Delete(line_num) => {
                if *line_num == 0 {
                    continue;
                }
                let idx = line_num - 1;
                if idx < lines.len() {
                    lines.remove(idx);
                    deleted_positions.push(*line_num);
                }
            }
            _ => {}
        }
    }

    // Phase 2: Apply Insert operations, adjusting positions for prior deletions.
    // Sort by (base_pos DESC, order DESC) so that multiple inserts at the
    // same base position end up in the correct source order: the last one
    // processed at a position pushes earlier ones down.
    let mut insert_ops: Vec<&ChangeOp> = operations
        .iter()
        .filter(|op| matches!(op, ChangeOp::Insert { .. }))
        .collect();
    insert_ops.sort_by(|a, b| {
        let pos_cmp = b.line_num().cmp(&a.line_num());
        if pos_cmp != std::cmp::Ordering::Equal {
            return pos_cmp;
        }
        // Tiebreak: higher order (head line number) processed first
        let a_order = if let ChangeOp::Insert { order, .. } = a {
            *order
        } else {
            0
        };
        let b_order = if let ChangeOp::Insert { order, .. } = b {
            *order
        } else {
            0
        };
        b_order.cmp(&a_order)
    });

    // Sort so we can binary-search instead of scanning the whole
    // list for every insert (avoids O(n²) on large diffs).
    deleted_positions.sort_unstable();

    for op in &insert_ops {
        if let ChangeOp::Insert {
            base_pos, content, ..
        } = op
        {
            if *base_pos == 0 {
                continue;
            }
            // Adjust for lines that were deleted at positions before this one
            let deletes_before = deleted_positions.partition_point(|&d| d < *base_pos);
            let adjusted = base_pos.saturating_sub(deletes_before);
            let idx = adjusted.saturating_sub(1).min(lines.len());
            lines.insert(idx, content.clone());
        }
    }

    lines
}

pub fn apply_changes(file_path: &str, operations: &[ChangeOp]) -> Result<(), Box<dyn Error>> {
    if operations.is_empty() {
        return Ok(());
    }

    let full_path = resolve_diff_path(file_path)?;
    let original_content = std::fs::read_to_string(&full_path)?;
    let has_trailing_newline = original_content.ends_with('\n');
    let lines: Vec<String> = original_content.lines().map(|s| s.to_string()).collect();

    let result_lines = apply_operations(&lines, operations);

    let mut result = result_lines.join("\n");
    if has_trailing_newline {
        result.push('\n');
    }
    std::fs::write(&full_path, result)?;

    Ok(())
}

pub fn check_rebase_needed() -> Result<Option<String>, Box<dyn Error>> {
    // Check if we're in a git repository
    let status = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()?;

    if !status.status.success() {
        return Ok(None);
    }

    // Get current branch name
    let branch_output = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()?;

    if !branch_output.status.success() {
        return Ok(None);
    }

    let current_branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();

    // Check if branch has an upstream and get its name
    let upstream_output = match Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            &format!("{}@{{u}}", current_branch),
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Ok(None), // No upstream configured
    };

    let upstream_name = String::from_utf8_lossy(&upstream_output.stdout)
        .trim()
        .to_string();

    // Check branch status relative to upstream
    let status_output = Command::new("git").args(["status", "-sb"]).output()?;
    let status_text = String::from_utf8_lossy(&status_output.stdout).to_string();

    // Check for diverged state (both ahead and behind)
    if status_text.contains("ahead") && status_text.contains("behind") {
        return Ok(Some(format!(
            "Your branch '{}' has diverged from '{}'.\nConsider rebasing to integrate changes cleanly.",
            current_branch, upstream_name
        )));
    }

    // Check for behind-only state
    if status_text.contains("[behind") {
        return Ok(Some(format!(
            "Your branch '{}' is behind '{}'. A rebase is recommended.",
            current_branch, upstream_name
        )));
    }

    Ok(None)
}

pub fn perform_rebase(upstream: &str) -> Result<bool, Box<dyn Error>> {
    if has_uncommitted_changes()? {
        return Err(
            "Cannot rebase: you have uncommitted changes. Please commit or stash them first."
                .into(),
        );
    }

    let output = Command::new("git").args(["rebase", upstream]).output()?;

    if !output.status.success() {
        // Abort the failed rebase to leave the repo in a clean state
        let _ = Command::new("git").args(["rebase", "--abort"]).output();
        return Ok(false);
    }

    Ok(true)
}

// ── File filtering ──────────────────────────────────────────────────

/// Convert a simple glob pattern to a regex string.
///
/// Supports `*` (any chars except `/`), `**` (any chars including `/`),
/// and `?` (single non-`/` char). All other regex metacharacters are
/// escaped.
fn glob_to_regex(pattern: &str) -> Result<Regex, Box<dyn Error>> {
    let mut re = String::from("(?:^|/)");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                re.push_str(".*");
                i += 2;
                // Skip optional trailing /
                if i < chars.len() && chars[i] == '/' {
                    re.push_str("/?");
                    i += 1;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(chars[i]);
            }
            '/' => re.push('/'),
            c => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    Ok(Regex::new(&re)?)
}

#[derive(Clone)]
pub struct FileFilter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl FileFilter {
    pub fn new(include_patterns: &[String], exclude_patterns: &[String]) -> Result<Self, Box<dyn Error>> {
        let include = include_patterns
            .iter()
            .map(|p| glob_to_regex(p))
            .collect::<Result<Vec<_>, _>>()?;
        let exclude = exclude_patterns
            .iter()
            .map(|p| glob_to_regex(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FileFilter { include, exclude })
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Returns true if the file path passes the filter.
    pub fn matches(&self, path: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|re| re.is_match(path)) {
            return false;
        }
        if self.exclude.iter().any(|re| re.is_match(path)) {
            return false;
        }
        true
    }

    /// Filter a `FileChanges` map in-place.
    pub fn apply(&self, file_changes: &mut FileChanges) {
        if self.is_empty() {
            return;
        }
        file_changes.retain(|path, _| self.matches(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_left_label / extract_right_label ────────────────────────

    #[test]
    fn left_label_from_range() {
        assert_eq!(extract_left_label("main..feature"), "main");
    }

    #[test]
    fn right_label_from_range() {
        assert_eq!(extract_right_label("main..feature"), "feature");
    }

    #[test]
    fn left_label_no_dots_returns_input() {
        // No ".." means the whole string is the left side
        assert_eq!(extract_left_label("--cached"), "--cached");
    }

    #[test]
    fn right_label_no_dots_returns_default() {
        // No ".." means there is no right side
        assert_eq!(extract_right_label("--cached"), "Target");
    }

    #[test]
    fn labels_with_empty_sides() {
        // "..feature" → left side is empty → default
        assert_eq!(extract_left_label("..feature"), "Base");
        // "main.." → right side is empty → default
        assert_eq!(extract_right_label("main.."), "Target");
    }

    #[test]
    fn labels_trim_whitespace() {
        assert_eq!(extract_left_label(" main .. feature "), "main");
        assert_eq!(extract_right_label(" main .. feature "), "feature");
    }

    // ── parse_diff_output: metadata skipping ────────────────────────────

    #[test]
    fn parse_skips_rename_metadata() {
        let diff = "\
diff --git a/old.rs b/new.rs
similarity index 95%
rename from old.rs
rename to new.rs
index abc..def 100644
--- a/old.rs
+++ b/new.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
";
        let changes = parse_diff_output(diff).unwrap();
        let (base, head) = changes.get("new.rs").expect("file should be present");
        // Only actual diff lines should be present, no metadata leaking as context
        assert!(
            !base
                .iter()
                .any(|(_, l)| l.contains("similarity") || l.contains("rename")),
            "metadata leaked into base lines: {:?}",
            base
        );
        assert!(
            !head
                .iter()
                .any(|(_, l)| l.contains("similarity") || l.contains("rename")),
            "metadata leaked into head lines: {:?}",
            head
        );
    }

    #[test]
    fn parse_skips_no_newline_marker() {
        let diff = "\
diff --git a/file.rs b/file.rs
index abc..def 100644
--- a/file.rs
+++ b/file.rs
@@ -1,2 +1,2 @@
-old line
\\ No newline at end of file
+new line
\\ No newline at end of file
";
        let changes = parse_diff_output(diff).unwrap();
        let (base, head) = changes.get("file.rs").expect("file should be present");
        assert!(
            !base.iter().any(|(_, l)| l.contains("No newline")),
            "no-newline marker leaked into base lines: {:?}",
            base
        );
        assert!(
            !head.iter().any(|(_, l)| l.contains("No newline")),
            "no-newline marker leaked into head lines: {:?}",
            head
        );
    }

    #[test]
    fn parse_skips_binary_files_line() {
        let diff = "\
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ
";
        let changes = parse_diff_output(diff).unwrap();
        // Binary file should have entry but no content lines
        if let Some((base, head)) = changes.get("image.png") {
            assert!(base.is_empty());
            assert!(head.is_empty());
        }
        // Or no entry at all — both are acceptable
    }

    // ── glob_to_regex ───────────────────────────────────────────────────

    #[test]
    fn glob_star_matches_filename() {
        let re = glob_to_regex("*.rs").unwrap();
        assert!(re.is_match("main.rs"));
        assert!(re.is_match("src/main.rs"));
        assert!(!re.is_match("main.toml"));
    }

    #[test]
    fn glob_star_does_not_cross_slash() {
        let re = glob_to_regex("src/*.rs").unwrap();
        assert!(re.is_match("src/main.rs"));
        assert!(!re.is_match("src/ui/render.rs"));
    }

    #[test]
    fn glob_double_star_crosses_slash() {
        let re = glob_to_regex("src/**/*.rs").unwrap();
        assert!(re.is_match("src/main.rs"));
        assert!(re.is_match("src/ui/render.rs"));
        assert!(!re.is_match("tests/main.rs"));
    }

    #[test]
    fn glob_question_mark_matches_single_char() {
        let re = glob_to_regex("?.rs").unwrap();
        assert!(re.is_match("a.rs"));
        assert!(!re.is_match("ab.rs"));
    }

    #[test]
    fn glob_escapes_dot() {
        let re = glob_to_regex("Cargo.lock").unwrap();
        assert!(re.is_match("Cargo.lock"));
        assert!(!re.is_match("Cargoxlock"));
    }

    // ── FileFilter ──────────────────────────────────────────────────────

    #[test]
    fn filter_empty_is_passthrough() {
        let f = FileFilter::new(&[], &[]).unwrap();
        assert!(f.is_empty());
        assert!(f.matches("anything.rs"));
    }

    #[test]
    fn filter_include_only() {
        let f = FileFilter::new(&["*.rs".into()], &[]).unwrap();
        assert!(f.matches("main.rs"));
        assert!(f.matches("src/lib.rs"));
        assert!(!f.matches("Cargo.toml"));
    }

    #[test]
    fn filter_exclude_only() {
        let f = FileFilter::new(&[], &["*.lock".into()]).unwrap();
        assert!(f.matches("main.rs"));
        assert!(!f.matches("Cargo.lock"));
    }

    #[test]
    fn filter_include_and_exclude() {
        let f = FileFilter::new(&["src/**/*.rs".into()], &["*test*".into()]).unwrap();
        assert!(f.matches("src/main.rs"));
        assert!(f.matches("src/ui/render.rs"));
        assert!(!f.matches("src/ui/tests.rs"));
        assert!(!f.matches("Cargo.toml"));
    }

    #[test]
    fn filter_apply_removes_non_matching() {
        let f = FileFilter::new(&["*.rs".into()], &[]).unwrap();
        let mut fc: FileChanges = HashMap::new();
        fc.insert("main.rs".into(), (vec![], vec![]));
        fc.insert("Cargo.toml".into(), (vec![], vec![]));
        fc.insert("src/lib.rs".into(), (vec![], vec![]));
        f.apply(&mut fc);
        assert!(fc.contains_key("main.rs"));
        assert!(fc.contains_key("src/lib.rs"));
        assert!(!fc.contains_key("Cargo.toml"));
    }
}
