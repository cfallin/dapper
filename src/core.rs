//! The debugger front-end core: gdb-style commands implemented with DAP
//! requests. Output is accumulated in [`Dapper::out`] and drained by the
//! line-mode or terminal-UI front-end.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;
use wasmtime_debug_dap::types::{Breakpoint, DisassembledInstruction, StackFrame, Variable};
use wasmtime_debug_dap::{
    Client, base64_decode, code_address, memory_reference, parse_memory_reference,
    split_code_address,
};
use wasmtime_debug_info::cli::{
    BreakpointSpec, ExamineFormat, HELP, basename, format_examine_string, parse_u64,
};
use wasmtime_debug_info::path;

/// A user breakpoint (numbered by the client, like gdb).
#[derive(Clone, Debug)]
pub struct UserBp {
    pub id: u32,
    pub spec: String,
    pub kind: BpKind,
    pub temporary: bool,
    pub hits: u32,
    /// The server's id for this breakpoint, and its description.
    pub server: Option<Breakpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BpKind {
    /// A source line (the source path as given by the user or the server).
    Line {
        path: String,
        line: u32,
    },
    Function(String),
    /// A code address (module, pc).
    Instruction(u64),
    /// A watchpoint (DAP data breakpoint), by the server's `dataId`.
    Data(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    NotStarted,
    Stopped,
    Exited,
}

/// How to resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Continue,
    Next,
    Step,
    NextI,
    StepI,
    Finish,
    ReverseContinue,
    ReverseNext,
    ReverseStep,
    ReverseNextI,
    ReverseStepI,
    ReverseFinish,
}

impl Resume {
    /// The command of the text interface for this.
    pub fn command(self) -> &'static str {
        match self {
            Resume::Continue => "continue",
            Resume::Next => "next",
            Resume::Step => "step",
            Resume::NextI => "nexti",
            Resume::StepI => "stepi",
            Resume::Finish => "finish",
            Resume::ReverseContinue => "reverse-continue",
            Resume::ReverseNext => "reverse-next",
            Resume::ReverseStep => "reverse-step",
            Resume::ReverseNextI => "reverse-nexti",
            Resume::ReverseStepI => "reverse-stepi",
            Resume::ReverseFinish => "reverse-finish",
        }
    }

    pub fn is_reverse(self) -> bool {
        matches!(
            self,
            Resume::ReverseContinue
                | Resume::ReverseNext
                | Resume::ReverseStep
                | Resume::ReverseNextI
                | Resume::ReverseStepI
                | Resume::ReverseFinish
        )
    }
}

/// The message for reverse execution of a program that is not a replay.
const NOT_REVERSIBLE: &str = "Reverse execution requires a recorded execution: record one with \
     `wasmtime run --record=TRACE ...`, then debug its replay (`dapper --replay TRACE`).";

/// Result of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

pub struct Dapper {
    pub client: Client,
    /// Output not yet shown to the user.
    pub out: String,
    pub state: ProgramState,
    /// Whether the program can run backward (it is a replay).
    pub reversible: bool,
    /// Stack frames (with parameters) as of the last stop.
    pub frames: Vec<StackFrame>,
    pub selected: usize,
    pub breakpoints: Vec<UserBp>,
    next_bp: u32,
    /// The next source line to show for `list`: (path, line).
    pub list_pos: Option<(String, u32)>,
    source_cache: HashMap<String, Option<Vec<String>>>,
    module_names: HashMap<i64, String>,
    /// Polled while the program runs; returns true to request a pause.
    pub interrupt: Box<dyn FnMut() -> bool>,
    /// Called with program output events while running.
    pub on_output: Box<dyn FnMut(&str)>,
}

fn frame_ip(f: &StackFrame) -> Option<(usize, u32)> {
    f.instruction_pointer_reference
        .as_deref()
        .and_then(parse_memory_reference)
        .map(split_code_address)
}

impl Dapper {
    /// Connect and perform the DAP initialization handshake, stopping on
    /// entry.
    pub fn connect(mut client: Client) -> Result<Dapper> {
        let caps = client.request(
            "initialize",
            json!({
                "clientID": "dapper",
                "clientName": "dapper",
                "adapterID": "wasmtime",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsMemoryReferences": true,
            }),
        )?;
        client.request("launch", json!({ "stopOnEntry": true }))?;
        client.request("configurationDone", json!({}))?;
        let mut d = Dapper {
            client,
            out: String::new(),
            state: ProgramState::NotStarted,
            reversible: caps["supportsStepBack"].as_bool() == Some(true),
            frames: vec![],
            selected: 0,
            breakpoints: vec![],
            next_bp: 1,
            list_pos: None,
            source_cache: HashMap::new(),
            module_names: HashMap::new(),
            interrupt: Box::new(|| false),
            on_output: Box::new(|_| {}),
        };
        let mut out = String::new();
        d.client.wait_event("stopped", &mut |e| {
            if e["event"] == "output" {
                out.push_str(e["body"]["output"].as_str().unwrap_or(""));
            }
        })?;
        d.out.push_str(&out);
        if let Ok(body) = d.client.request("modules", json!({})) {
            for m in body["modules"].as_array().cloned().unwrap_or_default() {
                if let (Some(id), Some(name)) = (m["id"].as_i64(), m["name"].as_str()) {
                    d.module_names.insert(id, name.to_string());
                }
            }
        }
        Ok(d)
    }

    fn println(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }

