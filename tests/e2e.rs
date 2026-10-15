//! End-to-end tests: run `dapper --run` against Wasmtime's DAP server
//! (`wasmtime run --debug-dap`) with command scripts, and check the
//! transcripts.
//!
//! Requirements (tests are skipped, with a message, if missing):
//! - `WASMTIME`: path to a `wasmtime` built with `--features debug-dap-tui`
//!   (default: `../wasmtime/target/debug/wasmtime`);
//! - `WASI_SDK_PATH` for the C test program;
//! - a Rust toolchain with the `wasm32-wasip1` target for the Rust one.

mod common;

use common::*;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run `dapper --run PROGRAM` with `script` on stdin; returns stdout.
fn dapper(wasmtime: &Path, program: &Path, wasmtime_args: &[&str], script: &str) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dapper"));
    cmd.arg("--wasmtime").arg(wasmtime);
    for a in wasmtime_args {
        cmd.arg(format!("--wasmtime-arg={a}"));
    }
    cmd.arg("--run")
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("--- transcript ---\n{stdout}");
    eprintln!("--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
    stdout
}

/// Check that `expected` lines occur, in order, as substrings of lines
/// of `output`.
fn check(output: &str, expected: &str) {
    let mut lines = output.lines();
    for want in expected.lines().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        if !lines.any(|l| l.contains(want)) {
            panic!("did not find `{want}` (in order) in transcript:\n{output}");
        }
    }
}

