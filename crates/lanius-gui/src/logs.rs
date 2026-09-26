//! Presentation-layer processing of raw captured log lines for the GUI's
//! log view.
//!
//! Raw lines come from [`crate::log_capture::LogBuffer`] in the form
//! `"YYYY-MM-DD HH:MM:SS | rest of message"` (possibly with embedded ANSI
//! color codes from libraries that colorize their own output, and possibly
//! without a recognizable timestamp at all, e.g. output that did not go
//! through the tracing capture layer). This module turns that raw text into
//! [`ProcessedLog`] rows the Slint UI can bind directly to a log list: ANSI
//! codes are stripped, the leading timestamp is parsed out into its own
//! `time` field (for a separate gutter column in the UI) and removed from
//! the message text, and only the most recent lines are kept to avoid
//! rendering unbounded lists. `controller.rs` calls [`process`] whenever the
//! log buffer changes, and [`export_text`] when the user exports logs to a
//! file.

/// Maximum number of most-recent log lines rendered in the UI's log view at
/// once. This is independent of and smaller than
/// [`crate::log_capture::MAX_LOG_LINES`] (the total retained in memory) —
/// older lines remain in the buffer for export but are not rendered live.
pub const MAX_VISIBLE_LOGS: usize = 500;

/// Removes ANSI escape sequences (e.g. `\x1b[32m`, `\x1b[1;31m`) from `text`,
/// leaving the visible characters untouched.
///
/// Some libraries emit ANSI color codes into their log messages even when
/// writing to a buffer rather than a real terminal; those codes would
/// otherwise show up as garbage in the GUI's plain-text log view.
///
/// This walks the string char-by-char (not byte-by-byte) so multi-byte
/// UTF-8 text is never split mid-character. On seeing an ESC (`\x1b`)
/// followed by `[` (a CSI sequence), it consumes parameter/intermediate
/// characters (digits and `;`) up to and including the final terminating
/// byte, which by the ANSI spec is the first character in the sequence that
/// is *not* a digit or `;`. Sequences with a non-alphabetic terminator (rare
/// but possible) have that terminator character pushed to the output rather
/// than silently dropped, so no character is ever lost — only recognized
/// color/style codes are stripped.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
                if !next.is_ascii_digit() && next != ';' {
                    out.push(next);
                    break;
                }
            }
        }
    }
    out
}

/// Extracts the `HH:MM:SS` time portion from a line that begins with the
/// `log_capture` module's `"YYYY-MM-DD HH:MM:SS | ..."` stamp format,
/// returning an empty string if `line` does not match that exact shape.
///
/// This deliberately validates the full fixed-width shape byte-by-byte
/// (digit positions, `-`, whitespace, `:` separators, and a trailing
/// whitespace-then-`|`) rather than using a general date/time parser,
/// because it must be fast (called once per visible log line on every
/// refresh) and must not misfire on log messages that happen to start with
/// digits but are not one of our own timestamps (e.g. "8000 connections
/// active"). Lines without this exact prefix — including untimestamped
/// passthrough output — are treated as having no time, and the full line is
/// shown as-is in the message column.
pub fn extract_time(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() < 19 {
        return String::new();
    }
    let digits_at = |idx: usize| bytes[idx].is_ascii_digit();
    // Validate the fixed "YYYY-MM-DD HH:MM:SS" layout position by position.
    let matches_shape = (0..4).all(digits_at)
        && bytes[4] == b'-'
        && digits_at(5)
        && digits_at(6)
        && bytes[7] == b'-'
        && digits_at(8)
        && digits_at(9)
        && (bytes[10] as char).is_whitespace()
        && digits_at(11)
        && digits_at(12)
        && bytes[13] == b':'
        && digits_at(14)
        && digits_at(15)
        && bytes[16] == b':'
        && digits_at(17)
        && digits_at(18);

    if !matches_shape {
        return String::new();
    }
    // The stamp format always separates the timestamp from the message with
    // " | "; require that separator too so we don't mistake an unrelated
    // 19-character digit run for one of our own timestamps.
    if !line[19..].trim_start().starts_with('|') {
        return String::new();
    }
    line[11..19].to_string()
}

