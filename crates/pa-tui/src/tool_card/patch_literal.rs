//! `apply_patch` literals in `python_repl` cells render as the patch they
//! carry: the V4A text inside the string literal passed to the kernel's
//! `apply_patch` skill gets diff colouring (envelope, file and hunk headers,
//! added/removed/context lines) over the cell's ordinary highlighting.
//!
//! Detection parses the cell with tree-sitter's Python grammar and accepts
//! only these call shapes (with or without `await`):
//! `apply_patch(<lit>)`, `apply_patch(patch=<lit>)`, `apply_patch.run(<lit>)`,
//! `apply_patch.run(patch=<lit>)`, where `<lit>` is a string literal, an
//! implicit concatenation of literals, either one parenthesized, or a name
//! bound exactly once earlier in the cell by `NAME = <lit>`. f-strings, byte
//! strings, and anything computed fall back to the plain string colour.
//!
//! Scope colouring stays with [`super::highlight`]; this module only
//! restyles the bytes between a detected literal's quotes, line by line,
//! classified by each source line's first characters (escapes are not
//! decoded: what is displayed is what is classified).

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use ratatui::style::{Modifier, Style};
use tree_sitter::{Node, Parser, Tree};

use super::highlight;
use crate::theme::{ColorMode, Theme, ThemeBg, ThemeColor};
use crate::{Line, Span};

/// The kernel binding the literal colouring follows (`skills/apply-patch`).
const APPLY_PATCH: &str = "apply_patch";

/// Distinct cell sources whose detected literals stay cached: a few cards
/// may stream or run at once, and each repaints its unchanged code every
/// frame.
const CACHED_CELLS: usize = 4;

/// One cached cell: its source and the detected literal bodies.
type CachedBodies = (String, Rc<[Range<usize>]>);

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
    static CACHE: RefCell<Vec<CachedBodies>> = const { RefCell::new(Vec::new()) };
}

/// Restyle the `apply_patch` literal bodies inside `lines`, the
/// highlighted rows of `code` (one row per `\n`-separated source line).
/// Cells without an `apply_patch` call are left untouched.
pub(crate) fn overlay_patch_literals(code: &str, lines: &mut [Line], theme: &Theme) {
    if !code.contains(APPLY_PATCH) {
        return;
    }
    let bodies = CACHE.with_borrow_mut(|cache| {
        if let Some(index) = cache.iter().position(|(source, _)| source == code) {
            let entry = cache.remove(index);
            let bodies = Rc::clone(&entry.1);
            cache.insert(0, entry);
            return bodies;
        }
        let bodies: Rc<[Range<usize>]> = patch_literal_bodies(code).into();
        cache.insert(0, (code.to_string(), Rc::clone(&bodies)));
        cache.truncate(CACHED_CELLS);
        bodies
    });
    if bodies.is_empty() {
        return;
    }
    let palette = PatchPalette::from_theme(theme);
    let mut line_index = 0usize;
    let mut line_start = 0usize;
    let mut scanned = 0usize;
    for body in bodies.iter() {
        // Advance the line bookkeeping to the body's first line (bodies are
        // sorted and disjoint, so the scan is linear in the cell).
        for (offset, byte) in code.as_bytes()[scanned..body.start].iter().enumerate() {
            if *byte == b'\n' {
                line_index += 1;
                line_start = scanned + offset + 1;
            }
        }
        let mut cursor = body.start;
        loop {
            let piece_end = code[cursor..body.end]
                .find('\n')
                .map_or(body.end, |n| cursor + n);
            let segments = line_segments(&code[cursor..piece_end], cursor - line_start, &palette);
            if let Some(line) = lines.get_mut(line_index) {
                restyle(line, &segments);
            }
            if piece_end == body.end {
                break;
            }
            cursor = piece_end + 1;
            line_index += 1;
            line_start = cursor;
        }
        scanned = body.end;
    }
}

