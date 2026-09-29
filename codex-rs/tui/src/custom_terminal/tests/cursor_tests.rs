use super::*;
use crate::terminal_palette::rgb_color;
use pretty_assertions::assert_eq;

// Crossterm memoizes NO_COLOR globally, and VT100Backend forces color globally.
// Run each color-sensitive assertion alone so neither the parent environment nor
// other tests can change its ANSI policy, including under a parallel test harness.
enum ColorEnvironment {
    Ansi,
    NoColor,
}

fn with_color_environment(test_name: &str, colors: ColorEnvironment, test: impl FnOnce()) {
    const CHILD: &str = "CODEX_CURSOR_COLOR_TEST_CHILD";
    let module = module_path!().split_once("::").expect("crate prefix").1;
    let qualified_name = format!("{module}::{test_name}");
    if std::env::var(CHILD).as_deref() == Ok(qualified_name.as_str()) {
        test();
        return;
    }

    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", &qualified_name, "--nocapture"])
        .env(CHILD, &qualified_name)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("FORCE_COLOR");
    match colors {
        ColorEnvironment::Ansi => {
            command.env_remove("NO_COLOR");
        }
        ColorEnvironment::NoColor => {
            command.env("NO_COLOR", "1");
        }
    }
    let output = command.output().expect("run isolated color test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{qualified_name}: {stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "child did not run {qualified_name}: {stdout}"
    );
}

#[test]
fn terminal_draw_respects_no_color() {
    with_color_environment(
        "terminal_draw_respects_no_color",
        ColorEnvironment::NoColor,
        || {
            let mut terminal =
                Terminal::with_options(CaptureBackend::new(/*width*/ 12, /*height*/ 2))
                    .expect("terminal");
            let area = Rect::new(
                /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 1,
            );
            terminal.set_viewport_area(area);
            terminal
                .draw(|frame| {
                    Paragraph::new("anchor")
                        .style(Style::default().bg(rgb_color((80, 80, 80))).bold())
                        .render(area, frame.buffer_mut());
                    frame.set_cursor_position((1, 0));
                })
                .expect("draw");
            let mut parser =
                vt100::Parser::new(/*rows*/ 2, /*cols*/ 12, /*scrollback_len*/ 0);
            parser.process(&terminal.backend().output);
            assert_eq!(parser.screen().contents(), "anchor      ");
            assert_eq!(parser.screen().cursor_position(), (0, 1));
            for column in 0..area.width {
                assert_eq!(
                    parser
                        .screen()
                        .cell(0, column)
                        .expect("viewport cell")
                        .bgcolor(),
                    vt100::Color::Default
                );
            }
        },
    );
}

#[test]
fn terminal_draw_keeps_intermediate_cursor_positions_hidden() {
    let mut terminal =
        Terminal::with_options(CaptureBackend::new(/*width*/ 12, /*height*/ 2)).expect("terminal");
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 2,
    );
    let cursor = (1, 4);
    terminal.set_viewport_area(area);
    let mut parser =
        vt100::Parser::new(/*rows*/ 2, /*cols*/ 12, /*scrollback_len*/ 0);

    for (index, (marker, show_cursor)) in [
        ("x", true),
        ("y", true),
        ("y", true),
        ("", false),
        ("z", true),
    ]
    .into_iter()
    .enumerate()
    {
        terminal.backend_mut().output.clear();
        terminal
            .draw(|frame| {
                let buffer = frame.buffer_mut();
                buffer.set_string(0, 0, "anchor", Style::default());
                if !marker.is_empty() {
                    buffer.set_string(10, 1, marker, Style::default());
                }
                if show_cursor {
                    frame.set_cursor_position((cursor.1, cursor.0));
                }
            })
            .expect("draw");

        // Process one byte at a time to expose cursor motion even if a terminal displays a
        // synchronized frame before all writes have arrived.
        let output = terminal.backend().output();
        for byte in output.bytes() {
            parser.process(&[byte]);
            if index > 0 && !parser.screen().hide_cursor() {
                assert_eq!(parser.screen().cursor_position(), cursor, "frame {index}");
            }
        }
        assert_eq!(parser.screen().hide_cursor(), !show_cursor, "frame {index}");
        assert_eq!(
            parser.screen().cell(1, 10).expect("marker cell").contents(),
            marker,
            "frame {index}"
        );
    }
}