    /// The banner shown on connection.
    pub fn banner(&mut self) {
        let mut out =
            String::from("dapper: Debug Adapter Protocol client for Wasmtime.\nModules:\n");
        let modules = self
            .client
            .request("modules", json!({}))
            .unwrap_or(json!({}));
        for m in modules["modules"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(
                out,
                "  [{}] {} ({})",
                m["id"],
                m["name"].as_str().unwrap_or("?"),
                m["symbolStatus"].as_str().unwrap_or("")
            );
        }
        out.push_str(
            "The program has not started. Type `start` to run to `main`, `continue` to run,\n\
             or `help` for a list of commands.\n",
        );
        self.out.push_str(&out);
    }

    /// Execute one command line.
    pub fn execute(&mut self, line: &str) -> Flow {
        let line = line.trim();
        if line.is_empty() {
            return Flow::Continue;
        }
        let (cmd, args) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        let (cmd, fmt) = cmd.split_once('/').unwrap_or((cmd, ""));
        let r = match cmd {
            "help" | "h" => {
                self.out.push_str(HELP);
                Ok(())
            }
            "quit" | "q" | "detach" => return Flow::Quit,
            "start" => self.cmd_start(),
            "continue" | "c" | "run" | "r" => self.resume(Resume::Continue),
            "next" | "n" => self.resume(Resume::Next),
            "step" | "s" => self.resume(Resume::Step),
            "nexti" | "ni" => self.resume(Resume::NextI),
            "stepi" | "si" => self.resume(Resume::StepI),
            "finish" | "fin" => self.resume(Resume::Finish),
            "reverse-continue" | "rc" => self.resume(Resume::ReverseContinue),
            "reverse-next" | "rn" => self.resume(Resume::ReverseNext),
            "reverse-step" | "rs" => self.resume(Resume::ReverseStep),
            "reverse-nexti" | "rni" => self.resume(Resume::ReverseNextI),
            "reverse-stepi" | "rsi" => self.resume(Resume::ReverseStepI),
            "reverse-finish" => self.resume(Resume::ReverseFinish),
            "watch" => self.cmd_watch(args),
            "interrupt" => {
                self.println("The program is not running.");
                Ok(())
            }
            "break" | "b" | "br" => self.cmd_break(args, false),
            "tbreak" => self.cmd_break(args, true),
            "delete" | "d" => self.cmd_delete(args),
            "backtrace" | "bt" | "where" => self.cmd_backtrace(),
            "frame" | "f" => self.cmd_frame(args),
            "up" => self.cmd_up_down(args, true),
            "down" => self.cmd_up_down(args, false),
            "list" | "l" => self.cmd_list(args),
            "x" => self.cmd_examine(args, fmt),
            "info" | "i"
                if matches!(
                    args.split_whitespace().next(),
                    Some("breakpoints" | "break" | "b")
                ) =>
            {
                self.info_breakpoints();
                Ok(())
            }
            "info" | "i" if matches!(args.split_whitespace().next(), Some("locals" | "args")) => {
                self.info_vars(args.starts_with("args"))
            }
            "wasm-locals" => self.wasm_scope("Wasm Locals"),
            "wasm-stack" => self.wasm_scope("Wasm Operand Stack"),
            "disassemble" | "disas" if args.is_empty() => self.cmd_disas(),
            "print" | "p" | "output" => self.cmd_print(line, args),
            _ => self.repl(line),
        };
        if let Err(e) = r {
            self.println(&format!("{e:#}"));
        }
        Flow::Continue
    }

    // -----------------------------------------------------------------
    // Helpers.

    fn require_stopped(&self) -> Result<()> {
        match self.state {
            ProgramState::Stopped if !self.frames.is_empty() => Ok(()),
            ProgramState::NotStarted => bail!("The program has not been started."),
            ProgramState::Exited => bail!("The program is not being run."),
            _ => bail!("No stack."),
        }
    }

    fn frame_id(&self) -> Option<i64> {
        self.frames.get(self.selected).map(|f| f.id)
    }

    /// Run a command on the server's REPL (used for commands that need
    /// server-side state, such as the value history).
    fn repl(&mut self, line: &str) -> Result<()> {
        let mut args = json!({ "expression": line, "context": "repl" });
        if let Some(id) = self.frame_id() {
            args["frameId"] = json!(id);
        }
        let body = self.client.request("evaluate", args)?;
        let text = body["result"].as_str().unwrap_or("");
        if !text.is_empty() {
            self.println(text);
        }
        Ok(())
    }

    fn cmd_print(&mut self, line: &str, args: &str) -> Result<()> {
        if args.is_empty() {
            bail!("Argument required (expression to print).");
        }
        // Check the access-path syntax locally for prompt feedback.
        path::parse(args)?;
        self.repl(line)
    }

    fn source_lines(&mut self, path: &str) -> Option<Vec<String>> {
        if !self.source_cache.contains_key(path) {
            let lines = std::fs::read_to_string(path)
                .ok()
                .or_else(|| {
                    // Ask the debugger (which may have access via
                    // `-Ddebug-fs`).
                    self.client
                        .request("source", json!({ "source": { "path": path } }))
                        .ok()
                        .and_then(|b| b["content"].as_str().map(|s| s.to_string()))
                })
                .map(|s| s.lines().map(|l| l.to_string()).collect());
            self.source_cache.insert(path.to_string(), lines);
        }
        self.source_cache.get(path).unwrap().clone()
    }

    /// Source text of `path`, if available (for front-ends).
    pub fn source_text(&mut self, path: &str) -> Option<Vec<String>> {
        self.source_lines(path)
    }

    fn print_source_line(&mut self, path: &str, line: u32) {
        let text = self
            .source_lines(path)
            .and_then(|ls| ls.get((line as usize).checked_sub(1)?).cloned());
        match text {
            Some(t) => self.println(&format!("{line}\t{t}")),
            None => self.println(&format!("{line}\tin {path} (source unavailable)")),
        }
    }

    fn frame_path(f: &StackFrame) -> Option<String> {
        f.source.as_ref().and_then(|s| s.path.clone())
    }