/// The byte ranges between the quotes of every literal passed to
/// `apply_patch` in `code`: sorted, disjoint, clipped to `code`.
pub(crate) fn patch_literal_bodies(code: &str) -> Vec<Range<usize>> {
    // A cell still streaming in usually ends inside the patch literal.
    // tree-sitter's recovery leaves an unclosed triple-quoted string as a
    // bare `string_start` in an ERROR node with no call around it, and
    // recovering through a long patch body parsed as code is slow. So a
    // cell whose tail is an unclosed triple-quoted string (as the
    // highlighter scans it) parses with that tail closed instead: the
    // string's delimiter and the brackets open around it appended. Body
    // ranges past the real end are clipped below.
    let closed;
    let source = match highlight::unclosed_triple_quoted_tail(code) {
        Some(closing) => {
            let mut text = String::with_capacity(code.len() + 1 + closing.len());
            text.push_str(code);
            // A trailing backslash would escape the first closing quote.
            let backslashes = code.bytes().rev().take_while(|b| *b == b'\\').count();
            if backslashes % 2 == 1 {
                text.push(' ');
            }
            text.push_str(&closing);
            closed = text;
            closed.as_str()
        }
        None => code,
    };
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let mut bodies: Vec<Range<usize>> = Detector::new(source, &tree)
        .literal_strings()
        .into_iter()
        .filter_map(|string| {
            // The bytes between the quotes; an unclosed string's body runs
            // to the node's end. Clipped to the real cell.
            let open = string.child(0)?;
            let last = string.child_count().checked_sub(1)?;
            let close = string
                .child(last)
                .filter(|close| close.kind() == "string_end" && !close.is_missing())
                .map_or(string.end_byte(), |close| close.start_byte());
            let end = close.min(code.len());
            (open.end_byte() < end).then_some(open.end_byte()..end)
        })
        .collect();
    bodies.sort_by_key(|body| body.start);
    bodies.dedup();
    bodies
}

fn parse(source: &str) -> Option<Tree> {
    PARSER.with_borrow_mut(|parser| {
        if parser.is_none() {
            let mut fresh = Parser::new();
            fresh
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .ok()?;
            *parser = Some(fresh);
        }
        parser.as_mut()?.parse(source, None)
    })
}

/// One pass over the tree collecting the matching calls and the cell's
/// name bindings, then the literal strings each call passes.
struct Detector<'tree> {
    source: &'tree str,
    calls: Vec<Node<'tree>>,
    /// Every binding of a plain name: the name, where it happens, and the
    /// bound value when the binding is a plain `NAME = <value>`.
    bindings: Vec<(&'tree str, usize, Option<Node<'tree>>)>,
}

impl<'tree> Detector<'tree> {
    fn new(source: &'tree str, tree: &'tree Tree) -> Self {
        let mut detector = Detector {
            source,
            calls: Vec::new(),
            bindings: Vec::new(),
        };
        let mut cursor = tree.walk();
        'walk: loop {
            detector.visit(cursor.node());
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
        detector
    }