#[test]
fn terminal_draw_repaints_the_cursor_anchor_only_when_its_style_needs_restoring() {
    let mut terminal =
        Terminal::with_options(CaptureBackend::new(/*width*/ 12, /*height*/ 2)).expect("terminal");
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 2,
    );
    terminal.set_viewport_area(area);
    let render = |terminal: &mut Terminal<CaptureBackend>, marker| {
        terminal.backend_mut().output.clear();
        terminal
            .draw(|frame| {
                let buffer = frame.buffer_mut();
                buffer.set_string(0, 0, "transcript", Style::default());
                buffer.set_string(10, 1, marker, Style::default());
                frame.set_cursor_position((4, 1));
            })
            .expect("draw");
        terminal.backend().output()
    };

    let first = render(&mut terminal, "x");
    assert!(first.contains("\x1b[1;1H"));
    let next = render(&mut terminal, "y");
    assert!(next.contains("\x1b[2;11H"));
    assert!(!next.contains("\x1b[1;1H"));

    terminal.invalidate_cursor_state();
    let restored = render(&mut terminal, "y");
    assert!(restored.contains("\x1b[1;1H"));
    let unchanged = render(&mut terminal, "y");
    assert!(!unchanged.contains("\x1b[1;1H"));
}

#[test]
fn terminal_draw_repairs_styled_anchor_on_cursor_only_frames() {
    with_color_environment(
        "terminal_draw_repairs_styled_anchor_on_cursor_only_frames",
        ColorEnvironment::Ansi,
        || {
            let mut terminal =
                Terminal::with_options(CaptureBackend::new(/*width*/ 12, /*height*/ 2))
                    .expect("terminal");
            let area = Rect::new(
                /*x*/ 0, /*y*/ 1, /*width*/ 12, /*height*/ 1,
            );
            terminal.set_viewport_area(area);
            let mut frames = Vec::new();
            let mut parser =
                vt100::Parser::new(/*rows*/ 2, /*cols*/ 12, /*scrollback_len*/ 0);

            for (x, style) in [
                (1, SetCursorStyle::DefaultUserShape),
                (4, SetCursorStyle::DefaultUserShape),
                (3, SetCursorStyle::SteadyBar),
                (1, SetCursorStyle::SteadyBlock),
            ] {
                terminal.backend_mut().output.clear();
                terminal
                    .draw(|frame| {
                        Paragraph::new("ab  cd  ef")
                            .style(Style::default().bg(rgb_color((80, 80, 80))).bold())
                            .render(area, frame.buffer_mut());
                        frame.set_cursor_style(style);
                        frame.set_cursor_position((x, 1));
                    })
                    .expect("draw");

                parser.process(&terminal.backend().output);
                assert_eq!(parser.screen().contents(), "\nab  cd  ef  ");
                assert_eq!(parser.screen().cursor_position(), (1, x));
                for column in 0..area.width {
                    let cell = parser.screen().cell(1, column).expect("viewport cell");
                    assert_eq!(cell.bgcolor(), vt100::Color::Rgb(80, 80, 80));
                    assert!(
                        cell.bold(),
                        "lost anchor or trailing-cell modifier at {column}"
                    );
                }
                frames.push(terminal.backend().output().escape_debug().to_string());
            }
            assert_snapshot!("cursor_style_styled_frames", frames.join("\n"));
        },
    );
}