    /// `name (args) at file:line` / `name (args) in module at pc 0x..`.
    fn frame_desc(&self, f: &StackFrame) -> String {
        match &f.source {
            Some(src) => format!(
                "{} at {}:{}",
                f.name,
                basename(src.path.as_deref().or(src.name.as_deref()).unwrap_or("??")),
                f.line
            ),
            None => {
                let (module, pc) = frame_ip(f).unwrap_or((0, 0));
                let label = self
                    .module_names
                    .get(&(module as i64))
                    .cloned()
                    .unwrap_or_else(|| format!("module{module}"));
                format!("{} in {label} at pc {pc:#x}", f.name)
            }
        }
    }

    /// Show the current line of frame `i`: its source line, or its
    /// instruction if there is no source.
    fn print_current_line(&mut self, i: usize, with_instruction: bool) {
        let f = self.frames[i].clone();
        if let Some(path) = Self::frame_path(&f) {
            self.list_pos = Some((path.clone(), (f.line as u32).saturating_sub(5).max(1)));
            self.print_source_line(&path, f.line as u32);
            if !with_instruction {
                return;
            }
        }
        if let Some(ip) = f.instruction_pointer_reference.clone() {
            let (_, pc) = frame_ip(&f).unwrap_or((0, 0));
            match self.disassemble(&ip, 0, 1) {
                Ok(instrs)
                    if instrs.first().is_some_and(|i| {
                        parse_memory_reference(&i.address).map(|a| split_code_address(a).1)
                            == Some(pc)
                    }) =>
                {
                    let text = instrs[0].instruction.trim_start().to_string();
                    self.println(&format!("=> {pc:#x}: {text}"));
                }
                _ => self.println(&format!("=> {pc:#x}")),
            }
        }
    }

    fn refresh_frames(&mut self) -> Result<()> {
        let body = self.client.request(
            "stackTrace",
            json!({
                "threadId": 1,
                "format": { "parameters": true, "parameterValues": true },
            }),
        )?;
        self.frames = serde_json::from_value(body["stackFrames"].clone())?;
        self.selected = 0;
        Ok(())
    }

    fn disassemble(
        &mut self,
        ip: &str,
        offset: i64,
        count: i64,
    ) -> Result<Vec<DisassembledInstruction>> {
        let body = self.client.request(
            "disassemble",
            json!({
                "memoryReference": ip,
                "instructionOffset": offset,
                "instructionCount": count,
                "resolveSymbols": true,
            }),
        )?;
        Ok(serde_json::from_value(body["instructions"].clone())?)
    }

    // -----------------------------------------------------------------
    // Execution.

    fn cmd_start(&mut self) -> Result<()> {
        if self.state != ProgramState::NotStarted {
            bail!("The program is already running.");
        }
        self.add_breakpoint("main", true)?;
        self.resume(Resume::Continue)
    }

    /// Resume execution and report how it stopped.
    pub fn resume(&mut self, how: Resume) -> Result<()> {
        if self.state == ProgramState::Exited {
            bail!("The program is not being run.");
        }
        if how.is_reverse() && !self.reversible {
            bail!("{NOT_REVERSIBLE}");
        }
        if self.state == ProgramState::NotStarted
            && matches!(
                how,
                Resume::Next
                    | Resume::Step
                    | Resume::Finish
                    | Resume::ReverseNext
                    | Resume::ReverseStep
                    | Resume::ReverseFinish
            )
        {
            bail!("The program has not been started; use `start` or `continue`.");
        }
        let before = self
            .frames
            .get(self.selected)
            .map(|f| (f.name.clone(), self.frames.len() - self.selected));
        if how == Resume::Finish {
            self.require_stopped()?;
            let desc = self.frame_desc(&self.frames[self.selected].clone());
            self.println(&format!("Run till exit from #{}  {desc}", self.selected));
        }
        if how == Resume::ReverseFinish {
            self.require_stopped()?;
            let desc = self.frame_desc(&self.frames[self.selected].clone());
            self.println(&format!("Run back to call of #{}  {desc}", self.selected));
        }
        // Reverse steps other than DAP's `stepBack` (the reverse of `next`)
        // are run as commands of the server's debug console, which then
        // reports the stop as usual.
        let repl = |command: &str, frame: Option<i64>| {
            let mut a = json!({ "expression": command, "context": "repl" });
            if let Some(id) = frame {
                a["frameId"] = json!(id);
            }
            ("evaluate", a)
        };
        let (command, args) = match how {
            Resume::Continue => ("continue", json!({ "threadId": 1 })),
            Resume::Next => ("next", json!({ "threadId": 1 })),
            Resume::Step => ("stepIn", json!({ "threadId": 1 })),
            Resume::NextI => (
                "next",
                json!({ "threadId": 1, "granularity": "instruction" }),
            ),
            Resume::StepI => (
                "stepIn",
                json!({ "threadId": 1, "granularity": "instruction" }),
            ),
            Resume::Finish => ("stepOut", json!({ "threadId": 1 })),
            Resume::ReverseContinue => ("reverseContinue", json!({ "threadId": 1 })),
            Resume::ReverseNext => ("stepBack", json!({ "threadId": 1 })),
            Resume::ReverseNextI => (
                "stepBack",
                json!({ "threadId": 1, "granularity": "instruction" }),
            ),
            Resume::ReverseStep | Resume::ReverseStepI | Resume::ReverseFinish => {
                repl(how.command(), self.frame_id())
            }
        };
        // `finish` applies to the selected frame: select it on the server.
        if how == Resume::Finish {
            let len = self.out.len();
            self.repl(&format!("frame {}", self.selected))?;
            self.out.truncate(len);
        }
        self.client.request(command, args)?;
        if self.state == ProgramState::NotStarted {
            self.state = ProgramState::Stopped;
        }
        // Wait for the program to stop, relaying output, and polling for
        // interrupt requests.
        let mut paused = false;
        // Console output (e.g. a returned value) is shown after the stop
        // report, as the text interface does.
        let mut console = String::new();
        let (event, body) = loop {
            match self.client.next_event(Some(Duration::from_millis(50)))? {
                Some(e) => {
                    let name = e["event"].as_str().unwrap_or("").to_string();
                    let body = e.get("body").cloned().unwrap_or(Value::Null);
                    match name.as_str() {
                        "stopped" | "terminated" => break (name, body),
                        "output" => {
                            let text = body["output"].as_str().unwrap_or("").to_string();
                            if body["category"].as_str() == Some("console") {
                                console.push_str(&text);
                            } else {
                                (self.on_output)(&text);
                            }
                        }
                        _ => {}
                    }
                }
                None => {
                    if !paused && (self.interrupt)() {
                        self.client.request("pause", json!({ "threadId": 1 }))?;
                        paused = true;
                    }
                }
            }
        };
        if event == "terminated" {
            self.state = ProgramState::Exited;
            self.frames.clear();
            self.out.push_str(&console);
            self.println("[Program exited]");
            return Ok(());
        }
        let r = self.report(&body, before, how, &mut console);
        self.out.push_str(&console);
        r
    }

