//! `dapper`: a command-line and terminal-UI client for the Debug Adapter
//! Protocol, built for Wasmtime's debugger (`wasmtime run --debug-dap`).

use dapper::{core, tui};

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::cell::RefCell;
use std::io::{BufRead, IsTerminal, Write};
use std::process::Child;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A debugger client for Wasmtime's Debug Adapter Protocol server.
///
/// Either connect to a running `wasmtime run --debug-dap ADDR:PORT`, use
/// `--run` to start Wasmtime on a program and connect to it, or use
/// `--replay` to debug a replay of a recorded execution (which can also
/// run backward).
#[derive(Parser)]
#[command(version)]
struct Options {
    /// Address of a DAP server (`PORT` or `ADDR:PORT`).
    #[arg(conflicts_with_all = ["run", "replay"])]
    address: Option<String>,

    /// Start Wasmtime on this program (with any following arguments) and
    /// debug it.
    #[arg(long, num_args = 1.., value_name = "WASM [ARGS]...", allow_hyphen_values = true)]
    run: Option<Vec<String>>,

    /// Start Wasmtime on a replay of this trace, recorded with `wasmtime
    /// run --record=TRACE ...`, and debug it.
    #[arg(long, value_name = "TRACE", conflicts_with = "run")]
    replay: Option<String>,

    /// The Wasmtime executable to use with `--run` or `--replay`.
    #[arg(long, default_value = "wasmtime")]
    wasmtime: String,

    /// Extra options for `wasmtime run` or `wasmtime replay` (with `--run`
    /// or `--replay`), e.g. `--wasmtime-arg=-Wgc=y`.
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
    if opts.run.is_none() && opts.replay.is_none() && opts.address.is_none() {
        bail!("give a DAP server address, `--run PROGRAM.wasm`, or `--replay TRACE`");
    }
    // Start Wasmtime (with `--run` or `--replay`).
    let spawn = {
        let (run, replay) = (opts.run.clone(), opts.replay.clone());
        let (wasmtime, args, tui) = (opts.wasmtime.clone(), opts.wasmtime_args.clone(), opts.tui);
        move || match (&run, &replay) {
            (Some(program), _) => dapper::spawn_wasmtime(&wasmtime, &args, program, tui),
            (None, Some(trace)) => dapper::spawn_wasmtime_replay(&wasmtime, &args, trace, tui),
            (None, None) => unreachable!(),
        }
    };
    let child: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
    let addr = match &opts.address {
        Some(a) => a.clone(),
        None => {
            let (c, a) = spawn()?;
            *child.borrow_mut() = Some(c);
            a
        }
    };
    let client = dapper::connect(&addr).with_context(|| format!("failed to connect to {addr}"))?;
    let mut d = core::Dapper::connect(client)?;
    if opts.address.is_none() {
        // After the program exits, `start` or `run` starts it again.
        let child = child.clone();
        d.restart = Some(Box::new(move || {
            if let Some(mut c) = child.borrow_mut().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            let (mut c, addr) = spawn()?;
            let stdout = c.stdout.take();
            *child.borrow_mut() = Some(c);
            let client =
                dapper::connect(&addr).with_context(|| format!("failed to connect to {addr}"))?;
            Ok((client, stdout))
        }));
    }

    let result = if opts.tui {
        let stdout = child.borrow_mut().as_mut().and_then(|c| c.stdout.take());
        tui::run(&mut d, stdout, opts.commands.as_deref())
    } else {
        line_mode(&mut d, opts.commands.as_deref())
    };
    let running = d.state != core::ProgramState::Exited;
    d.disconnect();
    if let Some(mut c) = child.borrow_mut().take() {
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
