use super::*;
use crate::theme::{ColorMode, Theme};
use crate::tool_card::highlight::{highlight_python, SyntaxPalette};

fn bodies(code: &str) -> Vec<&str> {
    patch_literal_bodies(code)
        .into_iter()
        .map(|body| &code[body])
        .collect()
}

fn theme(mode: ColorMode) -> Theme {
    Theme::builtin("prime", mode)
}

fn rendered(code: &str, theme: &Theme) -> Vec<Line> {
    let mut lines = highlight_python(code, &SyntaxPalette::from_theme(theme));
    overlay_patch_literals(code, &mut lines, theme);
    lines
}

#[test]
fn the_call_forms_pass_their_literal() {
    let cases = [
        ("apply_patch('''X''')", vec!["X"]),
        ("await apply_patch('''X''')", vec!["X"]),
        ("await apply_patch.run(\"\"\"X\"\"\")", vec!["X"]),
        ("await apply_patch(patch='X')", vec!["X"]),
        ("result = await apply_patch.run(patch=r'X')", vec!["X"]),
        ("await apply_patch(  # the edit\n    'X',\n)", vec!["X"]),
        ("await apply_patch \\\n    ('X')", vec!["X"]),
        (
            "if ok:\n    for _ in [1]:\n        await apply_patch('X')",
            vec!["X"],
        ),
    ];
    for (code, expected) in cases {
        assert_eq!(bodies(code), expected, "{code:?}");
    }
}

#[test]
fn a_name_bound_once_to_a_literal_passes_its_literal() {
    assert_eq!(bodies("P = '''X'''\nawait apply_patch(P)"), vec!["X"]);
    assert_eq!(
        bodies("P: str = ('A'\n     'B')\nawait apply_patch(patch=P)"),
        vec!["A", "B"]
    );
    assert_eq!(bodies("Q = P = 'X'\napply_patch(P)"), vec!["X"]);
}

#[test]
fn computed_arguments_fall_back_to_the_string_colour() {
    let cases = [
        "P = 'X'\nP = 'Y'\nawait apply_patch(P)",
        "P = 'X'\nP += 'Y'\nawait apply_patch(P)",
        "P, R = 'X', 'Y'\nawait apply_patch(P)",
        "for P in ['X']:\n    await apply_patch(P)",
        "await apply_patch(P)\nP = 'X'",
        "await apply_patch(build())",
        "await apply_patch('X' + suffix)",
        "await apply_patch('X'.strip())",
        "await apply_patch(f'''X {path}''')",
        "await apply_patch(F'X')",
        "await apply_patch(rf'X')",
        "await apply_patch(b'X')",
        "await apply_patch('X' f'{y}')",
        "await apply_patch(*parts)",
        "await apply_patch(None, 'X')",
        "await apply_patch(other='X')",
        "await tools.apply_patch('X')",
        "await apply_patch.check('X')",
        "await other.run('X')",
        "def apply_patch(patch='X'):\n    pass",
    ];
    for code in cases {
        assert_eq!(bodies(code), Vec::<&str>::new(), "{code:?}");
    }
}

#[test]
fn mentions_in_comments_and_strings_are_not_calls() {
    let cases = [
        "# await apply_patch('''X''')\nx = 1",
        "x = 1  # apply_patch('X')",
        "s = \"apply_patch('X')\"",
        "s = '''\nawait apply_patch(\"X\")\n'''",
        "print(apply_patch.__doc__)",
    ];
    for code in cases {
        assert_eq!(bodies(code), Vec::<&str>::new(), "{code:?}");
    }
}

#[test]
fn every_plain_prefix_and_quote_kind_is_a_literal() {
    for prefix in ["", "r", "R", "u", "U"] {
        for quote in ["'", "\"", "'''", "\"\"\""] {
            let code = format!("await apply_patch({prefix}{quote}X{quote})");
            assert_eq!(bodies(&code), vec!["X"], "{code:?}");
        }
    }
    for prefix in ["f", "F", "b", "B", "rb", "Rb", "bR", "BR", "fr", "rF", "FR"] {
        let code = format!("await apply_patch({prefix}'X')");
        assert_eq!(bodies(&code), Vec::<&str>::new(), "{code:?}");
    }
}

