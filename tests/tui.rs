//! Tests of the terminal UI, rendered headlessly with ratatui's
//! `TestBackend` while debugging a real program.

mod common;

use common::*;
use dapper::core::Dapper;
use dapper::tui::{App, Focus, testing::render, testing::select_variable};
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
    app.command(&mut d, "display tri.points[1]");
    app.command(&mut d, "mem &tri");
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    for want in [
        " Source: debug_tui_c.c ",
        "●   60 ►   int r = area(&tri);",
        " Stack ",
        "*#0  main () debug_tui_c.c:60",
        " Variables ",
        "▾ Display",
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
    // Rows: Display, tri.points[1], Arguments, Locals, tri.
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

#[test]
fn tui_reverse_execution_and_watchpoints() {
    let (Some(wt), Some(prog)) = (wasmtime(), build_c("debug_tui_c_history")) else {
        return;
    };
    let trace = out_dir().join("tui_history.trace");
    let status = std::process::Command::new(&wt)
        .arg("run")
        .arg(format!("--record={}", trace.display()))
        .arg(&prog)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let (mut child, addr) =
        dapper::spawn_wasmtime_replay(wt.to_str().unwrap(), &[], trace.to_str().unwrap(), true)
            .unwrap();
    let client = dapper::connect(&addr).unwrap();
    let mut d = Dapper::connect(client).unwrap();
    assert!(d.reversible);
    let mut app = App::new(child.stdout.take());

    app.command(&mut d, "b debug_tui_c_history.c:30");
    app.key(&mut d, key(KeyCode::F(5)));
    app.key(&mut d, key(KeyCode::F(5)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("●   30 ►     add(&acc, sq);"));
    assert!(screen.contains("S-F5/S-F10/S-F7/S-F8 reverse"));

    // Shift-F10 steps back a line, over the call of `square`.
    let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
    app.key(&mut d, shift(KeyCode::F(10)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("29 ►     int sq = square(i);"));

    // Watch `acc.total` from the variables pane: expand `acc`, select
    // `total`, and press `w`.
    for _ in 0..3 {
        app.key(&mut d, key(KeyCode::Tab));
    }
    assert_eq!(app.focus, Focus::Variables);
    assert!(select_variable(&mut app, "acc = "));
    app.key(&mut d, key(KeyCode::Enter));
    assert!(select_variable(&mut app, "total = "));
    app.key(&mut d, key(KeyCode::Char('w')));
    app.command(&mut d, "delete 1");
    // Forward to the write, then (Shift-F5) back to it.
    app.key(&mut d, key(KeyCode::F(5)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("Watchpoint 2: acc.total (memory"));
    assert!(screen.contains("Old value = 1"));
    assert!(screen.contains("New value = 5"));
    assert!(screen.contains("22 ►   acc->count++;"));
    app.key(&mut d, shift(KeyCode::F(5)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("acc.total (running backward: stopped before the write)"));
    assert!(screen.contains("21 ►   acc->total += v;"));
    // Shift-F7 / Shift-F8 step back by instructions.
    app.key(&mut d, shift(KeyCode::F(7)));
    app.key(&mut d, shift(KeyCode::F(8)));
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    assert!(screen.contains("(dapper) reverse-stepi"));
    assert!(screen.contains("(dapper) reverse-nexti"));

    d.disconnect();
    let _ = child.kill();
    child.wait().unwrap();
}
