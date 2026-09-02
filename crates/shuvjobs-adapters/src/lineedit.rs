//! A line editor for the files cron and anacron keep.
//!
//! Every write into an existing config file is "change exactly this one
//! line and leave the rest of the bytes alone". Reformatting somebody's
//! crontab because we round-tripped it through a parser is not
//! acceptable, so [`LineFile`] holds the lines verbatim, remembers the
//! newline style, and every mutation names the line it expects to find.
//! If the file moved under us the edit fails with [`EditError::Mismatch`]
//! rather than clobbering whatever is there now.
//!
//! One deliberate normalisation: [`LineFile::render`] always ends with a
//! newline. Vixie cron rejects a crontab whose last line is unterminated
//! ("premature EOF"), so a file we hand back is always terminated.

use crate::write::DISABLED_MARKER;

/// The line terminator a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newline {
    Lf,
    CrLf,
}

impl Newline {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// Why an edit was refused. Never partial: the file is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The 1-based line number is past the end of the file.
    OutOfRange { line: usize, len: usize },
    /// The line is not what the caller last read.
    Mismatch {
        line: usize,
        expected: String,
        actual: String,
    },
    /// Asked to re-enable a line that carries no disabled marker.
    NotDisabled { line: usize },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange { line, len } => {
                write!(f, "line {line} is past the end of the file ({len} lines)")
            }
            Self::Mismatch {
                line,
                expected,
                actual,
            } => write!(
                f,
                "line {line} changed since it was read; refresh and retry \
                 (expected `{expected}`, found `{actual}`)"
            ),
            Self::NotDisabled { line } => {
                write!(f, "line {line} is not disabled by shuvjobs")
            }
        }
    }
}

impl std::error::Error for EditError {}

impl From<EditError> for shuvjobs_core::Error {
    fn from(err: EditError) -> Self {
        shuvjobs_core::Error::Conflict(err.to_string())
    }
}

/// A text file as a sequence of lines plus its newline style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFile {
    lines: Vec<String>,
    newline: Newline,
    had_trailing_newline: bool,
}

impl LineFile {
    pub fn parse(text: &str) -> Self {
        // First terminator wins; mixed-ending files are rare and get
        // normalised to whichever style the file mostly is.
        let newline = if text.contains("\r\n") {
            Newline::CrLf
        } else {
            Newline::Lf
        };
        if text.is_empty() {
            return Self {
                lines: Vec::new(),
                newline,
                had_trailing_newline: true,
            };
        }
        let had_trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines = body
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect();
        Self {
            lines,
            newline,
            had_trailing_newline,
        }
    }

