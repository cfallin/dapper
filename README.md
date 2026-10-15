# dapper

A command-line and terminal-UI client for the [Debug Adapter Protocol]
(DAP), built for Wasmtime's built-in debugger (`wasmtime run
--debug-dap`).

dapper speaks the same gdb-style command language as Wasmtime's
text-mode debugger (`wasmtime run --debug-tui`), but over DAP: it uses
standard requests for breakpoints, execution control, stack traces,
variables, memory and disassembly, and reads source files locally.
Commands that need the debugger's own state (the value history,
`ptype`, `location`, Wasm-level inspection such as `wasm-table`, ...)
are run on the server via DAP's `evaluate` request.

## Usage

```console
# Start Wasmtime on a program and debug it:
$ dapper --run prog.wasm [ARGS...]

# ... with extra Wasmtime options and a specific Wasmtime binary:
$ dapper --wasmtime path/to/wasmtime --wasmtime-arg=-Wgc=y --run prog.wasm

# Or connect to an already-running server
# (`wasmtime run --debug-dap 1234 prog.wasm`):
$ dapper 1234

# The full-screen terminal UI:
$ dapper --tui --run prog.wasm
```

Wasmtime must be built with the `debug-tui` feature
(`cargo build --features debug-tui`).

Type `help` for the command list. When stdin is not a terminal, commands
are read from it and echoed after the prompt, producing a transcript;
`-x FILE` runs a command file first.

## Terminal UI

`--tui` shows source, stack, variables (an expandable tree of source
locals and arguments, Wasm locals, operand stack and globals, plus watch
expressions), disassembly, memory, and a console.

| Key             | Action                                         |
|-----------------|------------------------------------------------|
| F5              | continue                                       |
| F10 / F11       | next / step                                    |
| Shift-F11       | finish                                         |
| F7 / F8         | stepi / nexti                                  |
| F9              | toggle a breakpoint at the source cursor       |
| Tab / Shift-Tab | move focus between panes                       |
| Esc             | focus the console                              |
| arrows, Enter   | navigate / expand within the focused pane      |
| Ctrl-C, Esc     | pause the running program                      |
| Ctrl-D          | quit                                           |

In addition to the usual commands, the console accepts `mem EXPR` (show
memory at an address or at a value's location, e.g. `mem &buf`), and
`watch EXPR` / `unwatch [N]`.

## Testing

```console
$ WASMTIME=../wasmtime/target/debug/wasmtime WASI_SDK_PATH=/opt/wasi-sdk cargo test
```

Tests that need a missing tool are skipped with a message.

[Debug Adapter Protocol]: https://microsoft.github.io/debug-adapter-protocol/
