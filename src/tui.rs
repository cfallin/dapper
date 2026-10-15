//! The full-screen terminal UI.
//!
//! ```text
//! ┌ Source ─────────────────────────┬ Stack ───────────────┐
//! │  59    global_counter++;        │ *#0 main () :60      │
//! │● 60 ►  int r = area(&tri);      │  #1 _start () :56    │
//! │                                 ├ Variables ───────────┤
//! │                                 │ ▾ Locals             │
//! ├ Disassembly ────┬ Memory ───────┤   ▸ tri = struct ... │
//! │ => 0x3c7 ...    │ 0xffb8: 3b .. │                      │
//! ├ Console ────────┴───────────────┴──────────────────────┤
//! │ (dapper) p tri                                         │
//! │ > _                                                    │
//! └────────────────────────────────────────────────────────┘
//! ```
//!
//! The console accepts every `dapper` command, plus `mem EXPR` (set the
//! memory view's address) and `display EXPR` / `undisplay [N]`
//! (expressions shown in the variables pane). In the variables pane, `w`
//! sets a watchpoint on the selected variable.

use crate::core::{BpKind, Dapper, Flow, ProgramState, Resume};
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::ChildStdout;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;
use wasmtime_debug_dap::{parse_memory_reference, split_code_address};
use wasmtime_debug_info::cli::{basename, demangle_wat};

/// Which pane has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Console,
    Source,
    Stack,
    Variables,
    Disassembly,
    Memory,
}

impl Focus {
    fn next(self) -> Focus {
        match self {
            Focus::Console => Focus::Source,
            Focus::Source => Focus::Stack,
            Focus::Stack => Focus::Variables,
            Focus::Variables => Focus::Disassembly,
            Focus::Disassembly => Focus::Memory,
            Focus::Memory => Focus::Console,
        }
    }
}

/// A row of the variables tree.
#[derive(Clone, Debug)]
struct TreeRow {
    key: String,
    depth: usize,
    text: String,
    reference: i64,
    /// The variable's name and its parent's `variablesReference`, for
    /// variables (not scopes or displayed expressions).
    var: Option<(i64, String)>,
}

/// A console line, with its kind (for styling).
#[derive(Clone, Debug)]
enum ConsoleLine {
    Command(String),
    Output(String),
    Program(String),
}

pub struct App {
    pub focus: Focus,
    console: Vec<ConsoleLine>,
    /// Lines scrolled back from the bottom of the console.
    console_scroll: usize,
    input: String,
    history: Vec<String>,
    history_pos: Option<usize>,
    /// The file shown in the source pane, its lines, and the cursor line
    /// (1-based).
    source_path: Option<String>,
    source: Vec<String>,
    source_cursor: u32,
    /// The current line (in `source_path`), if stopped there.
    current_line: Option<u32>,
    stack_cursor: usize,
    tree: Vec<TreeRow>,
    tree_cursor: usize,
    expanded: HashSet<String>,
    displays: Vec<String>,
    disasm: Vec<(u32, String, bool)>,
    disasm_scroll: usize,
    mem_expr: Option<String>,
    mem_addr: u64,
    mem: Vec<u8>,
    status: String,
    pub quit: bool,
    program_output: Option<Receiver<String>>,
}

impl App {
    pub fn new(program_stdout: Option<ChildStdout>) -> App {
        let program_output = program_stdout.map(|out| {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            rx
        });
        let mut expanded = HashSet::new();
        expanded.insert("Arguments".to_string());
        expanded.insert("Locals".to_string());
        expanded.insert("Display".to_string());
        App {
            focus: Focus::Console,
            console: vec![],
            console_scroll: 0,
            input: String::new(),
            history: vec![],
            history_pos: None,
            source_path: None,
            source: vec![],
            source_cursor: 1,
            current_line: None,
            stack_cursor: 0,
            tree: vec![],
            tree_cursor: 0,
            expanded,
            displays: vec![],
            disasm: vec![],
            disasm_scroll: 0,
            mem_expr: None,
            mem_addr: 0,
            mem: vec![],
            status: String::new(),
            quit: false,
            program_output,
        }
    }

