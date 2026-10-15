//! `dapper`: a command-line and terminal-UI client for the Debug Adapter
//! Protocol, built for Wasmtime's debugger (`wasmtime run --debug-dap`).

mod core;
mod tui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wasmtime_debug_dap::Client;

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

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Start `wasmtime run --debug-dap` and wait until it is listening.
/// Returns the child and the address to connect to.
fn spawn_wasmtime(opts: &Options, program: &[String], capture_stdout: bool) -> Result<(Child, String)> {
    let addr = format!("127.0.0.1:{}", free_port()?);
    let mut cmd = Command::new(&opts.wasmtime);
    cmd.arg("run")
        .arg(format!("--debug-dap={addr}"))
        .args(&opts.wasmtime_args)
        .args(program)
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    if capture_stdout {
        cmd.stdout(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run `{}`", opts.wasmtime))?;
    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            let status = child.wait()?;
            bail!("wasmtime exited ({status}) before the debugger started");
        }
        if line.contains("Debugger listening on") {
            break;
        }
        if !line.starts_with("Debugger:") {
            eprint!("{line}");
        }
    }
    // Forward the rest of Wasmtime's stderr (e.g. traps).
    std::thread::spawn(move || {
        let mut rest = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    rest.push_str(&String::from_utf8_lossy(&buf[..n]));
                    while let Some(i) = rest.find('\n') {
                        let l: String = rest.drain(..=i).collect();
                        if !l.starts_with("Debugger:") {
                            eprint!("{l}");
                        }
                    }
                }
            }
        }
    });
    Ok((child, addr))
}

fn connect(addr: &str) -> Result<Client> {
    let addr = if addr.parse::<u16>().is_ok() {
        format!("127.0.0.1:{addr}")
    } else {
        addr.to_string()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match Client::connect(addr.as_str()) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn main() -> Result<()> {
    let opts = Options::parse();
    let (mut child, addr) = match (&opts.run, &opts.address) {
        (Some(program), _) => {
            let (c, a) = spawn_wasmtime(&opts, program, opts.tui)?;
            (Some(c), a)
        }
        (None, Some(a)) => (None, a.clone()),
        (None, None) => bail!("give a DAP server address, or `--run PROGRAM.wasm`"),
    };
    let client = connect(&addr).with_context(|| format!("failed to connect to {addr}"))?;
    let mut d = core::Dapper::connect(client)?;

    let result = if opts.tui {
        let stdout = child.as_mut().and_then(|c| c.stdout.take());
        tui::run(&mut d, stdout, opts.commands.as_deref())
    } else {
        line_mode(&mut d, opts.commands.as_deref())
    };
    d.disconnect();
    if let Some(mut c) = child {
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