    fn report(
        &mut self,
        stopped: &Value,
        before: Option<(String, usize)>,
        how: Resume,
        console: &mut String,
    ) -> Result<()> {
        self.refresh_frames()?;
        let reason = stopped["reason"].as_str().unwrap_or("");
        match reason {
            // The server describes these stops in the console: show that
            // first (with our watchpoint numbers).
            "data breakpoint" => {
                let hit = Self::hit_ids(stopped);
                let mut text = String::from("\n");
                for line in std::mem::take(console).lines() {
                    text.push_str(&self.renumber_watchpoint(line, &hit));
                    text.push('\n');
                }
                self.out.push_str(&text);
            }
            "beginning of recording" | "end of recording" => {
                self.out.push('\n');
                self.out.push_str(&std::mem::take(console));
                if self.frames.is_empty() {
                    if reason == "beginning of recording" {
                        // Before any Wasm ran, as if the program had not
                        // started.
                        self.state = ProgramState::NotStarted;
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
        if self.frames.is_empty() {
            self.println("Stopped (no Wasm frames on the stack).");
            return Ok(());
        }
        let desc0 = self.frame_desc(&self.frames[0].clone());
        let same_frame = before
            .as_ref()
            .is_some_and(|(n, depth)| *n == self.frames[0].name && *depth == self.frames.len());
        match reason {
            "data breakpoint" | "beginning of recording" | "end of recording" => {
                self.println(&desc0);
                self.print_current_line(0, false);
            }
            "breakpoint" => {
                let hit = Self::hit_ids(stopped);
                let bp = self.breakpoints.iter_mut().find(|b| {
                    b.server
                        .as_ref()
                        .and_then(|s| s.id)
                        .is_some_and(|id| hit.contains(&id))
                });
                match bp {
                    Some(bp) => {
                        bp.hits += 1;
                        let (id, temporary) = (bp.id, bp.temporary);
                        if temporary {
                            self.delete_breakpoint(id)?;
                            self.println(&format!("\nTemporary breakpoint {id}, {desc0}"));
                        } else {
                            self.println(&format!("\nBreakpoint {id}, {desc0}"));
                        }
                    }
                    None => self.println(&format!("\nStopped at {desc0}")),
                }
                self.print_current_line(0, false);
            }
            "pause" => {
                self.println(&format!("\nProgram interrupted.\n{desc0}"));
                self.print_current_line(0, false);
            }
            "exception" => {
                let d = stopped["description"].as_str().unwrap_or("Exception");
                self.println(&format!("\n{d}.\n{desc0}"));
                self.print_current_line(0, false);
            }
            _ => {
                if matches!(how, Resume::Finish | Resume::ReverseFinish) {
                    self.println(&desc0);
                    self.print_current_line(0, false);
                } else {
                    if !same_frame {
                        self.println(&desc0);
                    }
                    let instr = matches!(
                        how,
                        Resume::StepI | Resume::NextI | Resume::ReverseStepI | Resume::ReverseNextI
                    );
                    self.print_current_line(0, instr);
                }
            }
        }
        Ok(())
    }

    fn hit_ids(stopped: &Value) -> Vec<i64> {
        stopped["hitBreakpointIds"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default()
    }

    /// In the server's description of a watchpoint stop, replace its
    /// watchpoint number with ours (counting the hit).
    fn renumber_watchpoint(&mut self, line: &str, hit: &[i64]) -> String {
        let Some(rest) = line.strip_prefix("Watchpoint ") else {
            return line.to_string();
        };
        let Some((num, rest)) = rest.split_once(':') else {
            return line.to_string();
        };
        let Ok(server_id) = num.parse::<i64>() else {
            return line.to_string();
        };
        let bp = self.breakpoints.iter_mut().find(|b| {
            matches!(b.kind, BpKind::Data(_))
                && b.server.as_ref().and_then(|s| s.id) == Some(server_id)
                && hit.contains(&server_id)
        });
        match bp {
            Some(b) => {
                b.hits += 1;
                format!("Watchpoint {}:{rest}", b.id)
            }
            None => line.to_string(),
        }
    }

    // -----------------------------------------------------------------
    // Breakpoints.

    fn current_path(&self) -> Option<String> {
        if let Some(f) = self.frames.get(self.selected)
            && let Some(p) = Self::frame_path(f)
        {
            return Some(p);
        }
        self.list_pos.as_ref().map(|(p, _)| p.clone())
    }

    fn add_breakpoint(&mut self, spec: &str, temporary: bool) -> Result<u32> {
        let kind = match BreakpointSpec::parse(spec)? {
            BreakpointSpec::Line { file, line } => {
                let path = match file {
                    Some(f) => f,
                    None => self
                        .current_path()
                        .ok_or_else(|| anyhow!("No default source file; use FILE:LINE."))?,
                };
                BpKind::Line { path, line }
            }
            BreakpointSpec::Function(name) => BpKind::Function(name),
            BreakpointSpec::Pc { module, pc } => {
                let module = module
                    .or_else(|| {
                        self.frames
                            .get(self.selected)
                            .and_then(frame_ip)
                            .map(|(m, _)| m)
                    })
                    .unwrap_or(0);
                BpKind::Instruction(code_address(module, pc))
            }
        };
        let id = self.next_bp;
        self.breakpoints.push(UserBp {
            id,
            spec: spec.to_string(),
            kind: kind.clone(),
            temporary,
            hits: 0,
            server: None,
        });
        if let Err(e) = self.sync_breakpoints(&kind) {
            self.breakpoints.pop();
            return Err(e);
        }
        let bp = self.breakpoints.last().unwrap();
        match &bp.server {
            Some(s) if s.verified => {
                self.next_bp += 1;
                Ok(id)
            }
            other => {
                let msg = other
                    .as_ref()
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| "cannot set breakpoint".into());
                self.breakpoints.pop();
                self.sync_breakpoints(&kind)?;
                bail!("{msg}")
            }
        }
    }

    /// Send the full set of breakpoints of the same class (same source
    /// file, functions, or instructions) as `kind` to the server.
    fn sync_breakpoints(&mut self, kind: &BpKind) -> Result<()> {
        let same: Vec<usize> = self
            .breakpoints
            .iter()
            .enumerate()
            .filter(|(_, b)| match (&b.kind, kind) {
                (BpKind::Line { path: a, .. }, BpKind::Line { path: b, .. }) => a == b,
                (BpKind::Function(_), BpKind::Function(_)) => true,
                (BpKind::Instruction(_), BpKind::Instruction(_)) => true,
                (BpKind::Data(_), BpKind::Data(_)) => true,
                _ => false,
            })
            .map(|(i, _)| i)
            .collect();
        let body = match kind {
            BpKind::Line { path, .. } => {
                let lines: Vec<Value> = same
                    .iter()
                    .map(|&i| match &self.breakpoints[i].kind {
                        BpKind::Line { line, .. } => json!({ "line": line }),
                        _ => unreachable!(),
                    })
                    .collect();
                self.client.request(
                    "setBreakpoints",
                    json!({ "source": { "path": path }, "breakpoints": lines }),
                )?
            }
            BpKind::Function(_) => {
                let names: Vec<Value> = same
                    .iter()
                    .map(|&i| match &self.breakpoints[i].kind {
                        BpKind::Function(n) => json!({ "name": n }),
                        _ => unreachable!(),
                    })
                    .collect();
                self.client
                    .request("setFunctionBreakpoints", json!({ "breakpoints": names }))?
            }
            BpKind::Instruction(_) => {
                let refs: Vec<Value> = same
                    .iter()
                    .map(|&i| match &self.breakpoints[i].kind {
                        BpKind::Instruction(a) => {
                            json!({ "instructionReference": memory_reference(*a) })
                        }
                        _ => unreachable!(),
                    })
                    .collect();
                self.client
                    .request("setInstructionBreakpoints", json!({ "breakpoints": refs }))?
            }
            BpKind::Data(_) => {
                let ids: Vec<Value> = same
                    .iter()
                    .map(|&i| match &self.breakpoints[i].kind {
                        BpKind::Data(id) => json!({ "dataId": id, "accessType": "write" }),
                        _ => unreachable!(),
                    })
                    .collect();
                self.client
                    .request("setDataBreakpoints", json!({ "breakpoints": ids }))?
            }
        };
        let results: Vec<Breakpoint> = serde_json::from_value(body["breakpoints"].clone())?;
        for (&i, r) in same.iter().zip(results) {
            self.breakpoints[i].server = Some(r);
        }
        Ok(())
    }

    fn cmd_break(&mut self, args: &str, temporary: bool) -> Result<()> {
        let spec = if args.is_empty() {
            let f = self
                .frames
                .get(self.selected)
                .ok_or_else(|| anyhow!("No default breakpoint location."))?;
            let (m, pc) = frame_ip(f).ok_or_else(|| anyhow!("No default breakpoint location."))?;
            format!("*{m}:{pc:#x}")
        } else {
            args.to_string()
        };
        let id = self.add_breakpoint(&spec, temporary)?;
        let msg = self
            .breakpoints
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.server.as_ref())
            .and_then(|s| s.message.clone())
            .unwrap_or_default();
        let kind = if temporary {
            "Temporary breakpoint"
        } else {
            "Breakpoint"
        };
        self.println(&format!("{kind} {id} at {msg}"));
        Ok(())
    }

    fn cmd_watch(&mut self, args: &str) -> Result<()> {
        if args.is_empty() {
            bail!("Argument required (expression to watch, or `-m ADDR LEN`).");
        }
        let info = if let Some(rest) = args.strip_prefix("-m ") {
            let mut parts = rest.split_whitespace();
            let (Some(addr), Some(len), None) = (parts.next(), parts.next(), parts.next()) else {
                bail!("Usage: watch -m ADDR LEN");
            };
            let addr = match parse_u64(addr) {
                Ok(a) => a,
                Err(_) => self.address_of(addr)?,
            };
            self.client.request(
                "dataBreakpointInfo",
                json!({
                    "name": memory_reference(addr),
                    "asAddress": true,
                    "bytes": parse_u64(len)?,
                }),
            )?
        } else {
            path::parse(args)?;
            let mut a = json!({ "name": args });
            if let Some(id) = self.frame_id() {
                a["frameId"] = json!(id);
            }
            self.client.request("dataBreakpointInfo", a)?
        };
        let id = self.add_watchpoint(args, &info)?;
        self.report_new_watchpoint(id);
        Ok(())
    }

    /// Watch a variable shown in the variables tree: member `name` of
    /// `variables_reference`.
    pub fn watch_variable(&mut self, variables_reference: i64, name: &str) -> Result<u32> {
        let info = self.client.request(
            "dataBreakpointInfo",
            json!({ "variablesReference": variables_reference, "name": name }),
        )?;
        let spec = info["description"].as_str().unwrap_or(name).to_string();
        let id = self.add_watchpoint(&spec, &info)?;
        self.report_new_watchpoint(id);
        Ok(id)
    }

    /// Add a watchpoint from a `dataBreakpointInfo` response.
    fn add_watchpoint(&mut self, spec: &str, info: &Value) -> Result<u32> {
        let Some(data_id) = info["dataId"].as_str() else {
            bail!(
                "{}",
                info["description"]
                    .as_str()
                    .unwrap_or("This cannot be watched.")
            );
        };
        let kind = BpKind::Data(data_id.to_string());
        let id = self.next_bp;
        self.breakpoints.push(UserBp {
            id,
            spec: spec.to_string(),
            kind: kind.clone(),
            temporary: false,
            hits: 0,
            server: None,
        });
        if let Err(e) = self.sync_breakpoints(&kind) {
            self.breakpoints.pop();
            return Err(e);
        }
        let bp = self.breakpoints.last().unwrap();
        match &bp.server {
            Some(s) if s.verified => {
                self.next_bp += 1;
                Ok(id)
            }
            other => {
                let msg = other
                    .as_ref()
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| "cannot set watchpoint".into());
                self.breakpoints.pop();
                self.sync_breakpoints(&kind)?;
                bail!("{msg}")
            }
        }
    }

    fn report_new_watchpoint(&mut self, id: u32) {
        let msg = self
            .breakpoints
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.server.as_ref())
            .and_then(|s| s.message.clone())
            .unwrap_or_default();
        self.println(&format!("Watchpoint {id}: {msg}"));
    }

    fn delete_breakpoint(&mut self, id: u32) -> Result<bool> {
        let Some(i) = self.breakpoints.iter().position(|b| b.id == id) else {
            return Ok(false);
        };
        let bp = self.breakpoints.remove(i);
        self.sync_breakpoints(&bp.kind)?;
        Ok(true)
    }

    fn cmd_delete(&mut self, args: &str) -> Result<()> {
        let args = args.strip_prefix("breakpoints").unwrap_or(args).trim();
        if args.is_empty() {
            let kinds: Vec<BpKind> = self.breakpoints.drain(..).map(|b| b.kind).collect();
            for k in kinds {
                self.sync_breakpoints(&k)?;
            }
            self.println("Deleted all breakpoints.");
            return Ok(());
        }
        for a in args.split_whitespace() {
            let id: u32 = a
                .parse()
                .map_err(|_| anyhow!("Bad breakpoint number '{a}'"))?;
            if !self.delete_breakpoint(id)? {
                self.println(&format!("No breakpoint number {id}."));
            }
        }
        Ok(())
    }

    fn info_breakpoints(&mut self) {
        if self.breakpoints.is_empty() {
            self.println("No breakpoints or watchpoints.");
            return;
        }
        let mut out = String::from("Num  Type          Hits  Where\n");
        for b in &self.breakpoints {
            let kind = match (&b.kind, b.temporary) {
                (BpKind::Data(_), _) => "watchpoint",
                (_, true) => "tbreakpoint",
                (_, false) => "breakpoint",
            };
            let _ = writeln!(out, "{:<4} {:<13} {:<5} {}", b.id, kind, b.hits, b.spec);
            if let Some(m) = b.server.as_ref().and_then(|s| s.message.as_deref()) {
                // For watchpoints on values, show just where the value is.
                let m = m
                    .strip_prefix(&format!("{} (", b.spec))
                    .and_then(|m| m.strip_suffix(')'))
                    .unwrap_or(m);
                let _ = writeln!(out, "                         {m}");
            }
        }
        self.out.push_str(&out);
    }

    // -----------------------------------------------------------------
    // Stack.

    fn cmd_backtrace(&mut self) -> Result<()> {
        self.require_stopped()?;
        let mut out = String::new();
        for (i, f) in self.frames.iter().enumerate() {
            let marker = if i == self.selected { "*" } else { " " };
            // Inlined frames share their caller's instruction pointer.
            let inlined = self.frames.get(i + 1).is_some_and(|n| {
                n.instruction_pointer_reference == f.instruction_pointer_reference
            });
            let pcs = if inlined {
                "[inlined]".to_string()
            } else {
                format!("{:#08x}", frame_ip(f).map(|(_, pc)| pc).unwrap_or(0))
            };
            let _ = writeln!(out, "{marker}#{i:<3} {pcs} in {}", self.frame_desc(f));
        }
        self.out.push_str(&out);
        Ok(())
    }

    /// Select frame `i` (on the client, and on the server's REPL).
    pub fn select_frame(&mut self, i: usize) -> Result<()> {
        if i >= self.frames.len() {
            bail!("No frame at level {i}.");
        }
        self.selected = i;
        let desc = self.frame_desc(&self.frames[i].clone());
        self.println(&format!("#{i}  {desc}"));
        self.print_current_line(i, false);
        Ok(())
    }

    fn cmd_frame(&mut self, args: &str) -> Result<()> {
        self.require_stopped()?;
        let i = if args.is_empty() {
            self.selected
        } else {
            parse_u64(args)? as usize
        };
        self.select_frame(i)
    }

    fn cmd_up_down(&mut self, args: &str, up: bool) -> Result<()> {
        self.require_stopped()?;
        let n = if args.is_empty() {
            1
        } else {
            parse_u64(args)? as usize
        };
        let i = if up {
            let i = self.selected + n;
            if i >= self.frames.len() {
                bail!("Initial frame selected; you cannot go up.");
            }
            i
        } else {
            self.selected
                .checked_sub(n)
                .ok_or_else(|| anyhow!("Bottom (innermost) frame selected; you cannot go down."))?
        };
        self.select_frame(i)
    }

    // -----------------------------------------------------------------
    // Source.

    fn cmd_list(&mut self, args: &str) -> Result<()> {
        let (path, start) = if args.is_empty() {
            self.list_pos
                .clone()
                .ok_or_else(|| anyhow!("No default source file; use `list FILE:LINE`."))?
        } else {
            match BreakpointSpec::parse(args)? {
                BreakpointSpec::Line { file, line } => {
                    let path = match file {
                        Some(f) => self.resolve_source_path(&f),
                        None => self
                            .current_path()
                            .ok_or_else(|| anyhow!("No default source file; use FILE:LINE."))?,
                    };
                    (path, line.saturating_sub(5).max(1))
                }
                _ => {
                    // Let the server resolve other locations.
                    return self.repl(&format!("list {args}"));
                }
            }
        };
        let lines = self
            .source_lines(&path)
            .ok_or_else(|| anyhow!("Cannot read source file {path}."))?;
        let mut last = start;
        for n in start..start + 10 {
            let Some(text) = lines.get(n as usize - 1) else {
                break;
            };
            self.println(&format!("{n}\t{text}"));
            last = n;
        }
        self.list_pos = Some((path, last + 1));
        Ok(())
    }

    /// Resolve a user-given file name against known source paths.
    fn resolve_source_path(&self, f: &str) -> String {
        for fr in &self.frames {
            if let Some(p) = Self::frame_path(fr)
                && (p == f || p.ends_with(&format!("/{f}")))
            {
                return p;
            }
        }
        for b in &self.breakpoints {
            if let Some(p) = b
                .server
                .as_ref()
                .and_then(|s| s.source.as_ref())
                .and_then(|s| s.path.clone())
                && (p == f || p.ends_with(&format!("/{f}")))
            {
                return p;
            }
        }
        f.to_string()
    }

    // -----------------------------------------------------------------
    // Data.

    fn scope_variables(&mut self, scope: &str) -> Result<Option<Vec<Variable>>> {
        let id = self.frame_id().ok_or_else(|| anyhow!("No stack."))?;
        let body = self.client.request("scopes", json!({ "frameId": id }))?;
        let Some(r) = body["scopes"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["name"] == scope))
            .and_then(|s| s["variablesReference"].as_i64())
        else {
            return Ok(None);
        };
        let body = self
            .client
            .request("variables", json!({ "variablesReference": r }))?;
        Ok(Some(serde_json::from_value(body["variables"].clone())?))
    }