    fn text(&self, node: Node<'tree>) -> &'tree str {
        &self.source[node.byte_range()]
    }

    fn visit(&mut self, node: Node<'tree>) {
        match node.kind() {
            "call" => {
                if node
                    .child_by_field_name("function")
                    .is_some_and(|function| self.is_apply_patch(function))
                {
                    self.calls.push(node);
                }
            }
            "assignment" | "augmented_assignment" => {
                if let Some(left) = node.child_by_field_name("left") {
                    // Only `NAME = <value>` carries a usable value; `+=` and
                    // the rest just count as another binding.
                    let value = (node.kind() == "assignment")
                        .then(|| node.child_by_field_name("right"))
                        .flatten();
                    self.bind_target(left, value);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind_target(name, None);
                }
            }
            "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.bind_target(left, None);
                }
            }
            _ => {}
        }
    }

    /// `apply_patch` or `apply_patch.run`.
    fn is_apply_patch(&self, function: Node<'tree>) -> bool {
        match function.kind() {
            "identifier" => self.text(function) == APPLY_PATCH,
            "attribute" => {
                function
                    .child_by_field_name("object")
                    .is_some_and(|object| {
                        object.kind() == "identifier" && self.text(object) == APPLY_PATCH
                    })
                    && function
                        .child_by_field_name("attribute")
                        .is_some_and(|attribute| self.text(attribute) == "run")
            }
            _ => false,
        }
    }

    fn bind_target(&mut self, target: Node<'tree>, value: Option<Node<'tree>>) {
        if target.kind() == "identifier" {
            self.bindings
                .push((self.text(target), target.start_byte(), value));
            return;
        }
        // Unpacking binds each name to something unknown.
        let mut cursor = target.walk();
        for child in target.named_children(&mut cursor) {
            self.bind_target(child, None);
        }
    }

    /// The literal `string` nodes passed to the matching calls.
    fn literal_strings(&self) -> Vec<Node<'tree>> {
        let mut strings = Vec::new();
        for call in &self.calls {
            let Some(arguments) = call.child_by_field_name("arguments") else {
                continue;
            };
            // The patch argument: the first positional argument, or the
            // `patch=` keyword (the binding's only parameter).
            let mut cursor = arguments.walk();
            let mut patch = None;
            for (index, argument) in arguments
                .named_children(&mut cursor)
                .filter(|argument| argument.kind() != "comment")
                .enumerate()
            {
                match argument.kind() {
                    "keyword_argument" => {
                        if argument
                            .child_by_field_name("name")
                            .is_some_and(|name| self.text(name) == "patch")
                        {
                            patch = argument.child_by_field_name("value");
                            break;
                        }
                    }
                    "list_splat" | "dictionary_splat" => break,
                    _ => {
                        if index == 0 {
                            patch = Some(argument);
                        }
                        break;
                    }
                }
            }
            let Some(argument) = patch else {
                continue;
            };
            let literal = if argument.kind() == "identifier" {
                self.bound_literal(self.text(argument), call.start_byte())
            } else {
                Some(argument)
            };
            if let Some(found) = literal.and_then(|node| self.literal_group(node)) {
                strings.extend(found);
            }
        }
        strings
    }

    /// The value of `name`'s one binding in the cell, when it is an
    /// assignment before `before`.
    fn bound_literal(&self, name: &str, before: usize) -> Option<Node<'tree>> {
        let mut bindings = self.bindings.iter().filter(|(bound, ..)| *bound == name);
        let (_, at, value) = bindings.next()?;
        if bindings.next().is_some() || *at >= before {
            return None;
        }
        let mut value = (*value)?;
        // `A = B = <lit>`: the innermost right-hand side.
        while value.kind() == "assignment" {
            value = value.child_by_field_name("right")?;
        }
        Some(value)
    }

    /// The `string` nodes of a literal or an implicit concatenation of
    /// literals (optionally parenthesized); `None` for anything else.
    fn literal_group(&self, node: Node<'tree>) -> Option<Vec<Node<'tree>>> {
        match node.kind() {
            "string" => self.is_plain_literal(node).then(|| vec![node]),
            "concatenated_string" => {
                let mut cursor = node.walk();
                let mut strings = Vec::new();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "string" if self.is_plain_literal(child) => strings.push(child),
                        "comment" => {}
                        _ => return None,
                    }
                }
                Some(strings)
            }
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let mut inner = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment");
                let only = inner.next()?;
                if inner.next().is_some() {
                    return None;
                }
                self.literal_group(only)
            }
            _ => None,
        }
    }

    /// A `str` literal without substitutions: no `f` or `b` prefix.
    fn is_plain_literal(&self, string: Node<'tree>) -> bool {
        string.child(0).is_some_and(|start| {
            start.kind() == "string_start"
                && !self
                    .text(start)
                    .bytes()
                    .any(|byte| matches!(byte, b'f' | b'F' | b'b' | b'B'))
        })
    }
}

/// The styles the patch line kinds render with: the edit-diff look the
/// refinement diff rows use (`+`/`-` markers in the diff colours on the
/// diff backgrounds, the text in the code-block colour on them; plain diff
/// colours without a background below truecolor).
#[derive(Debug, Clone, Copy)]
struct PatchPalette {
    envelope: Style,
    file_marker: Style,
    file_path: Style,
    hunk: Style,
    added_marker: Style,
    added_text: Style,
    removed_marker: Style,
    removed_text: Style,
    context: Style,
}