#[test]
fn terminal_draw_repairs_owned_wide_hyperlink_after_skipped_glyphs() {
    with_color_environment(
        "terminal_draw_repairs_owned_wide_hyperlink_after_skipped_glyphs",
        ColorEnvironment::Ansi,
        || {
            let mut terminal =
                Terminal::with_options(CaptureBackend::new(/*width*/ 8, /*height*/ 1))
                    .expect("terminal");
            let area = Rect::new(
                /*x*/ 0, /*y*/ 0, /*width*/ 8, /*height*/ 1,
            );
            terminal.set_viewport_area(area);
            let mut frames = Vec::new();
            for _ in 0..2 {
                terminal.backend_mut().output.clear();
                terminal
                    .draw(|frame| {
                        let buffer = frame.buffer_mut();
                        buffer.set_string(0, 0, "中x界", Style::default().bg(Color::Blue));
                        buffer[(0, 0)].diff_option = CellDiffOption::Skip;
                        buffer[(2, 0)].diff_option = CellDiffOption::Skip;
                        buffer[(3, 0)].set_symbol("\x1b]8;;https://example.com\x07界\x1b]8;;\x07");
                        buffer[(3, 0)].diff_option = CellDiffOption::ForcedWidth(
                            NonZeroU16::new(/*n*/ 2).expect("wide glyph"),
                        );
                        frame.set_cursor_position((5, 0));
                    })
                    .expect("draw");
                frames.push(terminal.backend().output().escape_debug().to_string());
            }
            assert_snapshot!("cursor_style_owned_wide_frames", frames.join("\n"));
        },
    );
}

#[test]
fn terminal_draw_repairs_single_column_without_scrolling() {
    with_color_environment(
        "terminal_draw_repairs_single_column_without_scrolling",
        ColorEnvironment::Ansi,
        || {
            let mut terminal =
                Terminal::with_options(CaptureBackend::new(/*width*/ 1, /*height*/ 1))
                    .expect("terminal");
            let area = Rect::new(
                /*x*/ 0, /*y*/ 0, /*width*/ 1, /*height*/ 1,
            );
            terminal.set_viewport_area(area);
            let mut frames = Vec::new();
            let mut parser =
                vt100::Parser::new(/*rows*/ 1, /*cols*/ 1, /*scrollback_len*/ 1);
            for _ in 0..3 {
                terminal.backend_mut().output.clear();
                terminal
                    .draw(|frame| {
                        Paragraph::new("x")
                            .style(Style::default().bg(Color::Blue))
                            .render(area, frame.buffer_mut());
                        frame.set_cursor_position((0, 0));
                    })
                    .expect("draw");
                parser.process(&terminal.backend().output);
                assert_eq!(parser.screen().contents(), "x");
                assert_eq!(parser.screen().cursor_position(), (0, 0));
                frames.push(terminal.backend().output().escape_debug().to_string());
            }
            parser.screen_mut().set_scrollback(/*rows*/ 1);
            assert_eq!(parser.screen().scrollback(), 0);
            assert_snapshot!("cursor_style_single_column_frames", frames.join("\n"));
        },
    );
}

#[test]
fn terminal_draw_omits_cursor_style_without_an_owned_glyph() {
    with_color_environment(
        "terminal_draw_omits_cursor_style_without_an_owned_glyph",
        ColorEnvironment::Ansi,
        || {
            let mut terminal =
                Terminal::with_options(CaptureBackend::new(/*width*/ 2, /*height*/ 1))
                    .expect("terminal");
            for width in [0, 2] {
                terminal.set_viewport_area(Rect::new(
                    /*x*/ 0, /*y*/ 0, width, /*height*/ 1,
                ));
                for buffer in &mut terminal.buffers {
                    for cell in &mut buffer.content {
                        cell.diff_option = CellDiffOption::Skip;
                    }
                }
                terminal.backend_mut().output.clear();
                terminal
                    .draw(|frame| {
                        for cell in &mut frame.buffer_mut().content {
                            cell.diff_option = CellDiffOption::Skip;
                        }
                        frame.set_cursor_style(SetCursorStyle::SteadyBar);
                        frame.set_cursor_position((1, 0));
                    })
                    .expect("draw");
                assert_eq!(
                    terminal.backend().output(),
                    "\x1b[39m\x1b[49m\x1b[0m\x1b[1;2H\x1b[?25h"
                );
            }
            terminal.set_viewport_area(Rect::default());
            terminal.backend_mut().output.clear();
            terminal.draw(|_| {}).expect("hide cursor");
            assert_eq!(
                terminal.backend().output(),
                "\x1b[39m\x1b[49m\x1b[0m\x1b[?25l"
            );
        },
    );
}
