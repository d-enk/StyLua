//! Aligns trailing comments into a column, in the style of `gofmt`.
//!
//! This runs as a post-pass over the already formatted output. Alignment is a property of a group
//! of lines rather than of a single node, so it cannot be decided while formatting a node; working
//! on the rendered text also means the formatting itself is untouched - only the whitespace before
//! a trailing comment changes.
//!
//! A group is a run of consecutive lines that each have a trailing comment and the same leading
//! indentation. The run is broken by a blank line, a line without a trailing comment, or a change
//! in indentation, so unrelated code is never pulled into the same column.

use full_moon::{
    ast::{Field, LastStmt, Stmt},
    node::Node,
    tokenizer::{Token, TokenType},
    visitors::Visitor,
};

use crate::{AlignTrailingComments, Config, LineEndings, LuaVersion};

/// Aligns the trailing comments of the given formatted output.
///
/// Returns `None` if the output cannot be lexed. That does not happen for output produced by the
/// formatter itself; the fallback exists so that such a failure leaves the code as it is instead
/// of failing the format.
pub(crate) fn align_trailing_comments(code: &str, config: Config) -> Option<String> {
    if let AlignTrailingComments::Never = config.align_trailing_comments {
        return Some(code.to_string());
    }

    let mut lines = split_lines(code, config.syntax)?;
    mark_ignored_ranges(&mut lines, code, config.syntax);

    align(&mut lines);

    let line_ending = match config.line_endings {
        LineEndings::Unix => "\n",
        LineEndings::Windows => "\r\n",
    };
    Some(
        lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join(line_ending),
    )
}

/// A single line of the formatted output.
struct Line {
    /// The text of the line, including the whitespace before the trailing comment.
    text: String,
    /// Byte offset into `text` where the trailing comment starts, if this line has one.
    comment_start: Option<usize>,
    /// Whether the line must be left exactly as it is (`stylua: ignore`).
    protected: bool,
}

impl Line {
    fn new() -> Self {
        Self {
            text: String::new(),
            comment_start: None,
            protected: false,
        }
    }

    /// The width of the code preceding the trailing comment, excluding the separating whitespace.
    /// `None` if the line has no trailing comment.
    fn code_width(&self) -> Option<usize> {
        self.comment_start
            .map(|start| self.text[..start].trim_end().chars().count())
    }

    fn indent_width(&self) -> usize {
        self.text.chars().count() - self.text.trim_start().chars().count()
    }
}

/// Splits the formatted output into lines, recording where each trailing comment begins.
fn split_lines(code: &str, syntax: LuaVersion) -> Option<Vec<Line>> {
    let tokens = match full_moon::tokenizer::Lexer::new(code, syntax.into()).collect() {
        full_moon::tokenizer::LexerResult::Ok(tokens)
        | full_moon::tokenizer::LexerResult::Recovered(tokens, _) => tokens,
        full_moon::tokenizer::LexerResult::Fatal(_) => return None,
    };

    let mut lines = vec![Line::new()];
    for token in &tokens {
        push_token(token, &mut lines);
    }
    Some(lines)
}

fn push_token(token: &Token, lines: &mut Vec<Line>) {
    match token.token_type() {
        TokenType::Whitespace { characters } => push_text(characters, lines),
        // A comment is trailing only if something other than whitespace precedes it on the line.
        TokenType::SingleLineComment { .. } => push_comment(token, lines),
        TokenType::MultiLineComment { comment, .. } if !comment.contains('\n') => {
            push_comment(token, lines)
        }
        _ => push_text(&token.to_string(), lines),
    }
}

fn push_text(text: &str, lines: &mut Vec<Line>) {
    for (idx, segment) in text.split('\n').enumerate() {
        if idx > 0 {
            lines.push(Line::new());
        }
        lines.last_mut().unwrap().text.push_str(segment);
    }
}

fn push_comment(token: &Token, lines: &mut [Line]) {
    let line = lines.last_mut().unwrap();
    if !line.text.trim().is_empty() {
        line.comment_start = Some(line.text.len());
    }
    line.text.push_str(&token.to_string());
}