    fn push_output(&mut self, text: &str) {
        for l in text.trim_end_matches('\n').split('\n') {
            if !(l.is_empty() && text.is_empty()) {
                self.console.push(ConsoleLine::Output(l.to_string()));
            }
        }
        self.console_scroll = 0;
    }

    /// Move debugger output into the console.
    fn drain(&mut self, d: &mut Dapper) {
        let out = std::mem::take(&mut d.out);
        if !out.is_empty() {
            self.push_output(&out);
        }
        if let Some(rx) = &self.program_output {
            let lines: Vec<String> = rx.try_iter().collect();
            for l in lines {
                self.console.push(ConsoleLine::Program(l));
            }
        }
    }

    /// Run a console command.
    pub fn command(&mut self, d: &mut Dapper, line: &str) {
        self.console.push(ConsoleLine::Command(line.to_string()));
        if !line.trim().is_empty() && self.history.last().map(|s| s.as_str()) != Some(line) {
            self.history.push(line.to_string());
        }
        self.history_pos = None;
        let (cmd, rest) = line
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((line.trim(), ""));
        let rest = rest.trim();
        match cmd {
            "mem" | "memory" => {
                match d.address_of(rest) {
                    Ok(a) => {
                        self.mem_expr = Some(rest.to_string());
                        self.mem_addr = a;
                    }
                    Err(e) => self.push_output(&format!("{e:#}")),
                }
                self.refresh(d);
                return;
            }
            "display" if !rest.is_empty() => {
                self.displays.push(rest.to_string());
                self.refresh(d);
                return;
            }
            "undisplay" => {
                match rest.parse::<usize>() {
                    Ok(n) if n < self.displays.len() => {
                        self.displays.remove(n);
                    }
                    _ => self.displays.clear(),
                }
                self.refresh(d);
                return;
            }
            _ => {}
        }
        let before = (d.state, d.selected, d.frames.first().map(|f| f.id));
        let flow = self.run_debugger(d, |d| d.execute(line));
        if flow == Flow::Quit {
            self.quit = true;
            return;
        }
        let after = (d.state, d.selected, d.frames.first().map(|f| f.id));
        if before != after
            || matches!(
                cmd,
                "frame" | "f" | "up" | "down" | "b" | "break" | "tbreak" | "delete" | "d" | "watch"
            )
        {
            self.stack_cursor = d.selected;
        }
        self.refresh(d);
    }

    fn run_debugger<T>(&mut self, d: &mut Dapper, f: impl FnOnce(&mut Dapper) -> T) -> T {
        self.status = "Running… (Ctrl-C to pause)".into();
        let r = f(d);
        self.status.clear();
        self.drain(d);
        r
    }

    fn resume(&mut self, d: &mut Dapper, how: Resume) {
        self.command(d, how.command());
    }

    /// Set a watchpoint on the variable selected in the variables pane.
    fn watch_selected(&mut self, d: &mut Dapper) {
        let Some((parent, name)) = self.tree.get(self.tree_cursor).and_then(|r| r.var.clone())
        else {
            self.push_output("Select a variable to watch.");
            return;
        };
        let r = self.run_debugger(d, |d| d.watch_variable(parent, &name));
        if let Err(e) = r {
            self.push_output(&format!("{e:#}"));
        }
        self.drain(d);
    }

