//! User-level verifier for the `apply_patch` literal colouring: a replayed
//! session paints the patch rows in diff colours, while a plain cell's
//! lookalike string keeps the string colour.

use pa_tui::chat::Detail;
use pa_tui::session::{parse_jsonl, JsonlSessionStream, SessionStream};
use pa_tui::theme::{ColorMode, Theme, ThemeBg, ThemeColor};
use pa_tui::view::AgentView;
use pa_tui::{Line, Span};
use serde_json::json;

const PATCH_CELL: &str = "await apply_patch('''*** Begin Patch\n*** Update File: src/app.py\n@@ def main():\n-    print(\"hi\")\n+    print(\"hello\")\n     return\n*** End Patch''')";
const PLAIN_CELL: &str = "note = '''*** Begin Patch\n-    print(\"hi\")\n*** End Patch'''";

/// The expanded cells' first code rows (the `╰─ ` gutter; the summary row
/// above previews the same text).
const PATCH_ROW: &str = "\u{2570}\u{2500} await apply_patch";
const PLAIN_ROW: &str = "\u{2570}\u{2500} note = ";

fn session() -> String {
    let usage = json!({"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}});
    let entries = [
        json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "rename the greeting"}], "timestamp": 1}}),
        json!({"type": "message", "message": {"role": "assistant", "content": [
            {"type": "toolCall", "id": "toolu_1", "name": "python_repl", "arguments": {"code": PATCH_CELL}},
        ], "api": "openai-completions", "provider": "prime-inference", "model": "m", "usage": usage, "stopReason": "toolUse", "timestamp": 2}}),
        json!({"type": "message", "message": {"role": "toolResult", "toolCallId": "toolu_1", "toolName": "python_repl", "content": [], "details": {"status": "ok", "durationMs": 4}, "isError": false, "timestamp": 3}}),
        json!({"type": "message", "message": {"role": "assistant", "content": [
            {"type": "toolCall", "id": "toolu_2", "name": "python_repl", "arguments": {"code": PLAIN_CELL}},
        ], "api": "openai-completions", "provider": "prime-inference", "model": "m", "usage": usage, "stopReason": "toolUse", "timestamp": 4}}),
        json!({"type": "message", "message": {"role": "toolResult", "toolCallId": "toolu_2", "toolName": "python_repl", "content": [], "details": {"status": "ok", "durationMs": 1}, "isError": false, "timestamp": 5}}),
    ];
    entries.iter().fold(String::new(), |mut jsonl, entry| {
        jsonl.push_str(&entry.to_string());
        jsonl.push('\n');
        jsonl
    })
}

/// The painted frame, terminal escape spans (OSC 133 marks, hyperlinks)
/// dropped.
fn frame(theme: &Theme) -> Vec<Line> {
    let entries = parse_jsonl(&session()).expect("session parses");
    let mut stream = JsonlSessionStream::from_entries(entries);
    let mut view = AgentView::new(theme.clone());
    view.detail = Detail::All;
    while let pa_tui::session::SessionEvent::Item(item) = stream.poll().expect("poll") {
        view.push(item);
    }
    view.render_frame(100, 40)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .filter(|span| !span.content.contains('\u{1b}'))
                .collect()
        })
        .collect()
}

fn text(row: &Line) -> String {
    row.iter().map(|span| span.content.as_str()).collect()
}

/// The row whose text contains `needle`, after the first row containing
/// `after`.
fn row<'a>(frame: &'a [Line], after: &str, needle: &str) -> &'a Line {
    let start = frame
        .iter()
        .position(|row| text(row).contains(after))
        .unwrap_or_else(|| {
            panic!(
                "no {after:?} row in {:#?}",
                frame.iter().map(text).collect::<Vec<_>>()
            )
        });
    frame[start..]
        .iter()
        .find(|row| text(row).contains(needle))
        .unwrap_or_else(|| panic!("no {needle:?} row after {after:?}"))
}

/// The spans past the card's left margin and gutter (`" "` + the
/// `╰─ `/indent prefix), trailing padding dropped.
fn body(row: &Line) -> Vec<Span> {
    let mut spans: Vec<Span> = row.iter().skip(2).cloned().collect();
    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    spans
}

#[test]
fn a_replayed_apply_patch_cell_paints_the_patch_as_a_diff() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let frame = frame(&theme);
    let fg = |color| theme.fg_style(color);
    let string = fg(ThemeColor::SyntaxString);
    let added_bg = theme.bg_style(ThemeBg::ToolDiffAddedBg);
    let span = Span::styled;
    assert_eq!(
        body(row(&frame, PATCH_ROW, PATCH_ROW)),
        vec![
            span("await", fg(ThemeColor::SyntaxKeyword)),
            Span::raw(" apply_patch("),
            span("'''", string),
            span("*** Begin Patch", fg(ThemeColor::Dim)),
        ]
    );
    assert_eq!(
        body(row(&frame, PATCH_ROW, "print(\"hello\")")),
        vec![
            span("+", fg(ThemeColor::ToolDiffAdded).patch(added_bg)),
            span(
                "    print(\"hello\")",
                fg(ThemeColor::MdCodeBlock).patch(added_bg)
            ),
        ]
    );
    // The plain cell's lookalike string is not passed to apply_patch: it
    // keeps the string colour.
    assert_eq!(
        body(row(&frame, PLAIN_ROW, PLAIN_ROW)),
        vec![Span::raw("note = "), span("'''*** Begin Patch", string)]
    );
    assert_eq!(
        body(row(&frame, PLAIN_ROW, "print(\"hi\")")),
        vec![span("-    print(\"hi\")", string)]
    );
}