#[test]
fn c_values_and_stepping() {
    let (Some(wt), Some(prog)) = (wasmtime(), build_c("debug_tui_c")) else {
        return;
    };
    let out = dapper(
        &wt,
        &prog,
        &[],
        "b debug_tui_c.c:60\nc\nbt\np tri\np tri.points[1].x\np *tri.next\np %1.next->name\n\
         p tri.tag\nptype tri\ninfo locals\ns\ninfo args\np s->points[2]\nup\np r\ndown\n\
         finish\nn\np r\nx/5dw &arr\nx/s heap\nlist\ninfo breakpoints\ndisas\nsi\nwasm-stack\n\
         delete 1\nc\n",
    );
    check(
        &out,
        r#"
        [0] debug_tui_c.wasm (Symbols loaded)
        (dapper) b debug_tui_c.c:60
        Breakpoint 1 at pc 0x3c7 in main at debug_tui_c.c:60
        Breakpoint 1, main () at debug_tui_c.c:60
        60	  int r = area(&tri);
        *#0   0x0003c7 in main () at debug_tui_c.c:60
         #1   0x0001c5 in _start () at crt1-command.c:56
        %1 = struct Shape { .name = 0x1003b "triangle", .color = GREEN, .points = {{ .x = 0, .y = 0 }, { .x = 4, .y = 0 }, { .x = 0, .y = 3 }}, .next = 0xff78, .tag = "tri", .scale = 1.5, .flags = 5 }
        %2 = 4
        %3 = struct Shape { .name = 0x1001d "other", .color = BLUE,
        %4 = 0x1001d "other"
        %5 = "tri"
        type = struct Shape {
            unsigned int flags : 3;
        tri = struct Shape
        arr = {10, 20, 30, 40, 50}
        (dapper) s
        area (s=(struct Shape *) 0xffb8) at debug_tui_c.c:33
        33	  int a = s->points[1].x - s->points[0].x;
        s = (struct Shape *) 0xffb8
        %6 = struct Point { .x = 0, .y = 3 }
        #1  main () at debug_tui_c.c:60
        %7 = 0
        #0  area (s=(struct Shape *) 0xffb8) at debug_tui_c.c:33
        Run till exit from #0  area (s=(struct Shape *) 0xffb8) at debug_tui_c.c:33
        main () at debug_tui_c.c:60
        Value returned is %8 = 6
        61	  printf(
        %9 = 6
        0x0000ff50: 10 20 30 40
        0x0000ff60: 50
        0x111a0: "on the heap"
        (dapper) list
        56	  char *heap = malloc(16);
        63	}
        1    breakpoint    1     debug_tui_c.c:60
        Dump of Wasm code for function main:
        => 0x0003d8: local.get 0
        End of dump.
        (dapper) si
        => 0x3da: i32.load offset=28
        (dapper) wasm-stack
        stack[0]: i32 65328  <- top
        [Program exited]
        "#,
    );
}

#[test]
fn rust_values() {
    let (Some(wt), Some(prog)) = (wasmtime(), build_rust("debug_tui_rust")) else {
        return;
    };
    let out = dapper(
        &wt,
        &prog,
        &[],
        "start\nb describe\nc\nbt\ninfo args\np *shape\nup\np shapes\np list.next.value\n\
         p map\nptype shapes[0]\ndown\nfinish\nc\n",
    );
    check(
        &out,
        r#"
        Temporary breakpoint 1, debug_tui_rust::main () at debug_tui_rust.rs:39
        Breakpoint 2 at pc
        Breakpoint 2, debug_tui_rust::describe (shape=&Shape::Rect { w: 3, h: 4 }, color=Color::Blue, point=&Point { x: 1, y: 2 }) at debug_tui_rust.rs:34
        #1
        [inlined]
        shape = &Shape::Rect { w: 3, h: 4 }
        %1 = Shape::Rect { w: 3, h: 4 }
        #1  debug_tui_rust::main () at debug_tui_rust.rs:66
        %2 = vec![Shape::Circle(1.5), Shape::Rect { w: 3, h: 4 }, Shape::Empty]
        %3 = 2
        %4 = HashMap(len=2) {
        type = enum debug_tui_rust::Shape {
            Rect { w: u32, h: u32 },
        Run till exit from #0  debug_tui_rust::describe
        debug_tui_rust::main () at debug_tui_rust.rs:
        [Program exited]
        "#,
    );
}

#[test]
fn wasm_level_and_gc() {
    let Some(wt) = wasmtime() else { return };
    let out = dapper(
        &wt,
        &programs().join("gc.wat"),
        &["-Wgc=y,function-references=y"],
        "b use\nc\nwasm-locals\np %local0\np $p.2.1\nsi\nsi\nwasm-stack\nni\nni\nni\nni\nni\n\
         ni\nni\nwasm-table\np $t[1][3]\nwasm-memory 16 16\nx/4xb 16\nfinish\nc\n",
    );
    check(
        &out,
        r#"
        [0] gcdemo (No symbols)
        Breakpoint 1, use (p=struct (3 fields), a=array (len 4), i=i31 -42) in gcdemo at pc
        local 0 (p): struct (3 fields) { 0: i32 1, 1: f64 2.5, 2: struct (3 fields) }
        local 2 (i): i31 -42
        %1 = struct (3 fields) { 0: i32 1, 1: f64 2.5, 2: struct (3 fields) { 0: i32 3, 1: f64 4.5, 2: (ref.null any) } }
        %2 = f64 4.5
        stack[0]: i32 65
        stack[1]: i32 3  <- top
        [1] array (len 4) [i32 7, i32 7, i32 7, i32 7]
        %3 = i32 7
        0x00000010: 68 65 6c 6c 6f 2c 20 6d 65 6d 6f 72 79 21 00 00  |hello, memory!..|
        0x00000010: 0x68 0x65 0x6c 0x6c
        Run till exit from #0  use
        _start () in gcdemo at pc
        => 
        Wasm results: i32 8
        [Program exited]
        "#,
    );
}

#[test]
fn interrupt() {
    let Some(wt) = wasmtime() else { return };
    let mut child = Command::new(env!("CARGO_BIN_EXE_dapper"))
        .arg("--wasmtime")
        .arg(&wt)
        .arg("--run")
        .arg(programs().join("spin.wat"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"c\n").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    // Ctrl-C pauses the program.
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));
    stdin.write_all(b"bt\nquit\n").unwrap();
    drop(stdin);
    // Detaching leaves the program spinning; stop Wasmtime by killing
    // our process group's wasmtime child via the debugger exiting.
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("{stdout}");
    check(
        &stdout,
        r#"
        (dapper) c
        Program interrupted.
        spin () in
        (dapper) bt
        in spin ()
        "#,
    );
}