    /// The file's bytes, always newline-terminated (an empty file stays
    /// empty).
    pub fn render(&self) -> String {
        let nl = self.newline.as_str();
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push_str(nl);
        }
        out
    }

    pub fn newline(&self) -> Newline {
        self.newline
    }

    /// Whether the file as read ended with a newline. `render` adds one
    /// either way; this is here so a caller can say so in a plan.
    pub fn had_trailing_newline(&self) -> bool {
        self.had_trailing_newline
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The 1-based line `n`.
    pub fn line(&self, n: usize) -> Option<&str> {
        if n == 0 {
            return None;
        }
        self.lines.get(n - 1).map(String::as_str)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    fn check(&self, n: usize, expected: &str) -> Result<usize, EditError> {
        let actual = self.line(n).ok_or(EditError::OutOfRange {
            line: n,
            len: self.lines.len(),
        })?;
        if actual != expected {
            return Err(EditError::Mismatch {
                line: n,
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(n - 1)
    }

    pub fn replace(&mut self, n: usize, expected: &str, new: &str) -> Result<(), EditError> {
        let idx = self.check(n, expected)?;
        self.lines[idx] = new.to_string();
        Ok(())
    }

    pub fn delete(&mut self, n: usize, expected: &str) -> Result<(), EditError> {
        let idx = self.check(n, expected)?;
        self.lines.remove(idx);
        Ok(())
    }

    /// Append `line` and return its new 1-based number.
    pub fn append(&mut self, line: &str) -> usize {
        self.lines.push(line.to_string());
        self.lines.len()
    }

    /// Prefix `marker` to line `n`. The rest of the line, leading
    /// whitespace included, is kept byte for byte so [`Self::uncomment`]
    /// can restore it exactly.
    pub fn comment_out(&mut self, n: usize, expected: &str, marker: &str) -> Result<(), EditError> {
        let idx = self.check(n, expected)?;
        self.lines[idx] = format!("{marker}{expected}");
        Ok(())
    }

    /// Strip `marker` from line `n` and return the restored line.
    pub fn uncomment(&mut self, n: usize, marker: &str) -> Result<String, EditError> {
        let current = self.line(n).ok_or(EditError::OutOfRange {
            line: n,
            len: self.lines.len(),
        })?;
        let restored = current
            .strip_prefix(marker)
            .ok_or(EditError::NotDisabled { line: n })?
            .to_string();
        self.lines[n - 1] = restored.clone();
        Ok(restored)
    }

    /// The single line matching `pred`, as a 1-based number.
    ///
    /// `Ok(None)` when nothing matched; `Err(lines)` when more than one
    /// did, so the caller can report the ambiguity instead of guessing.
    pub fn find_unique<F>(&self, pred: F) -> Result<Option<usize>, Vec<usize>>
    where
        F: Fn(&str) -> bool,
    {
        let hits: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| pred(line))
            .map(|(idx, _)| idx + 1)
            .collect();
        match hits.len() {
            0 => Ok(None),
            1 => Ok(Some(hits[0])),
            _ => Err(hits),
        }
    }
}

/// The original line behind a shuvjobs-disabled comment, if this is one.
///
/// Tolerates a missing space after the marker so a hand-edited file
/// still reads back as ours.
pub fn strip_disabled_marker(line: &str) -> Option<&str> {
    let marker = DISABLED_MARKER.trim_end();
    let rest = line.strip_prefix(marker)?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LF: &str = "# header\n0 9 * * * echo hi\n\n30 2 * * 1 backup\n";
    const CRLF: &str = "# header\r\n0 9 * * * echo hi\r\n";

    #[test]
    fn lf_file_round_trips_byte_for_byte() {
        let file = LineFile::parse(LF);
        assert_eq!(file.newline(), Newline::Lf);
        assert_eq!(file.len(), 4);
        assert_eq!(file.line(2), Some("0 9 * * * echo hi"));
        assert_eq!(file.line(3), Some(""));
        assert_eq!(file.line(0), None);
        assert_eq!(file.line(5), None);
        assert_eq!(file.render(), LF);
    }

    #[test]
    fn crlf_file_round_trips_byte_for_byte() {
        let file = LineFile::parse(CRLF);
        assert_eq!(file.newline(), Newline::CrLf);
        assert_eq!(file.len(), 2);
        assert_eq!(file.line(1), Some("# header"));
        assert_eq!(file.render(), CRLF);
    }

    #[test]
    fn a_missing_trailing_newline_is_added_back() {
        let file = LineFile::parse("0 9 * * * echo hi");
        assert!(!file.had_trailing_newline());
        assert_eq!(file.len(), 1);
        assert_eq!(file.render(), "0 9 * * * echo hi\n");
    }

    #[test]
    fn empty_file_round_trips() {
        let file = LineFile::parse("");
        assert!(file.is_empty());
        assert_eq!(file.len(), 0);
        assert_eq!(file.render(), "");
    }

    #[test]
    fn single_newline_is_one_empty_line() {
        let file = LineFile::parse("\n");
        assert_eq!(file.len(), 1);
        assert_eq!(file.render(), "\n");
    }

    #[test]
    fn replace_and_delete_need_a_matching_line() {
        let mut file = LineFile::parse(LF);
        file.replace(2, "0 9 * * * echo hi", "0 10 * * * echo hi")
            .unwrap();
        assert_eq!(file.line(2), Some("0 10 * * * echo hi"));
        file.delete(4, "30 2 * * 1 backup").unwrap();
        assert_eq!(file.render(), "# header\n0 10 * * * echo hi\n\n");
    }

    #[test]
    fn a_stale_expected_line_is_a_mismatch() {
        let mut file = LineFile::parse(LF);
        let err = file
            .replace(2, "0 8 * * * echo hi", "x")
            .expect_err("must refuse");
        assert_eq!(
            err,
            EditError::Mismatch {
                line: 2,
                expected: "0 8 * * * echo hi".into(),
                actual: "0 9 * * * echo hi".into(),
            }
        );
        // The file is untouched.
        assert_eq!(file.render(), LF);
        let conflict: shuvjobs_core::Error = err.into();
        assert!(
            matches!(conflict, shuvjobs_core::Error::Conflict(_)),
            "got {conflict:?}"
        );
    }

    #[test]
    fn editing_past_the_end_is_out_of_range() {
        let mut file = LineFile::parse(LF);
        assert_eq!(
            file.delete(9, "whatever").expect_err("must refuse"),
            EditError::OutOfRange { line: 9, len: 4 }
        );
        assert_eq!(
            file.replace(0, "", "x").expect_err("must refuse"),
            EditError::OutOfRange { line: 0, len: 4 }
        );
    }

    #[test]
    fn append_terminates_a_file_that_had_no_trailing_newline() {
        let mut file = LineFile::parse("0 9 * * * echo hi");
        let n = file.append("30 2 * * 1 backup");
        assert_eq!(n, 2);
        assert_eq!(file.render(), "0 9 * * * echo hi\n30 2 * * 1 backup\n");
    }

    #[test]
    fn comment_out_then_uncomment_restores_the_exact_bytes() {
        let original = "  0 9 * * *\techo hi  ";
        let mut file = LineFile::parse(&format!("# header\n{original}\n"));
        file.comment_out(2, original, DISABLED_MARKER).unwrap();
        assert_eq!(
            file.line(2),
            Some("#shuvjobs-disabled#   0 9 * * *\techo hi  ")
        );
        assert_eq!(strip_disabled_marker(file.line(2).unwrap()), Some(original));

        let restored = file.uncomment(2, DISABLED_MARKER).unwrap();
        assert_eq!(restored, original);
        assert_eq!(file.render(), format!("# header\n{original}\n"));
    }

    #[test]
    fn uncommenting_an_unmarked_line_is_not_disabled() {
        let mut file = LineFile::parse(LF);
        assert_eq!(
            file.uncomment(2, DISABLED_MARKER).expect_err("must refuse"),
            EditError::NotDisabled { line: 2 }
        );
    }

    #[test]
    fn strip_disabled_marker_tolerates_a_missing_space() {
        assert_eq!(strip_disabled_marker("#shuvjobs-disabled# x"), Some("x"));
        assert_eq!(strip_disabled_marker("#shuvjobs-disabled#x"), Some("x"));
        assert_eq!(strip_disabled_marker("# a normal comment"), None);
        assert_eq!(strip_disabled_marker("0 9 * * * echo hi"), None);
    }

    #[test]
    fn find_unique_distinguishes_none_one_and_many() {
        let file = LineFile::parse("a\nb\nb\n");
        assert_eq!(file.find_unique(|l| l == "z"), Ok(None));
        assert_eq!(file.find_unique(|l| l == "a"), Ok(Some(1)));
        assert_eq!(file.find_unique(|l| l == "b"), Err(vec![2, 3]));
    }
}