/// Marks the lines that must stay byte-for-byte identical: those inside a `stylua: ignore
/// start`/`stylua: ignore end` region, and those covered by a `stylua: ignore` comment.
///
/// The extent of a single `stylua: ignore` is the node following it, which only the AST knows, so
/// the output is parsed again to find it. Positions come from the same text we are aligning, so
/// they map onto the line indices directly.
fn mark_ignored_ranges(lines: &mut [Line], code: &str, syntax: LuaVersion) {
    mark_ignored_regions(lines);

    // Re-parsing is only needed to locate the node a bare `stylua: ignore` covers, so skip it for
    // the common case of a file with no ignore comment at all.
    if !code.contains("stylua: ignore") {
        return;
    }

    let Ok(ast) = full_moon::parse_fallible(code, syntax.into()).into_result() else {
        return;
    };

    let mut visitor = IgnoredNodeVisitor::default();
    visitor.visit_ast(&ast);
    for range in visitor.ranges {
        for line in lines.iter_mut().take(range.end() + 1).skip(*range.start()) {
            line.protected = true;
        }
    }
}

/// Handles `stylua: ignore start` / `stylua: ignore end`, which delimit a region of lines and are
/// therefore recoverable from the text alone.
fn mark_ignored_regions(lines: &mut [Line]) {
    let mut in_region = false;
    for line in lines.iter_mut() {
        let trimmed = line.text.trim();
        if is_ignore_marker(trimmed, "stylua: ignore start") {
            in_region = true;
            line.protected = true;
        } else if is_ignore_marker(trimmed, "stylua: ignore end") {
            in_region = false;
            line.protected = true;
        } else if in_region {
            line.protected = true;
        }
    }
}

/// Matches a single-line comment which is exactly the given marker.
fn is_ignore_marker(trimmed_line: &str, marker: &str) -> bool {
    trimmed_line
        .strip_prefix("--")
        .is_some_and(|comment| comment.trim() == marker)
}

/// Collects the line ranges of the nodes covered by a `stylua: ignore` comment.
#[derive(Default)]
struct IgnoredNodeVisitor {
    /// Zero-based, inclusive line ranges.
    ranges: Vec<std::ops::RangeInclusive<usize>>,
}

impl IgnoredNodeVisitor {
    fn record(&mut self, node: &impl Node) {
        if !has_ignore_comment(node) {
            return;
        }
        if let (Some(start), Some(end)) = (node.start_position(), node.end_position()) {
            // full_moon positions are one-based.
            self.ranges.push(start.line() - 1..=end.line() - 1);
        }
    }
}

/// Whether the node is preceded by a `stylua: ignore` comment, mirroring the check the formatter
/// makes in `Context::should_format_node`.
fn has_ignore_comment(node: &impl Node) -> bool {
    node.surrounding_trivia().0.iter().any(|trivia| {
        let comment = match trivia.token_type() {
            TokenType::SingleLineComment { comment } => comment,
            TokenType::MultiLineComment { comment, .. } => comment,
            _ => return false,
        };
        // The token holds the comment text with the leading `--` already stripped.
        comment.lines().any(|line| line.trim() == "stylua: ignore")
    })
}

impl Visitor for IgnoredNodeVisitor {
    fn visit_stmt(&mut self, node: &Stmt) {
        self.record(node);
    }

    fn visit_last_stmt(&mut self, node: &LastStmt) {
        self.record(node);
    }

    fn visit_field(&mut self, node: &Field) {
        self.record(node);
    }
}

fn align(lines: &mut [Line]) {
    let mut start = 0;
    while start < lines.len() {
        if !is_alignable(&lines[start]) {
            start += 1;
            continue;
        }

        let indent = lines[start].indent_width();
        let mut end = start + 1;
        while end < lines.len() && is_alignable(&lines[end]) && lines[end].indent_width() == indent
        {
            end += 1;
        }

        // A single line has nothing to align against.
        if end - start > 1 {
            align_group(&mut lines[start..end]);
        }

        start = end;
    }
}

fn is_alignable(line: &Line) -> bool {
    line.comment_start.is_some() && !line.protected
}

