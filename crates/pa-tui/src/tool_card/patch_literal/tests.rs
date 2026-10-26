use super::*;
use crate::theme::{ColorMode, Theme};
use crate::tool_card::highlight::{highlight_python, SyntaxPalette};

fn bodies(code: &str) -> Vec<&str> {
    parse_cell(code, None)
        .bodies
        .iter()
        .map(|body| &code[body.clone()])
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

/// The literal bodies detection finds, including a streaming cell's
/// unclosed triple-quoted tail (closed for the parse, clipped to the cell).
#[test]
fn detected_literal_bodies() {
    let mut cases: Vec<(String, Vec<&str>)> = [
        ("apply_patch('''X''')", vec!["X"]),
        ("await apply_patch.run(\"\"\"X\"\"\")", vec!["X"]),
        ("result = await apply_patch.run(patch=r'X')", vec!["X"]),
        ("await apply_patch(  # the edit\n    'X',\n)", vec!["X"]),
        ("await apply_patch \\\n    ('X')", vec!["X"]),
        (
            "if ok:\n    for _ in [1]:\n        await apply_patch('X')",
            vec!["X"],
        ),
        ("P = '''X'''\nawait apply_patch(P)", vec!["X"]),
        (
            "P: str = ('A'\n     'B')\nawait apply_patch(patch=P)",
            vec!["A", "B"],
        ),
        ("Q = P = 'X'\napply_patch(P)", vec!["X"]),
        (r"apply_patch('a\'b')", vec![r"a\'b"]),
        (
            "apply_patch('''+x = \"\"\"doc\"\"\"\n-y = 'q' ''')",
            vec!["+x = \"\"\"doc\"\"\"\n-y = 'q' "],
        ),
        ("apply_patch('a\\\n-b')", vec!["a\\\n-b"]),
        (r"apply_patch(r'+\d\'')", vec![r"+\d\'"]),
        (
            "await apply_patch(\n    '*** Begin Patch\\n'  # start\n    \"-a\\n\"\n    r'''+b'''\n)",
            vec!["*** Begin Patch\\n", "-a\\n", "+b"],
        ),
        ("await apply_patch(('A' 'B'))", vec!["A", "B"]),
        (
            "x = 1\r\nawait apply_patch('''-a\r\n+b''')\r\n",
            vec!["-a\r\n+b"],
        ),
        (
            "await apply_patch('''*** Begin Patch\n@@\n-x",
            vec!["*** Begin Patch\n@@\n-x"],
        ),
        ("await apply_patch('''", vec![]),
        (
            "await apply_patch(patch=\"\"\"+a \"q\" '''\n",
            vec!["+a \"q\" '''\n"],
        ),
        ("await apply_patch('''+a\\", vec!["+a\\"]),
        ("await apply_patch(\n    '''-a\n", vec!["-a\n"]),
        ("P = '''-a\n+b", vec![]),
        ("print('''-a\n+b", vec![]),
    ]
    .into_iter()
    .map(|(code, expected)| (code.to_string(), expected))
    .collect();
    for prefix in ["", "r", "R", "u", "U"] {
        for quote in ["'", "\"", "'''", "\"\"\""] {
            cases.push((format!("apply_patch({prefix}{quote}X{quote})"), vec!["X"]));
        }
    }
    for (code, expected) in cases {
        assert_eq!(bodies(&code), expected, "{code:?}");
    }
}

/// Computed arguments, other call shapes, and mentions keep the string colour.
#[test]
fn undetected_arguments() {
    let mut cases: Vec<String> = [
        "P = 'X'\nP = 'Y'\nawait apply_patch(P)",
        "P = 'X'\nP += 'Y'\nawait apply_patch(P)",
        "P, R = 'X', 'Y'\nawait apply_patch(P)",
        "for P in ['X']:\n    await apply_patch(P)",
        "await apply_patch(P)\nP = 'X'",
        "await apply_patch(build())",
        "await apply_patch('X' + suffix)",
        "await apply_patch('X'.strip())",
        "await apply_patch(f'''X {path}''')",
        "await apply_patch('X' f'{y}')",
        "await apply_patch(*parts)",
        "await apply_patch(None, 'X')",
        "await apply_patch(other='X')",
        "await tools.apply_patch('X')",
        "await apply_patch.check('X')",
        "await other.run('X')",
        "def apply_patch(patch='X'):\n    pass",
        "# await apply_patch('''X''')\nx = 1",
        "x = 1  # apply_patch('X')",
        "s = \"apply_patch('X')\"",
        "s = '''\nawait apply_patch(\"X\")\n'''",
        "print(apply_patch.__doc__)",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    for prefix in ["f", "F", "b", "B", "rb", "Rb", "bR", "BR", "fr", "rF", "FR"] {
        cases.push(format!("await apply_patch({prefix}'X')"));
    }
    for code in cases {
        assert_eq!(bodies(&code), Vec::<&str>::new(), "{code:?}");
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

/// Parsing a streaming cell incrementally, chunk after chunk, finds
/// exactly the literals a fresh parse of each prefix finds, through quotes
/// and escapes inside the patch, its close, and code after it.
#[test]
fn incremental_parses_match_fresh_parses() {
    let code = concat!(
        "P = 'x'\n",
        "await apply_patch(r'''*** Begin Patch\n",
        "*** Update File: a.py\n",
        "@@ def f():\n",
        "-    s = 'it' '' \"q\"\n",
        "+    s = \"\\\\d\\'\"\n",
        "*** End Patch''')\n",
        "print('''tail''')\n",
        "await apply_patch(\"\"\"*** Begin Patch\n+a\\\n\"\"\")\n",
        "await apply_patch(P)",
    );
    for step in [1, 2, 5, 11] {
        let mut previous = None;
        let mut end = 0;
        while end < code.len() {
            end = (end + step).min(code.len());
            let cell = parse_cell(&code[..end], previous.take());
            assert_eq!(
                cell.bodies,
                parse_cell(&code[..end], None).bodies,
                "step {step}, cut at {end}: {:?}",
                &code[..end]
            );
            previous = Some(cell);
        }
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

/// Streaming cost of a ~2k-line patch cell arriving in 64-byte chunks,
/// rendered after every chunk as the card repaints: the highlighter (the
/// cell's base colouring, unchanged by this module), the patch overlay, and
/// the overlay's literal detection timed apart. Run in release:
/// `cargo test --release -p pa-tui --lib streaming_a_long_patch -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn streaming_a_long_patch_bench() {
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};
    let mut cell = String::from("await apply_patch(r'''*** Begin Patch\n");
    for file in 0..20 {
        let _ = writeln!(cell, "*** Update File: src/module_{file}.py");
        for hunk in 0..20 {
            let _ = write!(
                cell,
                "@@ def function_{hunk}(value):\n     total = value * {hunk}\n-    return total\n+    return total + {file}\n     # done\n"
            );
        }
    }
    cell.push_str("*** End Patch''')\n");
    let theme = theme(ColorMode::TrueColor);
    let syntax = SyntaxPalette::from_theme(&theme);
    let mut ends: Vec<usize> = (64..cell.len()).step_by(64).collect();
    ends.push(cell.len());
    let mut highlight = Vec::with_capacity(ends.len());
    let mut overlay = Vec::with_capacity(ends.len());
    for &end in &ends {
        let code = &cell[..end];
        let started = Instant::now();
        let mut lines = highlight_python(code, &syntax);
        highlight.push(started.elapsed());
        let started = Instant::now();
        overlay_patch_literals(code, &mut lines, &theme);
        overlay.push(started.elapsed());
    }
    let report = |name: &str, times: &[Duration]| {
        let total: Duration = times.iter().sum();
        let tail = &times[times.len() - 100..];
        eprintln!(
            "{name}: total {total:?}, mean {:?}/chunk, last 100 chunks mean {:?}, max {:?}",
            total / u32::try_from(times.len()).unwrap_or(u32::MAX),
            tail.iter().sum::<Duration>() / 100,
            times.iter().max().copied().unwrap_or_default(),
        );
    };
    eprintln!(
        "{} lines, {} bytes, {} chunks",
        cell.lines().count(),
        cell.len(),
        ends.len()
    );
    report("highlight_python", &highlight);
    report("overlay_patch_literals", &overlay);
    // The literal detection alone: incrementally, as the overlay does, and
    // from scratch for every chunk (the cost before incremental parsing).
    let mut incremental = Vec::with_capacity(ends.len());
    let mut previous = None;
    for &end in &ends {
        let started = Instant::now();
        let parsed = parse_cell(&cell[..end], previous.take());
        incremental.push(started.elapsed());
        previous = Some(parsed);
    }
    report("detection, incremental", &incremental);
    let mut scratch = Vec::with_capacity(ends.len());
    for &end in &ends {
        let started = Instant::now();
        std::hint::black_box(parse_cell(&cell[..end], None));
        scratch.push(started.elapsed());
    }
    report("detection, from scratch", &scratch);
}
