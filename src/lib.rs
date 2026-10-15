//! `dapper`: a command-line and terminal-UI client for the Debug Adapter
//! Protocol, built for Wasmtime's debugger (`wasmtime run --debug-dap`).

pub mod core;
pub mod tui;

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use wasmtime_debug_dap::Client;

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Start `wasmtime run --debug-dap` and wait until it is listening.
/// Returns the child and the address to connect to.
pub fn spawn_wasmtime(
    wasmtime: &str,
    wasmtime_args: &[String],
    program: &[String],
    capture_stdout: bool,
) -> Result<(Child, String)> {
    let addr = format!("127.0.0.1:{}", free_port()?);
    let mut cmd = Command::new(wasmtime);
    cmd.arg("run")
        .arg(format!("--debug-dap={addr}"))
        .args(wasmtime_args)
        .args(program)
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    if capture_stdout {
        cmd.stdout(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run `{wasmtime}`"))?;
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

pub fn connect(addr: &str) -> Result<Client> {
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