/// Pads the comments of a group so that they all start at the same column.
fn align_group(lines: &mut [Line]) {
    let column = lines.iter().filter_map(Line::code_width).max().unwrap();

    for line in lines.iter_mut() {
        let Some(comment_start) = line.comment_start else {
            continue;
        };
        let code = line.text[..comment_start].trim_end();
        let comment = &line.text[comment_start..];
        line.text = format!(
            "{code}{:width$}{comment}",
            "",
            width = column - code.chars().count() + 1
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Config {
        Config {
            align_trailing_comments: AlignTrailingComments::AdjacentLines,
            ..Config::default()
        }
    }

    fn run(input: &str, config: Config) -> String {
        align_trailing_comments(input, config).unwrap()
    }

    #[test]
    fn test_never_is_the_default_and_changes_nothing() {
        let input = "local x = 1 -- a\nlocal long = 2 -- b\n";
        assert_eq!(run(input, Config::default()), input);
    }

    #[test]
    fn test_aligns_consecutive_lines() {
        let input = "local x = 1 -- a\nlocal long = 2 -- b\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1    -- a\nlocal long = 2 -- b\n"
        );
    }

    #[test]
    fn test_blank_line_breaks_the_group() {
        let input =
            "local x = 1 -- a\nlocal long = 2 -- b\n\nlocal y = 3 -- c\nlocal zzz = 4 -- d\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1    -- a\nlocal long = 2 -- b\n\nlocal y = 3   -- c\nlocal zzz = 4 -- d\n"
        );
    }

    #[test]
    fn test_line_without_a_comment_breaks_the_group() {
        let input =
            "local x = 1 -- a\nlocal no_comment = 2\nlocal y = 3 -- c\nlocal zzz = 4 -- d\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1 -- a\nlocal no_comment = 2\nlocal y = 3   -- c\nlocal zzz = 4 -- d\n"
        );
    }

    #[test]
    fn test_indentation_change_breaks_the_group() {
        let input = "local x = 1 -- a\n\tlocal long = 2 -- b\n\tlocal y = 3 -- c\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1 -- a\n\tlocal long = 2 -- b\n\tlocal y = 3    -- c\n"
        );
    }

    #[test]
    fn test_standalone_comment_is_not_trailing() {
        let input = "local x = 1 -- a\n-- standalone\nlocal long = 2 -- b\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1 -- a\n-- standalone\nlocal long = 2 -- b\n"
        );
    }

    #[test]
    fn test_ignore_comment_keeps_hand_alignment() {
        let input = "-- stylua: ignore\nlocal t = {\n\ta = 1,   -- hand aligned\n\tbbb = 2, -- hand aligned\n}\n";
        assert_eq!(run(input, enabled()), input);
    }

    #[test]
    fn test_ignore_comment_protects_a_multiline_node() {
        // The tabs before the comments are deliberately uneven; the whole table is covered by the
        // ignore comment, so none of it may be touched.
        let input = "-- stylua: ignore\nlocal too = {\n\tx,\t\t-- string\n\ty,\t\t-- string\n}\n";
        assert_eq!(run(input, enabled()), input);
    }

    #[test]
    fn test_ignore_comment_on_a_table_field_keeps_it_untouched() {
        let input = "local bar = {\n\tx = 2,\n\t-- stylua: ignore\n\ty  =  3, -- keep\n\tzzz = 1, -- other\n}\n";
        assert_eq!(
            run(input, enabled()),
            "local bar = {\n\tx = 2,\n\t-- stylua: ignore\n\ty  =  3, -- keep\n\tzzz = 1, -- other\n}\n"
        );
    }

    #[test]
    fn test_ignore_region_is_left_alone() {
        let input = "-- stylua: ignore start\nlocal x = 1      -- a\nlocal long = 2   -- b\n-- stylua: ignore end\nlocal y = 3 -- c\nlocal zzz = 4 -- d\n";
        assert_eq!(
            run(input, enabled()),
            "-- stylua: ignore start\nlocal x = 1      -- a\nlocal long = 2   -- b\n-- stylua: ignore end\nlocal y = 3   -- c\nlocal zzz = 4 -- d\n"
        );
    }

    #[test]
    fn test_multibyte_characters_count_as_one_column() {
        let input = "local x = \"привет\" -- a\nlocal long = 2 -- b\n";
        assert_eq!(
            run(input, enabled()),
            "local x = \"привет\" -- a\nlocal long = 2     -- b\n"
        );
    }

    #[test]
    fn test_block_comment_on_the_same_line_is_aligned() {
        let input = "local x = 1 --[[ a ]]\nlocal long = 2 --[[ b ]]\n";
        assert_eq!(
            run(input, enabled()),
            "local x = 1    --[[ a ]]\nlocal long = 2 --[[ b ]]\n"
        );
    }

    #[test]
    fn test_crlf_line_endings_are_preserved() {
        let input = "local x = 1 -- a\nlocal long = 2 -- b\n";
        let config = Config {
            line_endings: LineEndings::Windows,
            ..enabled()
        };
        assert_eq!(
            run(input, config),
            "local x = 1    -- a\r\nlocal long = 2 -- b\r\n"
        );
    }
}
