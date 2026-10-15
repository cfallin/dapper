//! Tests of the terminal UI, rendered headlessly with ratatui's
//! `TestBackend` while debugging a real program.

mod common;

use common::*;
use dapper::core::Dapper;
use dapper::tui::{App, Focus, testing::render};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn tui_panes() {
    let (Some(wt), Some(prog)) = (wasmtime(), build_c("debug_tui_c")) else {
        return;
    };
    let (mut child, addr) = dapper::spawn_wasmtime(
        wt.to_str().unwrap(),
        &[],
        &[prog.to_str().unwrap().to_string()],
        true,
    )
    .unwrap();
    let client = dapper::connect(&addr).unwrap();
    let mut d = Dapper::connect(client).unwrap();
    let mut app = App::new(child.stdout.take());

    app.command(&mut d, "b debug_tui_c.c:60");
    app.command(&mut d, "c");
    app.command(&mut d, "watch tri.points[1]");
    app.command(&mut d, "mem &tri");
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    for want in [
        " Source: debug_tui_c.c ",
        "●   60 ►   int r = area(&tri);",
        " Stack ",
        "*#0  main () debug_tui_c.c:60",
        " Variables ",
        "▾ Watch",
        "tri.points[1] = struct Point { .x = 4, .y = 0 }",
        "▾ Locals",
        "▸ tri = struct Shape",
        "▸ Wasm Locals",
        " Disassembly ",
        "=> 0x0003c7 local.get 0",
        " Memory: &tri = 0xffb8 ",
        "(dapper) c",
        "Breakpoint 1, main () at debug_tui_c.c:60",
        "60      int r = area(&tri);",
    ] {
        assert!(screen.contains(want), "missing `{want}`");
    }

    // Expand `tri` in the variables pane, then `points`.
    app.key(&mut d, key(KeyCode::Tab)); // Source
    app.key(&mut d, key(KeyCode::Tab)); // Stack
    app.key(&mut d, key(KeyCode::Tab)); // Variables
    assert_eq!(app.focus, Focus::Variables);
    // Rows: Watch, tri.points[1], Arguments, Locals, tri.
    for _ in 0..4 {
        app.key(&mut d, key(KeyCode::Down));
    }
    app.key(&mut d, key(KeyCode::Enter));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("▾ tri = struct Shape"));
    assert!(screen.contains("    color = GREEN"));
    assert!(screen.contains("▸ points = {{ .x = 0, .y = 0 }"));

    // F11 steps into `area`; the panes follow.
    app.key(&mut d, key(KeyCode::F(11)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("33 ►   int a = s->points[1].x - s->points[0].x;"));
    assert!(screen.contains("*#0  area (s=(struct Shape *) 0xffb8) debug_tui_c.c:33"));
    assert!(screen.contains("▸ s = (struct Shape *) 0xffb8"));

    // Toggle a breakpoint at the cursor in the source pane (F9), and run to
    // it.
    app.key(&mut d, key(KeyCode::Esc));
    app.key(&mut d, key(KeyCode::Tab)); // Source
    app.key(&mut d, key(KeyCode::Down));
    app.key(&mut d, key(KeyCode::Down));
    app.key(&mut d, key(KeyCode::F(9)));
    app.key(&mut d, key(KeyCode::F(5)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("●   35 ►   int result = a * b / 2;"));
    assert!(screen.contains("Breakpoint 2, area"));

    // Run to completion; program output appears in the console.
    app.command(&mut d, "delete");
    app.command(&mut d, "c");
    std::thread::sleep(std::time::Duration::from_millis(200));
    app.refresh(&mut d);
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("[Program exited]"));
    assert!(screen.contains("area = 6 on the heap 30"));
    assert!(screen.contains("[exited]"));
    d.disconnect();
    child.wait().unwrap();
}