impl PatchPalette {
    fn from_theme(theme: &Theme) -> Self {
        let file_marker = theme.fg_style(ThemeColor::MdHeading);
        let (added_text, removed_text, added_bg, removed_bg) = if theme.mode == ColorMode::TrueColor
        {
            (
                theme.fg_style(ThemeColor::MdCodeBlock),
                theme.fg_style(ThemeColor::MdCodeBlock),
                theme.bg_style(ThemeBg::ToolDiffAddedBg),
                theme.bg_style(ThemeBg::ToolDiffRemovedBg),
            )
        } else {
            (
                theme.fg_style(ThemeColor::ToolDiffAdded),
                theme.fg_style(ThemeColor::ToolDiffRemoved),
                Style::default(),
                Style::default(),
            )
        };
        Self {
            envelope: theme.fg_style(ThemeColor::Dim),
            file_marker,
            file_path: file_marker.add_modifier(Modifier::BOLD),
            hunk: theme.fg_style(ThemeColor::SyntaxFunction),
            added_marker: theme.fg_style(ThemeColor::ToolDiffAdded).patch(added_bg),
            added_text: added_text.patch(added_bg),
            removed_marker: theme
                .fg_style(ThemeColor::ToolDiffRemoved)
                .patch(removed_bg),
            removed_text: removed_text.patch(removed_bg),
            context: theme.fg_style(ThemeColor::ToolDiffContext),
        }
    }
}

const ENVELOPE_MARKERS: [&str; 3] = ["*** Begin Patch", "*** End Patch", "*** End of File"];
const FILE_MARKERS: [&str; 4] = [
    "*** Update File:",
    "*** Add File:",
    "*** Delete File:",
    "*** Move to:",
];

/// The restyled ranges (offset by `at`, the piece's column in its source
/// line) for one source line's piece of a patch literal.
fn line_segments(piece: &str, at: usize, palette: &PatchPalette) -> Vec<(Range<usize>, Style)> {
    let whole = |style: Style| vec![(at..at + piece.len(), style)];
    let marked = |marker: Style, text: Style| {
        let mut segments = vec![(at..at + 1, marker)];
        if piece.len() > 1 {
            segments.push((at + 1..at + piece.len(), text));
        }
        segments
    };
    if piece.is_empty() {
        return Vec::new();
    }
    if ENVELOPE_MARKERS
        .iter()
        .any(|marker| piece.starts_with(marker))
    {
        return whole(palette.envelope);
    }
    if let Some(marker) = FILE_MARKERS
        .iter()
        .find(|marker| piece.starts_with(**marker))
    {
        let path_start = piece.len() - piece[marker.len()..].trim_start().len();
        let mut segments = vec![(at..at + path_start, palette.file_marker)];
        if path_start < piece.len() {
            segments.push((at + path_start..at + piece.len(), palette.file_path));
        }
        return segments;
    }
    match piece.as_bytes()[0] {
        b'@' if piece.starts_with("@@") => whole(palette.hunk),
        b'+' => marked(palette.added_marker, palette.added_text),
        b'-' => marked(palette.removed_marker, palette.removed_text),
        b' ' => whole(palette.context),
        _ => Vec::new(),
    }
}

/// Re-split `line`'s spans at the segment boundaries and give the bytes
/// inside each segment its style. `segments` are sorted, disjoint, and in
/// the line's byte coordinates.
fn restyle(line: &mut Line, segments: &[(Range<usize>, Style)]) {
    if segments.is_empty() {
        return;
    }
    let mut out: Line = Vec::with_capacity(line.len() + segments.len() * 2);
    let mut pending = segments.iter().peekable();
    let mut offset = 0usize;
    for span in line.drain(..) {
        let span_start = offset;
        let span_end = offset + span.content.len();
        offset = span_end;
        while pending
            .next_if(|(range, _)| range.end <= span_start)
            .is_some()
        {}
        if pending
            .peek()
            .is_none_or(|(range, _)| range.start >= span_end)
        {
            out.push(span);
            continue;
        }
        let mut cursor = span_start;
        while cursor < span_end {
            let (end, style) = match pending.peek() {
                Some((range, style)) if range.start <= cursor => (range.end.min(span_end), *style),
                Some((range, _)) if range.start < span_end => (range.start, span.style),
                _ => (span_end, span.style),
            };
            out.push(Span::styled(
                &span.content[cursor - span_start..end - span_start],
                style,
            ));
            cursor = end;
            while pending.next_if(|(range, _)| range.end <= cursor).is_some() {}
        }
    }
    *line = out;
}

#[cfg(test)]
mod tests;