    /// Refresh all panes from the debugger's state.
    pub fn refresh(&mut self, d: &mut Dapper) {
        self.drain(d);
        let frame = d.frames.get(d.selected).cloned();
        // Source.
        let path = frame
            .as_ref()
            .and_then(|f| f.source.as_ref())
            .and_then(|s| s.path.clone())
            .or_else(|| d.list_pos.as_ref().map(|(p, _)| p.clone()));
        if path != self.source_path {
            self.source = path
                .as_deref()
                .and_then(|p| d.source_text(p))
                .unwrap_or_default();
            self.source_path = path;
        }
        self.current_line = frame
            .as_ref()
            .filter(|f| f.source.is_some())
            .map(|f| f.line as u32);
        if let Some(l) = self.current_line {
            self.source_cursor = l;
        }
        // Variables.
        self.rebuild_tree(d);
        // Disassembly.
        self.disasm.clear();
        self.disasm_scroll = 0;
        if d.state == ProgramState::Stopped {
            let cur = frame
                .as_ref()
                .and_then(|f| f.instruction_pointer_reference.as_deref())
                .and_then(parse_memory_reference);
            if let Ok(instrs) = d.disassembly_around_pc(12, 40) {
                for i in instrs {
                    if i.presentation_hint.as_deref() == Some("invalid") {
                        continue;
                    }
                    let a = parse_memory_reference(&i.address);
                    let pc = a.map(|a| split_code_address(a).1).unwrap_or(0);
                    let mut text = i.instruction.clone();
                    if let Some(sym) = &i.symbol {
                        text = format!("{text}    <{sym}>");
                    }
                    self.disasm.push((pc, demangle_wat(&text), a == cur));
                }
            }
        }
        // Memory.
        if let Some(e) = self.mem_expr.clone()
            && let Ok(a) = d.address_of(&e)
        {
            self.mem_addr = a;
        }
        self.mem = if d.state == ProgramState::Stopped || d.state == ProgramState::NotStarted {
            d.read_memory(self.mem_addr, 256).unwrap_or_default()
        } else {
            vec![]
        };
        self.drain(d);
    }

    fn rebuild_tree(&mut self, d: &mut Dapper) {
        self.tree.clear();
        if d.state == ProgramState::Exited {
            return;
        }
        // Displayed expressions.
        if !self.displays.is_empty() {
            self.tree.push(TreeRow {
                key: "Display".into(),
                depth: 0,
                text: "Display".into(),
                reference: -1,
                var: None,
            });
            if self.expanded.contains("Display") {
                for (i, w) in self.displays.clone().iter().enumerate() {
                    let (text, reference) = match d.evaluate_watch(w) {
                        Ok((v, r)) => (format!("{w} = {v}"), r),
                        Err(e) => (format!("{w}: {e}"), 0),
                    };
                    let key = format!("Display/{i}");
                    self.tree.push(TreeRow {
                        key: key.clone(),
                        depth: 1,
                        text,
                        reference,
                        var: None,
                    });
                    self.expand_children(d, &key, reference, 2);
                }
            }
        }
        if d.state != ProgramState::Stopped {
            return;
        }
        for (name, reference) in d.scopes().unwrap_or_default() {
            self.tree.push(TreeRow {
                key: name.clone(),
                depth: 0,
                text: name.clone(),
                reference,
                var: None,
            });
            self.expand_children(d, &name, reference, 1);
        }
        self.tree_cursor = self.tree_cursor.min(self.tree.len().saturating_sub(1));
    }

    fn expand_children(&mut self, d: &mut Dapper, key: &str, reference: i64, depth: usize) {
        if reference <= 0 || !self.expanded.contains(key) || depth > 12 {
            return;
        }
        for v in d
            .variables(reference)
            .unwrap_or_default()
            .into_iter()
            .take(500)
        {
            let k = format!("{key}/{}", v.name);
            let text = if v.name.starts_with("local ")
                || v.name.starts_with("stack[")
                || v.name.starts_with("global ")
            {
                format!("{}: {}", v.name, v.value)
            } else {
                format!("{} = {}", v.name, v.value)
            };
            self.tree.push(TreeRow {
                key: k.clone(),
                depth,
                text,
                reference: v.variables_reference,
                var: Some((reference, v.name.clone())),
            });
            self.expand_children(d, &k, v.variables_reference, depth + 1);
        }
    }