    fn info_vars(&mut self, args: bool) -> Result<()> {
        self.require_stopped()?;
        let scope = if args { "Arguments" } else { "Locals" };
        match self.scope_variables(scope)? {
            None => self.println("No symbol table info available."),
            Some(vars) if vars.is_empty() => {
                self.println(if args { "No arguments." } else { "No locals." })
            }
            Some(vars) => {
                for v in vars {
                    self.println(&format!("{} = {}", v.name, v.value));
                }
            }
        }
        Ok(())
    }

    fn wasm_scope(&mut self, scope: &str) -> Result<()> {
        self.require_stopped()?;
        let vars = self.scope_variables(scope)?.unwrap_or_default();
        let stack = scope == "Wasm Operand Stack";
        if vars.is_empty() {
            self.println(if stack {
                "Operand stack is empty."
            } else {
                "No locals."
            });
        }
        let n = vars.len();
        for (i, v) in vars.into_iter().enumerate() {
            let top = if stack && i + 1 == n { "  <- top" } else { "" };
            self.println(&format!("{}: {}{top}", v.name, v.value));
        }
        Ok(())
    }

    fn cmd_examine(&mut self, args: &str, fmt: &str) -> Result<()> {
        let ef = ExamineFormat::parse(fmt)?;
        let addr = match parse_u64(args) {
            Ok(a) => a,
            Err(_) => {
                let mut a = json!({ "expression": args, "context": "watch" });
                if let Some(id) = self.frame_id() {
                    a["frameId"] = json!(id);
                }
                let body = self.client.request("evaluate", a)?;
                match body["memoryReference"]
                    .as_str()
                    .and_then(parse_memory_reference)
                {
                    Some(m) => m,
                    None => {
                        // A plain integer value used as an address.
                        let r = body["result"].as_str().unwrap_or("");
                        let num = r.split_whitespace().last().unwrap_or("");
                        parse_u64(num).map_err(|_| anyhow!("Value has no address."))?
                    }
                }
            }
        };
        let read = |c: &mut Client, addr: u64, n: u64| -> Result<Vec<u8>> {
            let body = c.request(
                "readMemory",
                json!({ "memoryReference": memory_reference(addr), "count": n }),
            )?;
            let data = base64_decode(body["data"].as_str().unwrap_or(""))
                .ok_or_else(|| anyhow!("invalid memory data"))?;
            if (data.len() as u64) < n {
                bail!(
                    "cannot access memory at address {:#x}",
                    addr + data.len() as u64
                );
            }
            Ok(data)
        };
        if ef.format == 's' {
            let mut a = addr;
            for _ in 0..ef.count {
                let start = a;
                let mut bytes = vec![];
                'outer: loop {
                    let chunk =
                        read(&mut self.client, a, 64).or_else(|_| read(&mut self.client, a, 1))?;
                    for b in chunk {
                        a += 1;
                        if b == 0 || bytes.len() >= 1024 {
                            break 'outer;
                        }
                        bytes.push(b);
                    }
                }
                a = start + bytes.len() as u64 + 1;
                self.out.push_str(&format_examine_string(start, &bytes));
            }
            return Ok(());
        }
        let bytes = read(&mut self.client, addr, ef.byte_len())?;
        self.out.push_str(&ef.format(addr, &bytes));
        Ok(())
    }

    fn cmd_disas(&mut self) -> Result<()> {
        self.require_stopped()?;
        let f = self.frames[self.selected].clone();
        let ip = f
            .instruction_pointer_reference
            .clone()
            .ok_or_else(|| anyhow!("No function at pc."))?;
        let cur = frame_ip(&f).map(|(_, pc)| pc);
        let instrs = self.disassemble(&ip, -10, 25)?;
        // Restrict to the current function: from the last symbol at or
        // before the PC to the next symbol after it.
        let here = instrs
            .iter()
            .position(|i| {
                parse_memory_reference(&i.address).map(|a| split_code_address(a).1) == cur
            })
            .unwrap_or(10);
        let start = instrs[..=here.min(instrs.len().saturating_sub(1))]
            .iter()
            .rposition(|i| i.symbol.is_some());
        let end = instrs[here + 1..]
            .iter()
            .position(|i| i.symbol.is_some() || i.presentation_hint.as_deref() == Some("invalid"))
            .map(|p| here + 1 + p)
            .unwrap_or(instrs.len());
        let name = start
            .and_then(|s| instrs[s].symbol.clone())
            .unwrap_or_else(|| f.name.split(" (").next().unwrap_or("?").to_string());
        let mut out = format!("Dump of Wasm code for function {name}:\n");
        let begin = start.unwrap_or(0);
        if start.is_none() {
            out.push_str("   ...\n");
        }
        for i in &instrs[begin..end] {
            if i.presentation_hint.as_deref() == Some("invalid") {
                continue;
            }
            let pc = parse_memory_reference(&i.address)
                .map(|a| split_code_address(a).1)
                .unwrap_or(0);
            let marker = if Some(pc) == cur { "=>" } else { "  " };
            let _ = writeln!(out, "{marker} {pc:#08x}: {}", i.instruction);
        }
        if end == instrs.len() {
            out.push_str("   ...\n");
        }
        out.push_str("End of dump.\n");
        self.out.push_str(&out);
        Ok(())
    }

    /// Expand a variables reference (for front-ends that show trees).
    pub fn variables(&mut self, r: i64) -> Result<Vec<Variable>> {
        let body = self
            .client
            .request("variables", json!({ "variablesReference": r }))?;
        Ok(serde_json::from_value(body["variables"].clone())?)
    }

    /// Scopes of the selected frame: (name, variables reference).
    pub fn scopes(&mut self) -> Result<Vec<(String, i64)>> {
        let Some(id) = self.frame_id() else {
            return Ok(vec![]);
        };
        let body = self.client.request("scopes", json!({ "frameId": id }))?;
        Ok(body["scopes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap_or("").to_string(),
                    s["variablesReference"].as_i64().unwrap_or(0),
                )
            })
            .collect())
    }

    /// Read memory (for front-ends that show a memory view).
    pub fn read_memory(&mut self, addr: u64, n: u64) -> Result<Vec<u8>> {
        let body = self.client.request(
            "readMemory",
            json!({ "memoryReference": memory_reference(addr), "count": n }),
        )?;
        base64_decode(body["data"].as_str().unwrap_or("")).ok_or_else(|| anyhow!("bad data"))
    }

    /// Disassembly around the selected frame's PC (for front-ends).
    pub fn disassembly_around_pc(
        &mut self,
        before: i64,
        count: i64,
    ) -> Result<Vec<DisassembledInstruction>> {
        let Some(ip) = self
            .frames
            .get(self.selected)
            .and_then(|f| f.instruction_pointer_reference.clone())
        else {
            return Ok(vec![]);
        };
        self.disassemble(&ip, -before, count)
    }

    /// The memory address an expression denotes (for front-ends).
    pub fn address_of(&mut self, expr: &str) -> Result<u64> {
        if let Ok(a) = parse_u64(expr) {
            return Ok(a);
        }
        let mut a = json!({ "expression": expr, "context": "watch" });
        if let Some(id) = self.frame_id() {
            a["frameId"] = json!(id);
        }
        let body = self.client.request("evaluate", a)?;
        body["memoryReference"]
            .as_str()
            .and_then(parse_memory_reference)
            .ok_or_else(|| anyhow!("`{expr}` has no memory address"))
    }

    /// Evaluate a watch expression: (value, variables reference).
    pub fn evaluate_watch(&mut self, expr: &str) -> Result<(String, i64)> {
        let mut a = json!({ "expression": expr, "context": "watch" });
        if let Some(id) = self.frame_id() {
            a["frameId"] = json!(id);
        }
        let body = self.client.request("evaluate", a)?;
        Ok((
            body["result"].as_str().unwrap_or("").to_string(),
            body["variablesReference"].as_i64().unwrap_or(0),
        ))
    }

    /// Disconnect from the server (the program runs to completion).
    pub fn disconnect(&mut self) {
        let _ = self.client.request("disconnect", json!({}));
    }
}
