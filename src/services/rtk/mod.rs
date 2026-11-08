// RTK — Real Token Killer: compress tool_result content in chat requests
// Injected before caveman, after normalize_tool_messages.
// Port of 9router open-sse/rtk/index.js + autodetect.js

pub mod filters;

use super::rtk_filters::*;

use crate::types::chat::Message;

// ── Auto-detect: which filter to use based on content pattern ──

fn auto_detect_filter(text: &str) -> Option<fn(&str) -> String> {
    let head = if text.len() > DETECT_WINDOW { text.chars().take(DETECT_WINDOW).collect::<String>() } else { text.to_string() };

    // git diff: starts with "diff --git" or contains "@@"
    if head.contains("diff --git ") || head.contains("\n@@ ") || head.starts_with("@@ ") {
        return Some(git_diff);
    }

    // git status
    if head.contains("On branch ") || head.contains("nothing to commit") || head.contains("Untracked files:")
        || head.contains("Changes ") {
        return Some(git_status);
    }

    // Build output BEFORE porcelain: prevent "Compiling" misdetection as git-status
    if head.contains("Compiling") || head.contains("Downloading")
        || head.contains("npm ERR!") || head.contains("npm error") || head.contains("yarn error")
        || head.contains("npm warn") || head.contains("yarn warn")
        || head.contains("BUILD SUCCESS") || head.contains("BUILD FAILED")
        || head.contains("[ERROR]") || head.contains("Successfully installed") || head.contains("Successfully built")
        || head.contains("Finished") || head.contains("added ") {
        return Some(build_output);
    }

    // Porcelain git status (e.g. "M  path", "?? path")
    if is_mostly_porcelain(&head) { return Some(git_status); }

    let lines: Vec<&str> = head.lines().collect();
    let non_empty: Vec<&str> = lines.iter().filter(|l| !l.trim().is_empty()).copied().collect();

    // grep: file:lineno:content pattern
    if non_empty.len() >= 5 && non_empty.iter().take(5).any(|l| is_grep_line(l)) {
        return Some(grep);
    }

    // find: path-like lines with no ':'
    if non_empty.len() >= 3 && non_empty.iter().all(|l| is_path_like(l)) {
        return Some(find);
    }

    // tree: box-drawing glyphs
    if head.contains("├──") || head.contains("└──") || head.contains("│  ") {
        return Some(tree);
    }

    // ls: "total N" header or perms rows
    if head.contains("total ") || count_ls_rows(&head) >= 3 {
        return Some(ls);
    }

    // search-list header
    if head.starts_with("Result of search in '") {
        return Some(search_list);
    }

    // Line-numbered file dump: "  N|content"
    if lines.len() >= SMART_TRUNCATE_MIN_LINES && is_line_numbered(&lines) {
        return Some(read_numbered);
    }

    // dedupLog: fallback for generic noise with ≥5 lines
    if non_empty.len() >= 5 { return Some(dedup_log); }

    // smartTruncate: last resort for very big blobs
    if text.lines().count() >= SMART_TRUNCATE_MIN_LINES { return Some(smart_truncate); }

    None
}

fn is_grep_line(line: &str) -> bool {
    let first = match line.find(':') { Some(i) => i, None => return false };
    let rest = &line[first + 1..];
    let second = match rest.find(':') { Some(i) => i, None => return false };
    rest[..second].chars().all(|c| c.is_ascii_digit())
}

fn is_path_like(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.contains(':') { return false; }
    t.starts_with('.') || t.starts_with('/') || t.contains('/')
}

fn is_mostly_porcelain(head: &str) -> bool {
    let lines: Vec<&str> = head.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 3 { return false; }
    let re = |s: &&str| {
        s.len() >= 3
            && s.chars().nth(0).map_or(false, |c| " MADRCU?!".contains(c))
            && s.chars().nth(1).map_or(false, |c| " MADRCU?!".contains(c))
            && s.chars().nth(2) == Some(' ')
    };
    let hits = lines.iter().filter(|l| re(l)).count();
    (hits as f64 / lines.len() as f64) >= 0.6
}