    fn toggle_breakpoint(&mut self, d: &mut Dapper) {
        let Some(path) = self.source_path.clone() else {
            return;
        };
        let line = self.source_cursor;
        let existing = d.breakpoints.iter().find(|b| {
            matches!(&b.kind, BpKind::Line { path: p, line: l } if *l == line && (*p == path || path.ends_with(&format!("/{p}"))))
                || b.server.as_ref().is_some_and(|s| {
                    s.line == Some(i64::from(line))
                        && s.source.as_ref().and_then(|s| s.path.as_deref()) == Some(path.as_str())
                })
        });
        let cmd = match existing {
            Some(b) => format!("delete {}", b.id),
            None => format!("break {path}:{line}"),
        };
        self.command(d, &cmd);
    }

    fn line_has_breakpoint(&self, d: &Dapper, line: u32) -> bool {
        let Some(path) = &self.source_path else {
            return false;
        };
        d.breakpoints.iter().any(|b| {
            b.server.as_ref().is_some_and(|s| {
                s.line == Some(i64::from(line))
                    && s.source.as_ref().and_then(|s| s.path.as_deref()) == Some(path.as_str())
            })
        })
    }

    /// Handle a key press.
    pub fn key(&mut self, d: &mut Dapper, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        // Global keys.
        match k.code {
            KeyCode::F(5) if shift => return self.resume(d, Resume::ReverseContinue),
            KeyCode::F(5) => return self.resume(d, Resume::Continue),
            KeyCode::F(10) if shift => return self.resume(d, Resume::ReverseNext),
            KeyCode::F(10) => return self.resume(d, Resume::Next),
            KeyCode::F(11) if shift => return self.resume(d, Resume::Finish),
            KeyCode::F(11) => return self.resume(d, Resume::Step),
            KeyCode::F(7) if shift => return self.resume(d, Resume::ReverseStepI),
            KeyCode::F(7) => return self.resume(d, Resume::StepI),
            KeyCode::F(8) if shift => return self.resume(d, Resume::ReverseNextI),
            KeyCode::F(8) => return self.resume(d, Resume::NextI),
            KeyCode::F(9) => return self.toggle_breakpoint(d),
            KeyCode::Tab => {
                self.focus = self.focus.next();
                return;
            }
            KeyCode::BackTab => {
                for _ in 0..5 {
                    self.focus = self.focus.next();
                }
                return;
            }
            KeyCode::Esc => {
                self.focus = Focus::Console;
                return;
            }
            KeyCode::Char('d') if ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::Char('l') if ctrl => return,
            _ => {}
        }
        match self.focus {
            Focus::Console => self.console_key(d, k),
            Focus::Source => match k.code {
                KeyCode::Up => self.source_cursor = self.source_cursor.saturating_sub(1).max(1),
                KeyCode::Down => {
                    self.source_cursor =
                        (self.source_cursor + 1).min(self.source.len().max(1) as u32)
                }
                KeyCode::PageUp => {
                    self.source_cursor = self.source_cursor.saturating_sub(20).max(1)
                }
                KeyCode::PageDown => {
                    self.source_cursor =
                        (self.source_cursor + 20).min(self.source.len().max(1) as u32)
                }
                KeyCode::Char('b') | KeyCode::Enter => self.toggle_breakpoint(d),
                _ => {}
            },
            Focus::Stack => match k.code {
                KeyCode::Up => self.stack_cursor = self.stack_cursor.saturating_sub(1),
                KeyCode::Down => {
                    self.stack_cursor =
                        (self.stack_cursor + 1).min(d.frames.len().saturating_sub(1))
                }
                KeyCode::Enter => {
                    let n = self.stack_cursor;
                    self.command(d, &format!("frame {n}"));
                }
                _ => {}
            },
            Focus::Variables => match k.code {
                KeyCode::Up => self.tree_cursor = self.tree_cursor.saturating_sub(1),
                KeyCode::Down => {
                    self.tree_cursor = (self.tree_cursor + 1).min(self.tree.len().saturating_sub(1))
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Left | KeyCode::Char(' ') => {
                    if let Some(row) = self.tree.get(self.tree_cursor) {
                        let key = row.key.clone();
                        let expand = match k.code {
                            KeyCode::Right => true,
                            KeyCode::Left => false,
                            _ => !self.expanded.contains(&key),
                        };
                        if expand {
                            self.expanded.insert(key);
                        } else {
                            self.expanded.remove(&key);
                        }
                        self.rebuild_tree(d);
                    }
                }
                KeyCode::Char('w') => self.watch_selected(d),
                _ => {}
            },
            Focus::Disassembly => match k.code {
                KeyCode::Up => self.disasm_scroll = self.disasm_scroll.saturating_sub(1),
                KeyCode::Down => self.disasm_scroll += 1,
                _ => {}
            },
            Focus::Memory => {
                let delta = match k.code {
                    KeyCode::Up => -16i64,
                    KeyCode::Down => 16,
                    KeyCode::PageUp => -256,
                    KeyCode::PageDown => 256,
                    _ => 0,
                };
                if delta != 0 {
                    self.mem_expr = None;
                    self.mem_addr = self.mem_addr.saturating_add_signed(delta);
                    self.mem = d.read_memory(self.mem_addr, 256).unwrap_or_default();
                }
            }
        }
    }

    fn console_key(&mut self, d: &mut Dapper, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                self.command(d, &line);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Up => {
                if self.history.is_empty() {
                    return;
                }
                let i = match self.history_pos {
                    Some(i) => i.saturating_sub(1),
                    None => self.history.len() - 1,
                };
                self.history_pos = Some(i);
                self.input = self.history[i].clone();
            }
            KeyCode::Down => match self.history_pos {
                Some(i) if i + 1 < self.history.len() => {
                    self.history_pos = Some(i + 1);
                    self.input = self.history[i + 1].clone();
                }
                _ => {
                    self.history_pos = None;
                    self.input.clear();
                }
            },
            KeyCode::PageUp => self.console_scroll += 10,
            KeyCode::PageDown => self.console_scroll = self.console_scroll.saturating_sub(10),
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn block(&self, title: String, focus: Focus) -> Block<'static> {
        let style = if self.focus == focus {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .title(Span::styled(title, style))
    }

    /// Draw the UI.
    pub fn draw(&self, d: &Dapper, f: &mut Frame) {
        let [main, console, status] = Layout::vertical([
            Constraint::Fill(3),
            Constraint::Fill(2),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(main);
        let [source, bottom_left] =
            Layout::vertical([Constraint::Fill(3), Constraint::Fill(2)]).areas(left);
        let [disasm, memory] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(bottom_left);
        let [stack, vars] =
            Layout::vertical([Constraint::Fill(1), Constraint::Fill(2)]).areas(right);
        self.draw_source(d, f, source);
        self.draw_disasm(f, disasm);
        self.draw_memory(f, memory);
        self.draw_stack(d, f, stack);
        self.draw_vars(f, vars);
        self.draw_console(f, console);
        let state = match d.state {
            ProgramState::NotStarted => "not started",
            ProgramState::Stopped => "stopped",
            ProgramState::Exited => "exited",
        };
        let reverse = if d.reversible {
            "  S-F5/S-F10/S-F7/S-F8 reverse"
        } else {
            ""
        };
        let help = format!(
            " {} [{state}] │ F5 cont  F10 next  F11 step  S-F11 finish  F7 stepi  F8 nexti{reverse}  F9 bp  Tab focus  Ctrl-D quit",
            self.status
        );
        f.render_widget(
            Paragraph::new(help).style(Style::default().fg(Color::Black).bg(Color::Gray)),
            status,
        );
    }

    fn draw_source(&self, d: &Dapper, f: &mut Frame, area: Rect) {
        let title = match &self.source_path {
            Some(p) => format!(" Source: {} ", basename(p)),
            None => " Source ".into(),
        };
        let height = area.height.saturating_sub(2) as u32;
        let top = self.source_cursor.saturating_sub(height / 2).max(1);
        let mut lines = vec![];
        if self.source.is_empty() {
            lines.push(Line::from(Span::styled(
                match &self.source_path {
                    Some(p) => format!("(source unavailable: {p})"),
                    None => "(no source)".into(),
                },
                Style::default().fg(Color::DarkGray),
            )));
        }
        for n in top..top + height {
            let Some(text) = self.source.get(n as usize - 1) else {
                break;
            };
            let bp = if self.line_has_breakpoint(d, n) {
                "●"
            } else {
                " "
            };
            let cur = if Some(n) == self.current_line {
                "►"
            } else {
                " "
            };
            let mut style = Style::default();
            if Some(n) == self.current_line {
                style = style.bg(Color::Rgb(70, 60, 0)).add_modifier(Modifier::BOLD);
            }
            if self.focus == Focus::Source && n == self.source_cursor {
                style = style.add_modifier(Modifier::REVERSED);
            }
            lines.push(Line::from(vec![
                Span::styled(bp, Style::default().fg(Color::Red)),
                Span::styled(format!("{n:>5} "), Style::default().fg(Color::DarkGray)),
                Span::styled(cur, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {text}"), style),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).block(self.block(title, Focus::Source)),
            area,
        );
    }

    fn draw_disasm(&self, f: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(2) as usize;
        let cur = self.disasm.iter().position(|(_, _, c)| *c).unwrap_or(0);
        let start = (cur + self.disasm_scroll).saturating_sub(height / 2);
        let lines: Vec<Line> = self
            .disasm
            .iter()
            .skip(start)
            .take(height)
            .map(|(pc, text, is_cur)| {
                let style = if *is_cur {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(
                        if *is_cur { "=> " } else { "   " },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(format!("{pc:#08x} "), Style::default().fg(Color::DarkGray)),
                    Span::styled(text.clone(), style),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(self.block(" Disassembly ".into(), Focus::Disassembly)),
            area,
        );
    }

    fn draw_memory(&self, f: &mut Frame, area: Rect) {
        let title = match &self.mem_expr {
            Some(e) => format!(" Memory: {e} = {:#x} ", self.mem_addr),
            None => format!(" Memory: {:#x} ", self.mem_addr),
        };
        let width = area.width.saturating_sub(2) as usize;
        // Bytes per row that fit: "0x00000000: " + 3/byte + 1/byte ascii.
        let per_row = ((width.saturating_sub(13)) / 4).clamp(4, 16);
        let lines: Vec<Line> = self
            .mem
            .chunks(per_row)
            .enumerate()
            .map(|(i, chunk)| {
                let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
                let ascii: String = chunk
                    .iter()
                    .map(|b| {
                        if (0x20..0x7f).contains(b) {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                Line::from(vec![
                    Span::styled(
                        format!("{:#010x} ", self.mem_addr + (i * per_row) as u64),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(format!("{hex:<w$}", w = per_row * 3)),
                    Span::styled(ascii, Style::default().fg(Color::Green)),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(self.block(title, Focus::Memory)),
            area,
        );
    }

    fn draw_stack(&self, d: &Dapper, f: &mut Frame, area: Rect) {
        let lines: Vec<Line> = d
            .frames
            .iter()
            .enumerate()
            .map(|(i, fr)| {
                let loc = fr
                    .source
                    .as_ref()
                    .map(|s| format!(" {}:{}", s.name.as_deref().unwrap_or("?"), fr.line))
                    .unwrap_or_default();
                let marker = if i == d.selected { "*" } else { " " };
                let mut style = Style::default();
                if i == d.selected {
                    style = style.fg(Color::Yellow);
                }
                if self.focus == Focus::Stack && i == self.stack_cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(
                    format!("{marker}#{i:<2} {}{loc}", fr.name),
                    style,
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(self.block(" Stack ".into(), Focus::Stack)),
            area,
        );
    }

    fn draw_vars(&self, f: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(2) as usize;
        let start = self.tree_cursor.saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = self
            .tree
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(i, row)| {
                let arrow = if row.reference != 0 {
                    if self.expanded.contains(&row.key) {
                        "▾ "
                    } else {
                        "▸ "
                    }
                } else {
                    "  "
                };
                let mut style = if row.depth == 0 {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                if self.focus == Focus::Variables && i == self.tree_cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(
                    format!("{}{arrow}{}", "  ".repeat(row.depth), row.text),
                    style,
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(self.block(" Variables ".into(), Focus::Variables)),
            area,
        );
    }

    fn draw_console(&self, f: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(3) as usize;
        let width = area.width.saturating_sub(2).max(1) as usize;
        // Wrap long lines.
        let mut rows: Vec<Line> = vec![];
        for l in &self.console {
            let (text, style) = match l {
                ConsoleLine::Command(c) => {
                    (format!("(dapper) {c}"), Style::default().fg(Color::Cyan))
                }
                ConsoleLine::Output(o) => (o.clone(), Style::default()),
                ConsoleLine::Program(p) => (p.clone(), Style::default().fg(Color::Green)),
            };
            let chars: Vec<char> = text.replace('\t', "    ").chars().collect();
            if chars.is_empty() {
                rows.push(Line::from(""));
            }
            for chunk in chars.chunks(width) {
                rows.push(Line::from(Span::styled(
                    chunk.iter().collect::<String>(),
                    style,
                )));
            }
        }
        let end = rows.len().saturating_sub(self.console_scroll);
        let start = end.saturating_sub(height);
        let mut lines: Vec<Line> = rows[start..end].to_vec();
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(self.input.clone()),
            Span::styled(
                if self.focus == Focus::Console {
                    "█"
                } else {
                    ""
                },
                Style::default().fg(Color::Cyan),
            ),
        ]));
        f.render_widget(
            Paragraph::new(lines).block(self.block(" Console ".into(), Focus::Console)),
            area,
        );
    }
}

/// Run the terminal UI.
pub fn run(
    d: &mut Dapper,
    program_stdout: Option<ChildStdout>,
    script: Option<&str>,
) -> Result<()> {
    let mut app = App::new(program_stdout);
    d.banner();
    app.drain(d);
    if let Some(path) = script {
        for line in std::fs::read_to_string(path)?.lines() {
            app.command(d, line);
        }
    }
    app.refresh(d);
    // While the program runs, Ctrl-C (or Esc) pauses it.
    d.interrupt = Box::new(|| {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read()
                && k.kind == KeyEventKind::Press
                && (k.code == KeyCode::Esc
                    || (k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return true;
            }
        }
        false
    });
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| app.draw(d, f))?;
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(k) = event::read()?
                    && k.kind == KeyEventKind::Press
                {
                    app.key(d, k);
                }
            } else {
                app.drain(d);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Helpers for testing the UI headlessly.
pub mod testing {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Select the first row of the variables pane whose text starts with
    /// `prefix`; returns whether one was found.
    pub fn select_variable(app: &mut App, prefix: &str) -> bool {
        match app.tree.iter().position(|r| r.text.starts_with(prefix)) {
            Some(i) => {
                app.tree_cursor = i;
                true
            }
            None => false,
        }
    }

    /// Render the app to a string, one row per line.
    pub fn render(app: &App, d: &Dapper, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(d, f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}