#[test]
fn escapes_and_inner_quotes_stay_inside_the_literal() {
    assert_eq!(bodies(r"apply_patch('a\'b')"), vec![r"a\'b"]);
    assert_eq!(
        bodies(r#"apply_patch("say \"hi\"")"#),
        vec![r#"say \"hi\""#]
    );
    assert_eq!(
        bodies("apply_patch('''+x = \"\"\"doc\"\"\"\n-y = 'q' ''')"),
        vec!["+x = \"\"\"doc\"\"\"\n-y = 'q' "]
    );
    assert_eq!(
        bodies("apply_patch(\"\"\"+s = '''a'''\n\"\"\")"),
        vec!["+s = '''a'''\n"]
    );
    assert_eq!(bodies("apply_patch('a\\\n-b')"), vec!["a\\\n-b"]);
    assert_eq!(bodies(r"apply_patch(r'+\d\'')"), vec![r"+\d\'"]);
}

#[test]
fn implicit_concatenation_passes_every_piece() {
    assert_eq!(
        bodies("await apply_patch(\n    '*** Begin Patch\\n'  # start\n    \"-a\\n\"\n    r'''+b'''\n)"),
        vec!["*** Begin Patch\\n", "-a\\n", "+b"]
    );
    assert_eq!(bodies("await apply_patch(('A' 'B'))"), vec!["A", "B"]);
}

#[test]
fn crlf_cells_keep_the_carriage_returns_in_the_body() {
    assert_eq!(
        bodies("x = 1\r\nawait apply_patch('''*** Begin Patch\r\n-a\r\n*** End Patch''')\r\n"),
        vec!["*** Begin Patch\r\n-a\r\n*** End Patch"]
    );
}

/// A cell still streaming in ends inside the literal: tree-sitter alone
/// leaves the opening quote in an ERROR node, and the closed-tail reparse
/// recovers the call.
#[test]
fn an_unclosed_triple_quoted_tail_runs_to_the_end_of_the_cell() {
    let code = "await apply_patch('''*** Begin Patch\n*** Update File: a.py\n@@\n-x";
    let tree = parse(code).expect("parses");
    let sexp = tree.root_node().to_sexp();
    assert!(
        sexp.contains("ERROR") && !sexp.contains("(call") && !sexp.contains("(string "),
        "tree-sitter alone recovers no call and no string: {sexp}"
    );
    assert_eq!(
        bodies(code),
        vec!["*** Begin Patch\n*** Update File: a.py\n@@\n-x"]
    );
    let cases = [
        ("await apply_patch('''", vec![]),
        ("await apply_patch('''*", vec!["*"]),
        (
            "await apply_patch(patch=\"\"\"+a \"q\" '''\n",
            vec!["+a \"q\" '''\n"],
        ),
        ("if ok:\n    await apply_patch('''-a\n+b", vec!["-a\n+b"]),
        ("await apply_patch('''+a\\", vec!["+a\\"]),
        ("await apply_patch(\n    '''-a\n", vec!["-a\n"]),
        ("P = '''-a\n+b", vec![]),
        ("print('''-a\n+b", vec![]),
    ];
    for (code, expected) in cases {
        assert_eq!(bodies(code), expected, "{code:?}");
    }
}

#[test]
fn a_patch_literal_renders_as_a_diff() {
    let theme = theme(ColorMode::TrueColor);
    let syntax = SyntaxPalette::from_theme(&theme);
    let patch = PatchPalette::from_theme(&theme);
    let code = "await apply_patch('''*** Begin Patch\n*** Update File: src/app.py\n@@ def main():\n-    print(\"hi\")\n+    print(\"hello\")\n     return\n\n*** Add File: notes.txt\n+first\n*** End of File\nplain text\n*** End Patch''')";
    let span = Span::styled;
    assert_eq!(
        rendered(code, &theme),
        vec![
            vec![
                span("await", syntax.keyword),
                Span::raw(" apply_patch("),
                span("'''", syntax.string),
                span("*** Begin Patch", patch.envelope),
            ],
            vec![
                span("*** Update File: ", patch.file_marker),
                span("src/app.py", patch.file_path),
            ],
            vec![span("@@ def main():", patch.hunk)],
            vec![
                span("-", patch.removed_marker),
                span("    print(\"hi\")", patch.removed_text),
            ],
            vec![
                span("+", patch.added_marker),
                span("    print(\"hello\")", patch.added_text),
            ],
            vec![span("     return", patch.context)],
            vec![],
            vec![
                span("*** Add File: ", patch.file_marker),
                span("notes.txt", patch.file_path),
            ],
            vec![
                span("+", patch.added_marker),
                span("first", patch.added_text)
            ],
            vec![span("*** End of File", patch.envelope)],
            vec![span("plain text", syntax.string)],
            vec![
                span("*** End Patch", patch.envelope),
                span("'''", syntax.string),
                Span::raw(")"),
            ],
        ]
    );
}

/// The diff look matches the edit-diff rows: the marker in the diff colour
/// on the diff background, the text in the code-block colour on it; plain
/// diff colours below truecolor.
#[test]
fn the_palette_is_the_edit_diff_look() {
    let truecolor = theme(ColorMode::TrueColor);
    let palette = PatchPalette::from_theme(&truecolor);
    let added_bg = truecolor.bg_style(ThemeBg::ToolDiffAddedBg);
    assert_eq!(
        (palette.added_marker, palette.added_text),
        (
            truecolor
                .fg_style(ThemeColor::ToolDiffAdded)
                .patch(added_bg),
            truecolor.fg_style(ThemeColor::MdCodeBlock).patch(added_bg),
        )
    );
    let ansi = theme(ColorMode::Color256);
    let palette = PatchPalette::from_theme(&ansi);
    assert_eq!(
        (palette.removed_marker, palette.removed_text),
        (
            ansi.fg_style(ThemeColor::ToolDiffRemoved),
            ansi.fg_style(ThemeColor::ToolDiffRemoved),
        )
    );
}

#[test]
fn concatenated_pieces_colour_each_piece_between_its_quotes() {
    let theme = theme(ColorMode::TrueColor);
    let syntax = SyntaxPalette::from_theme(&theme);
    let patch = PatchPalette::from_theme(&theme);
    let code = "P = ('-a\\n'\n     '+b\\n')\napply_patch(P)";
    let span = Span::styled;
    assert_eq!(
        rendered(code, &theme),
        vec![
            vec![
                Span::raw("P = ("),
                span("'", syntax.string),
                span("-", patch.removed_marker),
                span("a\\n", patch.removed_text),
                span("'", syntax.string),
            ],
            vec![
                Span::raw("     "),
                span("'", syntax.string),
                span("+", patch.added_marker),
                span("b\\n", patch.added_text),
                span("'", syntax.string),
                Span::raw(")"),
            ],
            vec![Span::raw("apply_patch(P)")],
        ]
    );
}

#[test]
fn a_streaming_cell_colours_what_has_arrived() {
    let theme = theme(ColorMode::TrueColor);
    let syntax = SyntaxPalette::from_theme(&theme);
    let patch = PatchPalette::from_theme(&theme);
    let code = "await apply_patch('''*** Begin Patch\n*** Delete File: old.py\n@@\n-gone\n+ne";
    let span = Span::styled;
    assert_eq!(
        rendered(code, &theme),
        vec![
            vec![
                span("await", syntax.keyword),
                Span::raw(" apply_patch("),
                span("'''", syntax.string),
                span("*** Begin Patch", patch.envelope),
            ],
            vec![
                span("*** Delete File: ", patch.file_marker),
                span("old.py", patch.file_path),
            ],
            vec![span("@@", patch.hunk)],
            vec![
                span("-", patch.removed_marker),
                span("gone", patch.removed_text)
            ],
            vec![span("+", patch.added_marker), span("ne", patch.added_text)],
        ]
    );
}

/// Every prefix of a streaming cell, cut anywhere inside the literal,
/// renders its complete lines exactly as the finished cell does: nothing
/// recolours as more arrives.
#[test]
fn streaming_prefixes_never_recolour_finished_lines() {
    let theme = theme(ColorMode::TrueColor);
    let code = "x = 1\nawait apply_patch('''*** Begin Patch\n*** Update File: a.py\n@@ def f():\n-    return 1\n+    return 2\n     pass\n*** End Patch''')\nprint(x)";
    let finished = rendered(code, &theme);
    let open = code.find("'''").expect("opening quote") + 3;
    let close = code.rfind("'''").expect("closing quote");
    for cut in open..=close {
        let prefix = &code[..cut];
        let lines = rendered(prefix, &theme);
        // The last source line may still grow; the ones before it are done.
        let complete = prefix.split('\n').count() - 1;
        assert_eq!(
            lines[..complete],
            finished[..complete],
            "cut at {cut}: {prefix:?}"
        );
    }
}

#[test]
fn cells_without_a_patch_call_are_untouched() {
    let theme = theme(ColorMode::TrueColor);
    for code in [
        "text = '''*** Begin Patch\n-a\n+b\n*** End Patch'''\nprint(text)",
        "# apply_patch('''-a''')\ns = '-a'",
        "await apply_patch(make())\nt = '''+b'''",
    ] {
        assert_eq!(
            rendered(code, &theme),
            highlight_python(code, &SyntaxPalette::from_theme(&theme)),
            "{code:?}"
        );
    }
}

#[test]
fn restyle_splits_spans_at_segment_edges() {
    let a = Style::default().add_modifier(Modifier::ITALIC);
    let b = Style::default().add_modifier(Modifier::BOLD);
    let mut line = vec![
        Span::styled("abc", a),
        Span::raw("def"),
        Span::styled("gh", a),
    ];
    restyle(&mut line, &[(1..4, b), (5..7, Style::default())]);
    assert_eq!(
        line,
        vec![
            Span::styled("a", a),
            Span::styled("bc", b),
            Span::styled("d", b),
            Span::raw("e"),
            Span::raw("f"),
            Span::raw("g"),
            Span::styled("h", a),
        ]
    );
}
