//! `dapper`: a command-line and terminal-UI client for the Debug Adapter
//! Protocol, built for Wasmtime's debugger (`wasmtime run --debug-dap`).

use dapper::{core, tui};

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A debugger client for Wasmtime's Debug Adapter Protocol server.
///
/// Either connect to a running `wasmtime run --debug-dap ADDR:PORT`, or
/// use `--run` to start Wasmtime on a program and connect to it.
#[derive(Parser)]
#[command(version)]
struct Options {
    /// Address of a DAP server (`PORT` or `ADDR:PORT`).
    #[arg(conflicts_with = "run")]
    address: Option<String>,

    /// Start Wasmtime on this program (with any following arguments) and
    /// debug it.
    #[arg(long, num_args = 1.., value_name = "WASM [ARGS]...", allow_hyphen_values = true)]
    run: Option<Vec<String>>,

    /// The Wasmtime executable to use with `--run`.
    #[arg(long, default_value = "wasmtime")]
    wasmtime: String,

    /// Extra options for `wasmtime run` (with `--run`), e.g.
    /// `--wasmtime-arg=-Wgc=y`.
    #[arg(long = "wasmtime-arg", value_name = "ARG", allow_hyphen_values = true)]
    wasmtime_args: Vec<String>,

    /// Use the full-screen terminal UI.
    #[arg(long)]
    tui: bool,

    /// Run commands from this file, then continue interactively (or exit
    /// at end of input if stdin is not a terminal).
    #[arg(short = 'x', long = "command", value_name = "FILE")]
    commands: Option<String>,
}

fn main() -> Result<()> {
    let opts = Options::parse();
    let (mut child, addr) = match (&opts.run, &opts.address) {
        (Some(program), _) => {
            let (c, a) =
                dapper::spawn_wasmtime(&opts.wasmtime, &opts.wasmtime_args, program, opts.tui)?;
            (Some(c), a)
        }
        (None, Some(a)) => (None, a.clone()),
        (None, None) => bail!("give a DAP server address, or `--run PROGRAM.wasm`"),
    };
    let client = dapper::connect(&addr).with_context(|| format!("failed to connect to {addr}"))?;
    let mut d = core::Dapper::connect(client)?;

    let result = if opts.tui {
        let stdout = child.as_mut().and_then(|c| c.stdout.take());
        tui::run(&mut d, stdout, opts.commands.as_deref())
    } else {
        line_mode(&mut d, opts.commands.as_deref())
    };
    let running = d.state != core::ProgramState::Exited;
    d.disconnect();
    if let Some(mut c) = child {
        // Like gdb, quitting kills a program that we started.
        if running {
            let _ = c.kill();
        }
        let _ = c.wait();
    }
    result
}

/// The line-oriented interface. When stdin is not a terminal (e.g. a
/// script), commands are echoed after the prompt, making transcripts.
fn line_mode(d: &mut core::Dapper, script: Option<&str>) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let i = interrupted.clone();
        ctrlc::set_handler(move || i.store(true, Ordering::SeqCst))?;
    }
    let i = interrupted.clone();
    d.interrupt = Box::new(move || i.swap(false, Ordering::SeqCst));
    d.on_output = Box::new(|s| {
        print!("{s}");
        let _ = std::io::stdout().flush();
    });
    d.banner();
    let flush = |d: &mut core::Dapper| {
        print!("{}", std::mem::take(&mut d.out));
        let _ = std::io::stdout().flush();
    };
    flush(d);
    const PROMPT: &str = "(dapper) ";

    if let Some(path) = script {
        let text = std::fs::read_to_string(path)?;
        for line in text.lines() {
            println!("{PROMPT}{line}");
            let flow = d.execute(line);
            flush(d);
            if flow == core::Flow::Quit {
                return Ok(());
            }
        }
    }

    if std::io::stdin().is_terminal() {
        let mut rl = rustyline::DefaultEditor::new()?;
        loop {
            match rl.readline(PROMPT) {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = rl.add_history_entry(line.as_str());
                    }
                    interrupted.store(false, Ordering::SeqCst);
                    let flow = d.execute(&line);
                    flush(d);
                    if flow == core::Flow::Quit {
                        break;
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(_) => break,
            }
        }
    } else {
        for line in std::io::stdin().lock().lines() {
            let line = line?;
            println!("{PROMPT}{line}");
            let flow = d.execute(&line);
            flush(d);
            if flow == core::Flow::Quit {
                break;
            }
        }
    }
    Ok(())
}