fn count_ls_rows(head: &str) -> usize {
    head.lines().filter(|l| {
        l.len() >= 10 && "-dlbcps".contains(l.chars().next().unwrap_or(' '))
            && l.chars().skip(1).take(3).all(|c| "rwx-".contains(c))
    }).count()
}

fn is_line_numbered(lines: &[&str]) -> bool {
    let mut hits = 0;
    let mut non_empty = 0;
    let sample = lines.iter().take(100);
    for l in sample {
        if l.trim().is_empty() { continue; }
        non_empty += 1;
        // "  N|content" or "   N|content"
        if l.len() > 3 && l.chars().next().map_or(false, |c| c.is_ascii_digit())
            || l.trim_start().chars().next().map_or(false, |c| c.is_ascii_digit())
                && l.contains('|') {
            hits += 1;
        }
    }
    if non_empty < 5 { return false; }
    (hits as f64 / non_empty as f64) >= READ_NUMBERED_MIN_HIT_RATIO
}

// ── Safe apply: catch panics, return original on error ──

fn safe_apply(filter_fn: fn(&str) -> String, text: &str) -> String {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter_fn(text)));
    match result {
        Ok(out) => {
            // Safety: never return empty, never grow the input
            if out.is_empty() || out.len() >= text.len() { text.to_string() } else { out }
        }
        Err(_) => {
            tracing::warn!(target: "rtk", "filter panicked — passing through raw output");
            text.to_string()
        }
    }
}

fn compress_text(text: &str, stats: &mut RtkStats) -> String {
    let bytes_in = text.len();
    stats.bytes_before += bytes_in;

    if bytes_in < MIN_COMPRESS_SIZE || bytes_in > RAW_CAP {
        stats.bytes_after += bytes_in;
        return text.to_string();
    }

    let filter_fn = match auto_detect_filter(text) {
        Some(f) => f,
        None => {
            stats.bytes_after += bytes_in;
            return text.to_string();
        }
    };

    let out = safe_apply(filter_fn, text);

    // Safety: never return empty, never grow the input
    if out.is_empty() || out.len() >= bytes_in {
        stats.bytes_after += bytes_in;
        return text.to_string();
    }

    stats.bytes_after += out.len();
    stats.hits += 1;
    out
}

// ── Stats ──

#[derive(Debug, Clone, Default)]
pub struct RtkStats {
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub hits: usize,
}

impl RtkStats {
    pub fn log_line(&self) -> Option<String> {
        if self.hits == 0 { return None; }
        let saved = self.bytes_before.saturating_sub(self.bytes_after);
        let pct = if self.bytes_before > 0 {
            (saved as f64 / self.bytes_before as f64) * 100.0
        } else { 0.0 };
        Some(format!(
            "[RTK] saved {}B / {}B ({:.1}%) hits={}",
            saved, self.bytes_before, pct, self.hits
        ))
    }
}

// ── Main entry: compress tool_result content in-place ──

pub fn compress_tool_messages(messages: &mut Vec<Message>) -> RtkStats {
    let mut stats = RtkStats::default();

    for msg in messages.iter_mut() {
        if msg.role != "tool" { continue; }
        if let Some(ref content) = msg.content {
            let compressed = compress_text(content, &mut stats);
            msg.content = Some(compressed);
        }
    }

    stats
}

// ── One-liner entry for chat pipeline ──

pub async fn compress(db: &sqlx::PgPool, messages: &mut Vec<Message>) {
    let enabled: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'rtk_enabled'")
        .fetch_one(db)
        .await
        .unwrap_or_else(|_| "true".to_string());

    if enabled != "true" { return; }

    let stats = compress_tool_messages(messages);
    if let Some(log_line) = stats.log_line() {
        tracing::info!(target: "rtk", "{}", log_line);
    }
}