/// A single log line ready for display: the parsed `HH:MM:SS` time (empty
/// if the line had no recognizable timestamp) and the remaining message
/// text with the timestamp/separator already stripped off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedLog {
    pub time: String,
    pub text: String,
}

/// Converts the tail of the raw log buffer into display-ready
/// [`ProcessedLog`] rows.
///
/// Only the most recent [`MAX_VISIBLE_LOGS`] lines are processed — older
/// lines are dropped from the *view* (they remain available for export via
/// [`export_text`] against the full buffer). For each visible line, ANSI
/// codes are stripped, then the leading timestamp (if present per
/// [`extract_time`]) is split off into `time` and removed, along with its
/// `|` separator and surrounding whitespace, from the displayed `text`, so
/// the timestamp is never rendered twice.
pub fn process(lines: &[String]) -> Vec<ProcessedLog> {
    let start = lines.len().saturating_sub(MAX_VISIBLE_LOGS);
    lines[start..]
        .iter()
        .map(|line| {
            let text = strip_ansi(line);
            let time = extract_time(&text);
            let text = if time.is_empty() {
                text
            } else {
                text[19..]
                    .trim_start()
                    .trim_start_matches('|')
                    .trim_start()
                    .to_string()
            };
            ProcessedLog { time, text }
        })
        .collect()
}

/// Renders the full (unfiltered, unbounded-by-`MAX_VISIBLE_LOGS`) log buffer
/// as plain text for the "export logs" feature, stripping ANSI codes but
/// leaving timestamps in place (unlike [`process`], which splits them into a
/// separate field for the UI's gutter column).
pub fn export_text(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_colour_codes_are_removed() {
        assert_eq!(strip_ansi("\u{1b}[32mINFO\u{1b}[0m ready"), "INFO ready");
        assert_eq!(strip_ansi("\u{1b}[1;31merror\u{1b}[m"), "error");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(
            strip_ansi("multi\u{1b}[33mbyte 日本語\u{1b}[0m"),
            "multibyte 日本語",
            "stripping must not corrupt multi-byte text"
        );
    }

    #[test]
    fn timestamps_are_split_off_when_present() {
        assert_eq!(
            extract_time("2026-02-10 18:11:11 | INFO | started"),
            "18:11:11"
        );
        assert_eq!(extract_time("2026-02-10 18:11:11| x"), "18:11:11");
        assert_eq!(extract_time("2026-02-10 18:11:11 INFO"), "");
        assert_eq!(extract_time("Gateway listening on port 8000"), "");
        assert_eq!(extract_time("short"), "");
    }

    #[test]
    fn process_strips_the_timestamp_prefix_from_the_message() {
        let lines = vec!["2026-02-10 18:11:11 | Gateway listening on port 8000".to_string()];
        let processed = process(&lines);
        assert_eq!(processed[0].time, "18:11:11");
        assert_eq!(
            processed[0].text, "Gateway listening on port 8000",
            "the gutter already shows the time; the message must not repeat it"
        );
    }

    #[test]
    fn process_leaves_untimestamped_lines_untouched() {
        let lines = vec!["Gateway listening on port 8000".to_string()];
        let processed = process(&lines);
        assert_eq!(processed[0].time, "");
        assert_eq!(processed[0].text, "Gateway listening on port 8000");
    }

    #[test]
    fn only_the_visible_tail_is_processed() {
        let lines: Vec<String> = (0..MAX_VISIBLE_LOGS + 25)
            .map(|i| format!("line {i}"))
            .collect();
        let processed = process(&lines);
        assert_eq!(processed.len(), MAX_VISIBLE_LOGS);
        assert_eq!(
            processed[0].text, "line 25",
            "oldest lines are dropped first"
        );
        assert_eq!(
            processed.last().unwrap().text,
            format!("line {}", MAX_VISIBLE_LOGS + 24)
        );
    }

    #[test]
    fn export_joins_cleaned_lines() {
        let lines = vec!["\u{1b}[32ma\u{1b}[0m".to_string(), "b".to_string()];
        assert_eq!(export_text(&lines), "a\nb");
        assert_eq!(export_text(&[]), "");
    }
}
