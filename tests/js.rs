//! Debugging JavaScript compiled to Wasm by NightMonkey (`nightmonkey
//! --debug-info`): source lines, JS locals and globals, and JS values
//! pretty-printed from the engine's heap.
//!
//! NightMonkey is not a dependency: these run when `NIGHTMONKEY_DEMO_WASM`
//! names `demo.js` compiled with `--debug-info` (see the
//! nightmonkey-dwarf-debugging tree's `build.sh`).

mod common;

use common::*;
use dapper::core::Dapper;
use dapper::tui::{App, testing::render};

#[test]
fn js_demo_tui() {
    let Some(wt) = wasmtime() else {
        return;
    };
    let Some(prog) = std::env::var_os("NIGHTMONKEY_DEMO_WASM") else {
        eprintln!("skipping: NIGHTMONKEY_DEMO_WASM not set");
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

    app.command(&mut d, "b demo.js:31");
    app.command(&mut d, "c");
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    for want in [
        " Source: demo.js ",
        "31 ►   print(s, d, things.items.length, counter);",
        "main (this=[object ShellWindowProxy]) demo.js:31",
        "p = Point {x: 3, y: 4}",
        "d = 25",
        "s = \"hello, world\"",
        "Breakpoint 1, main (this=[object ShellWindowProxy]) at demo.js:31",
    ] {
        assert!(screen.contains(want), "missing `{want}`");
    }

    app.command(&mut d, "p things");
    app.command(&mut d, "p things.items[3]");
    app.command(&mut d, "p greeting");
    app.command(&mut d, "p p.norm2");
    let screen = render(&app, &d, 160, 50);
    eprintln!("{screen}");
    for want in [
        "{name: \"thing\", count: 5, items: [0, 2, 4, 6, 8], nested: {a: 1.5, b: null}}",
        "= 6",
        "= \"hello\"",
        "= [Function: norm2]",
    ] {
        assert!(screen.contains(want), "missing `{want}`");
    }
    let _ = child.kill();
}
