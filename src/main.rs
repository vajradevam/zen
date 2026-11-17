// SPDX-License-Identifier: GPL-2.0-only

//! zen — a text editor with nothing in it but a cursor.
//!
//! The whole editor lives in this file and uses nothing beyond the Rust
//! standard library. Settings live in `config.toml`, which is compiled in as
//! the defaults and can be overridden by `~/.config/zen/config.toml`.
//!
//! Layout of this file:
//!   1. terminal (raw mode, size, signals)   5. commands and prompts
//!   2. configuration                        6. key sequences and key handling
//!   3. keyboard and mouse input             7. files, recovery, clipboard
//!   4. text buffer, undo, movement          8. drawing, main

use std::ffi::{c_char, CString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

const DEFAULT_CONFIG: &str = include_str!("../config.toml");

// ============================================================================
// 1. Terminal
// ============================================================================

/// Linux terminal structures and constants.
mod sys {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        pub iflag: u32,
        pub oflag: u32,
        pub cflag: u32,
        pub lflag: u32,
        pub line: u8,
        pub cc: [u8; 32],
        pub ispeed: u32,
        pub ospeed: u32,
    }
    pub type Nfds = u64;
    pub const INPUT_FLAGS: u32 = 0o1 | 0o2 | 0o10 | 0o40 | 0o100 | 0o200 | 0o400 | 0o2000;
    pub const LOCAL_FLAGS: u32 = 0o1 | 0o2 | 0o10 | 0o100 | 0o100000;
    pub const OPOST: u32 = 0o1;
    pub const PARENB: u32 = 0o400;
    pub const CS8: u32 = 0o60;
    pub const VMIN: usize = 6;
    pub const VTIME: usize = 5;
    pub const TIOCGWINSZ: u64 = 0x5413;
}

use sys::Termios;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// C `struct tm`.
#[repr(C)]
struct Tm {
    fields: [i32; 9],
    gmtoff: i64,
    zone: *const c_char,
}

extern "C" {
    fn tcgetattr(fd: i32, t: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, action: i32, t: *const Termios) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn poll(fds: *mut PollFd, n: sys::Nfds, timeout: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    fn isatty(fd: i32) -> i32;
    fn dup2(from: i32, to: i32) -> i32;
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn time(t: *mut i64) -> i64;
    fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    fn strftime(buf: *mut c_char, max: usize, format: *const c_char, tm: *const Tm) -> usize;
}

const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;
const SIGWINCH: i32 = 28;

static RESIZED: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);
static ORIGINAL: OnceLock<Termios> = OnceLock::new();

extern "C" fn on_signal(sig: i32) {
    if sig == SIGWINCH {
        RESIZED.store(true, Ordering::Relaxed);
    } else {
        STOPPED.store(true, Ordering::Relaxed);
    }
}

fn term_enter(cfg: &Config) -> Result<(), String> {
    unsafe {
        if isatty(0) == 0 || isatty(1) == 0 {
            return Err("zen needs to run in a terminal".into());
        }
        let mut t: Termios = std::mem::zeroed();
        if tcgetattr(0, &mut t) != 0 {
            return Err("cannot read terminal settings".into());
        }
        let _ = ORIGINAL.set(t);
        // Raw mode: no echo, no line buffering, no signal keys, no translation.
        t.iflag &= !sys::INPUT_FLAGS;
        t.oflag &= !sys::OPOST;
        t.lflag &= !sys::LOCAL_FLAGS;
        t.cflag = (t.cflag & !sys::PARENB) | sys::CS8;
        t.cc[sys::VMIN] = 1;
        t.cc[sys::VTIME] = 0;
        if tcsetattr(0, 2, &t) != 0 {
            return Err("cannot switch the terminal to raw mode".into());
        }
        signal(SIGWINCH, on_signal);
        signal(SIGTERM, on_signal);
        signal(SIGHUP, on_signal);
    }
    let shape = match cfg.cursor.as_str() {
        "block" => 1,
        "underline" => 3,
        _ => 5,
    } + if cfg.cursor_blink { 0 } else { 1 };
    // Alternate screen, bracketed paste, cursor shape and colour, mouse.
    let mut s = format!("\x1b[?1049h\x1b[?2004h\x1b[{shape} q");
    if let Some((r, g, b)) = cfg.cursor_color {
        s.push_str(&format!("\x1b]12;#{r:02x}{g:02x}{b:02x}\x07"));
    }
    if cfg.mouse {
        s.push_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    }
    put(&s);
    Ok(())
}

fn term_leave() {
    put("\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[0 q\x1b]112\x07\x1b[?2004l\x1b[?1049l\x1b[?25h");
    if let Some(t) = ORIGINAL.get() {
        unsafe {
            tcsetattr(0, 2, t);
        }
    }
}

fn term_size() -> (usize, usize) {
    let mut ws = [0u16; 4];
    let ok = unsafe { ioctl(1, sys::TIOCGWINSZ, ws.as_mut_ptr()) } == 0;
    if ok && ws[0] > 0 && ws[1] > 0 {
        (ws[0] as usize, ws[1] as usize)
    } else {
        (24, 80)
    }
}

fn put(s: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// The current local time, formatted the way C's strftime does it.
fn now(format: &str) -> String {
    let Ok(format) = CString::new(format) else {
        return String::new();
    };
    let mut buf = [0u8; 256];
    let n = unsafe {
        let t = time(std::ptr::null_mut());
        let mut tm: Tm = std::mem::zeroed();
        localtime_r(&t, &mut tm);
        strftime(buf.as_mut_ptr() as *mut c_char, buf.len(), format.as_ptr(), &tm)
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

// ============================================================================
// 2. Configuration
// ============================================================================

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmd {
    Save,
    SaveAs,
    Quit,
    ForceQuit,
    SaveQuit,
    Undo,
    Redo,
    Find,
    FindNext,
    FindPrev,
    Replace,
    GotoLine,
    Select,
    SelectAll,
    Copy,
    Cut,
    Paste,
    DeleteLine,
    DuplicateLine,
    LineUp,
    LineDown,
    ParagraphUp,
    ParagraphDown,
    Reflow,
    WordCount,
    Typewriter,
    Focus,
}

const COMMANDS: &[(&str, Cmd)] = &[
    ("save", Cmd::Save),
    ("save_as", Cmd::SaveAs),
    ("quit", Cmd::Quit),
    ("force_quit", Cmd::ForceQuit),
    ("save_quit", Cmd::SaveQuit),
    ("undo", Cmd::Undo),
    ("redo", Cmd::Redo),
    ("find", Cmd::Find),
    ("find_next", Cmd::FindNext),
    ("find_prev", Cmd::FindPrev),
    ("replace", Cmd::Replace),
    ("goto_line", Cmd::GotoLine),
    ("select", Cmd::Select),
    ("select_all", Cmd::SelectAll),
    ("copy", Cmd::Copy),
    ("cut", Cmd::Cut),
    ("paste", Cmd::Paste),
    ("delete_line", Cmd::DeleteLine),
    ("duplicate_line", Cmd::DuplicateLine),
    ("line_up", Cmd::LineUp),
    ("line_down", Cmd::LineDown),
    ("paragraph_up", Cmd::ParagraphUp),
    ("paragraph_down", Cmd::ParagraphDown),
    ("reflow", Cmd::Reflow),
    ("word_count", Cmd::WordCount),
    ("typewriter", Cmd::Typewriter),
    ("focus", Cmd::Focus),
];

/// What a completed key sequence does.
#[derive(Clone, PartialEq)]
enum Action {
    Cmd(Cmd),
    /// Insert this text (after filling in `{date}` and friends).
    Text(String),
    /// Pipe the selection through this shell command.
    Shell(String),
}

type Rgb = (u8, u8, u8);

#[derive(Default)]
struct Config {
    cursor: String,
    cursor_blink: bool,
    cursor_color: Option<Rgb>,
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    dim_color: Option<Rgb>,
    margin_top: usize,
    margin_bottom: usize,
    margin_left: usize,
    margin_right: usize,
    width: usize,
    center: bool,
    typewriter: bool,
    typewriter_position: usize,
    focus: String,
    reflow_width: usize,
    tab_width: usize,
    soft_tabs: bool,
    auto_indent: bool,
    final_newline: bool,
    mouse: bool,
    system_clipboard: bool,
    copy_command: String,
    paste_command: String,
    recovery: bool,
    recovery_interval: u64,
    autosave: bool,
    autosave_delay: u64,
    check_disk: bool,
    remember_position: bool,
    sequence_timeout: u64,
    messages: bool,
    keys: Vec<(String, Action)>,
}

impl Config {
    fn load(explicit: Option<PathBuf>) -> Result<Config, String> {
        let mut cfg = Config::default();
        cfg.apply(DEFAULT_CONFIG).map_err(|e| format!("built-in config: {e}"))?;
        let path = explicit.clone().or_else(|| Some(home_dir("XDG_CONFIG_HOME", ".config")?.join("config.toml")));
        if let Some(path) = path {
            match fs::read_to_string(&path) {
                Ok(src) => cfg.apply(&src).map_err(|e| format!("{}: {e}", path.display()))?,
                Err(e) if explicit.is_some() => return Err(format!("{}: {e}", path.display())),
                Err(_) => {}
            }
        }
        cfg.check().map_err(|e| format!("config: {e}"))?;
        Ok(cfg)
    }

    fn apply(&mut self, src: &str) -> Result<(), String> {
        let mut section = String::new();
        for (n, raw) in src.lines().enumerate() {
            let at = |e: String| format!("line {}: {e}", n + 1);
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_string();
                continue;
            }
            let (key, value) = split_assignment(line).ok_or_else(|| at("expected `name = value`".into()))?;
            let key = parse_values(key).map_err(at)?.swap_remove(0);
            let values = parse_values(value).map_err(at)?;
            match section.as_str() {
                "" => self.set(&key, &values).map_err(at)?,
                "keys" => {
                    let cmd = COMMANDS
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, c)| Action::Cmd(*c))
                        .ok_or_else(|| at(format!("unknown command `{key}`")))?;
                    self.keys.retain(|(_, a)| *a != cmd);
                    self.keys.extend(values.into_iter().filter(|v| !v.is_empty()).map(|v| (v, cmd.clone())));
                }
                "expand" | "shell" => {
                    let [value] = values.as_slice() else {
                        return Err(at(format!("\"{key}\" takes a single value")));
                    };
                    self.keys.retain(|(s, _)| *s != key);
                    if !value.is_empty() && !key.is_empty() {
                        let action = if section == "shell" { Action::Shell(value.clone()) } else { Action::Text(value.clone()) };
                        self.keys.push((key, action));
                    }
                }
                other => return Err(at(format!("unknown section [{other}]"))),
            }
        }
        Ok(())
    }

    fn set(&mut self, key: &str, values: &[String]) -> Result<(), String> {
        let [v] = values else {
            return Err(format!("`{key}` takes a single value"));
        };
        let flag = || match v.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("`{key}` must be true or false")),
        };
        let number = || v.parse::<u64>().map_err(|_| format!("`{key}` must be a whole number"));
        let colour = || {
            if v.is_empty() {
                return Ok(None);
            }
            let hex = v.strip_prefix('#').unwrap_or(v);
            match (hex.len(), u32::from_str_radix(hex, 16)) {
                (6, Ok(n)) => Ok(Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))),
                _ => Err(format!("`{key}` must look like \"#rrggbb\"")),
            }
        };
        let one_of = |allowed: &[&str]| {
            if allowed.contains(&v.as_str()) {
                Ok(v.clone())
            } else {
                Err(format!("`{key}` must be one of: {}", allowed.join(", ")))
            }
        };
        match key {
            "cursor" => self.cursor = one_of(&["block", "bar", "underline"])?,
            "cursor_blink" => self.cursor_blink = flag()?,
            "cursor_color" | "cursor_colour" => self.cursor_color = colour()?,
            "foreground" => self.foreground = colour()?,
            "background" => self.background = colour()?,
            "dim_color" | "dim_colour" => self.dim_color = colour()?,
            "margin_top" => self.margin_top = number()? as usize,
            "margin_bottom" => self.margin_bottom = number()? as usize,
            "margin_left" => self.margin_left = number()? as usize,
            "margin_right" => self.margin_right = number()? as usize,
            "width" => self.width = number()? as usize,
            "center" | "centre" => self.center = flag()?,
            "typewriter" => self.typewriter = flag()?,
            "typewriter_position" => self.typewriter_position = (number()? as usize).min(100),
            "focus" => self.focus = one_of(&["off", "paragraph", "sentence", "line"])?,
            "reflow_width" => self.reflow_width = number()? as usize,
            "tab_width" => self.tab_width = (number()? as usize).clamp(1, 16),
            "soft_tabs" => self.soft_tabs = flag()?,
            "auto_indent" => self.auto_indent = flag()?,
            "final_newline" => self.final_newline = flag()?,
            "mouse" => self.mouse = flag()?,
            "system_clipboard" => self.system_clipboard = flag()?,
            "copy_command" => self.copy_command = v.clone(),
            "paste_command" => self.paste_command = v.clone(),
            "recovery" => self.recovery = flag()?,
            "recovery_interval" => self.recovery_interval = number()?.max(1),
            "autosave" => self.autosave = flag()?,
            "autosave_delay" => self.autosave_delay = number()?,
            "check_disk" => self.check_disk = flag()?,
            "remember_position" => self.remember_position = flag()?,
            "sequence_timeout" => self.sequence_timeout = number()?,
            "messages" => self.messages = flag()?,
            _ => return Err(format!("unknown setting `{key}`")),
        }
        Ok(())
    }

    /// A sequence that is the start of another could never let the longer one run.
    fn check(&self) -> Result<(), String> {
        for (i, (a, _)) in self.keys.iter().enumerate() {
            for (j, (b, _)) in self.keys.iter().enumerate() {
                if i != j && b.starts_with(a.as_str()) {
                    return Err(format!("key sequence \"{a}\" is the start of \"{b}\"; make them distinct"));
                }
            }
        }
        Ok(())
    }

    fn sequence_for(&self, cmd: Cmd) -> Option<&str> {
        self.keys.iter().find(|(_, a)| *a == Action::Cmd(cmd)).map(|(s, _)| s.as_str())
    }
}

/// `$<var>/zen`, or `~/<fallback>/zen` when the variable is not set.
fn home_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))?;
    Some(base.join("zen"))
}

fn state_dir() -> Option<PathBuf> {
    home_dir("XDG_STATE_HOME", ".local/state")
}

/// Byte offset of the first `stop` character that is not inside quotes.
fn find_unquoted(line: &str, stop: char) -> Option<usize> {
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            c if c == stop && !quoted => return Some(i),
            _ => {}
        }
    }
    None
}

fn strip_comment(line: &str) -> &str {
    find_unquoted(line, '#').map_or(line, |i| &line[..i])
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    find_unquoted(line, '=').map(|i| (&line[..i], &line[i + 1..]))
}

/// Parses `value`, `"value"` or `["a", "b"]` into a list of strings.
fn parse_values(src: &str) -> Result<Vec<String>, String> {
    let src = src.trim();
    let inner = match src.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']').ok_or("missing `]`")?,
        None => src,
    };
    let mut items = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == ',' {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut item = String::new();
            loop {
                match chars.next().ok_or("missing closing quote")? {
                    '"' => break,
                    '\\' => match chars.next().ok_or("missing closing quote")? {
                        'n' => item.push('\n'),
                        't' => item.push('\t'),
                        other => item.push(other),
                    },
                    other => item.push(other),
                }
            }
            items.push(item);
        } else {
            let mut item = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == ',' {
                    break;
                }
                item.push(c);
                chars.next();
            }
            items.push(item);
        }
    }
    if items.is_empty() {
        return Err("missing value".into());
    }
    Ok(items)
}

/// Fills in `{date}`, `{time}` and `{%strftime}` in an expansion.
fn expand(template: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(len) = rest[open..].find('}') else {
            rest = &rest[open..];
            break;
        };
        let inner = &rest[open + 1..open + len];
        match inner {
            "date" => out.push_str(&now("%Y-%m-%d")),
            "time" => out.push_str(&now("%H:%M")),
            _ if inner.contains('%') => out.push_str(&now(inner)),
            _ => out.push_str(&rest[open..=open + len]),
        }
        rest = &rest[open + len + 1..];
        if rest.is_empty() {
            return out;
        }
    }
    out.push_str(rest);
    out
}

// ============================================================================
// 3. Keyboard and mouse input
// ============================================================================

#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Paste(String),
    Enter,
    Tab,
    Backspace,
    BackWord,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    /// Mouse button pressed at (column, row), counted from 0.
    Click(usize, usize),
    Drag(usize, usize),
    WheelUp,
    WheelDown,
    Ignore,
}

struct Press {
    key: Key,
    shift: bool,
    ctrl: bool,
}

enum Event {
    Press(Press),
    Resize,
    Timeout,
    Closed,
}

enum Parsed {
    Press(Press),
    Incomplete,
    Empty,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Default)]
struct Input {
    buf: Vec<u8>,
}

impl Input {
    /// Waits for the next event. `timeout` of `None` waits forever.
    fn next(&mut self, timeout: Option<Duration>) -> Event {
        loop {
            if STOPPED.load(Ordering::Relaxed) {
                return Event::Closed;
            }
            if let Some(p) = self.buffered() {
                return Event::Press(p);
            }
            if RESIZED.swap(false, Ordering::Relaxed) {
                return Event::Resize;
            }
            // A half-received escape sequence gets a short grace period.
            let wait = if !self.buf.is_empty() {
                30
            } else {
                timeout.map_or(-1, |d| d.as_millis().min(60_000) as i32)
            };
            let mut fd = PollFd { fd: 0, events: 1, revents: 0 };
            let ready = unsafe { poll(&mut fd, 1, wait) };
            if ready < 0 {
                continue; // interrupted by a signal
            }
            if ready == 0 {
                if self.buf.is_empty() {
                    return Event::Timeout;
                }
                if self.buf.starts_with(PASTE_START) {
                    continue; // a paste still arriving
                }
                let lone_escape = self.buf == [0x1b];
                self.buf.clear();
                if lone_escape {
                    return Event::Press(Press { key: Key::Esc, shift: false, ctrl: false });
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            let n = unsafe { read(0, chunk.as_mut_ptr(), chunk.len()) };
            if n == 0 {
                return Event::Closed;
            }
            if n > 0 {
                self.buf.extend_from_slice(&chunk[..n as usize]);
            }
        }
    }

    /// Returns a key press that is already fully received, if any.
    fn buffered(&mut self) -> Option<Press> {
        match self.parse() {
            Parsed::Press(p) => Some(p),
            _ => None,
        }
    }

    fn take(&mut self, n: usize, key: Key) -> Parsed {
        self.buf.drain(..n);
        Parsed::Press(Press { key, shift: false, ctrl: false })
    }

    fn parse(&mut self) -> Parsed {
        let b = &self.buf;
        let Some(&first) = b.first() else {
            return Parsed::Empty;
        };
        match first {
            0x1b => self.parse_escape(),
            b'\r' | b'\n' => self.take(1, Key::Enter),
            b'\t' => self.take(1, Key::Tab),
            0x7f | 0x08 => self.take(1, Key::Backspace),
            0..=0x1f => self.take(1, Key::Ignore), // control keys do nothing
            _ => {
                let len = match first {
                    0..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => return self.take(1, Key::Ignore),
                };
                if b.len() < len {
                    return Parsed::Incomplete;
                }
                match std::str::from_utf8(&b[..len]).ok().and_then(|s| s.chars().next()) {
                    Some(c) => self.take(len, Key::Char(c)),
                    None => self.take(1, Key::Ignore),
                }
            }
        }
    }

    fn parse_escape(&mut self) -> Parsed {
        let b = &self.buf;
        if b.len() < 2 {
            return Parsed::Incomplete;
        }
        match b[1] {
            b'[' => {
                let Some(end) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)).map(|i| i + 2) else {
                    return Parsed::Incomplete;
                };
                let text = String::from_utf8_lossy(&b[2..end]).into_owned();
                let mouse = text.starts_with('<');
                let params: Vec<usize> = text.trim_start_matches('<').split(';').map(|p| p.parse().unwrap_or(0)).collect();
                let first = params.first().copied().unwrap_or(0);
                let mods = params.get(1).copied().unwrap_or(1).saturating_sub(1);
                let key = match b[end] {
                    b'M' if mouse && params.len() == 3 => {
                        let (x, y) = (params[1].saturating_sub(1), params[2].saturating_sub(1));
                        match first {
                            b if b & 64 != 0 && b & 1 == 0 => Key::WheelUp,
                            b if b & 64 != 0 => Key::WheelDown,
                            b if b & 3 != 0 => Key::Ignore, // not the left button
                            b if b & 32 != 0 => Key::Drag(x, y),
                            _ => Key::Click(x, y),
                        }
                    }
                    _ if mouse => Key::Ignore,
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    b'~' => match first {
                        1 | 7 => Key::Home,
                        4 | 8 => Key::End,
                        3 => Key::Delete,
                        5 => Key::PageUp,
                        6 => Key::PageDown,
                        200 => {
                            let body = &b[end + 1..];
                            let Some(len) = body.windows(PASTE_END.len()).position(|w| w == PASTE_END) else {
                                return Parsed::Incomplete;
                            };
                            let text = String::from_utf8_lossy(&body[..len]).replace("\r\n", "\n").replace('\r', "\n");
                            return self.take(end + 1 + len + PASTE_END.len(), Key::Paste(text));
                        }
                        _ => Key::Ignore,
                    },
                    _ => Key::Ignore,
                };
                self.buf.drain(..=end);
                let (shift, ctrl) = if mouse { (false, false) } else { (mods & 1 != 0, mods & 4 != 0) };
                Parsed::Press(Press { key, shift, ctrl })
            }
            b'O' => {
                if b.len() < 3 {
                    return Parsed::Incomplete;
                }
                let key = match b[2] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => Key::Ignore,
                };
                self.take(3, key)
            }
            0x7f | 0x08 => self.take(2, Key::BackWord), // Alt+Backspace
            _ => self.take(1, Key::Esc),
        }
    }
}

// ============================================================================
// 4. Text buffer, undo, movement
// ============================================================================

/// A place in the text: line number and byte offset within that line.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Pos {
    row: usize,
    col: usize,
}

const START: Pos = Pos { row: 0, col: 0 };

/// One undoable change: `removed` was replaced by `inserted` at `at`.
struct Edit {
    at: Pos,
    removed: String,
    inserted: String,
    before: Pos,
    after: Pos,
    typing: bool,
}

/// Where text ends if it is inserted at `at`.
fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos { row: at.row, col: at.col + text.len() },
        Some(i) => Pos { row: at.row + text.matches('\n').count(), col: text.len() - i - 1 },
    }
}

/// Columns a character occupies when drawn at column `x`.
fn char_width(c: char, x: usize, tab: usize) -> usize {
    match c as u32 {
        0x09 => tab - x % tab,
        0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x200b..=0x200f | 0x20d0..=0x20ff | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1faff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

fn text_width(s: &str, tab: usize) -> usize {
    s.chars().fold(0, |x, c| x + char_width(c, x, tab))
}

/// Splits a line into the byte ranges of its screen rows, breaking at spaces.
fn wrap(line: &str, width: usize, tab: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let (mut start, mut x, mut i) = (0, 0, 0);
    let mut brk: Option<usize> = None;
    while i < line.len() {
        let c = line[i..].chars().next().unwrap();
        let w = char_width(c, x, tab);
        if x + w > width && i > start {
            if c == ' ' {
                // Let the space hang off the end rather than start the next row.
                i += 1;
                rows.push((start, i));
                (start, x, brk) = (i, 0, None);
                continue;
            }
            let at = brk.filter(|&b| b > start).unwrap_or(i);
            rows.push((start, at));
            (start, brk) = (at, None);
            x = text_width(&line[at..i], tab);
            continue;
        }
        x += w;
        i += c.len_utf8();
        if c == ' ' || c == '\t' {
            brk = Some(i);
        }
    }
    rows.push((start, line.len()));
    rows
}

/// Which screen row of a wrapped line holds byte offset `col`.
fn row_of(rows: &[(usize, usize)], col: usize) -> usize {
    rows.iter().rposition(|r| r.0 <= col).unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Space,
    Word,
    Other,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Other
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    Find,
    Replace,
    ReplaceWith,
    SaveAs,
    Line,
}

/// A question on the bottom row that takes typed text.
struct Prompt {
    ask: Ask,
    text: String,
    then_quit: bool,
}

/// A question on the bottom row answered with a single key.
enum Confirm {
    /// Offer unsaved text found from an earlier session.
    Recover(String),
    /// Stepping through matches during find and replace.
    Replace,
}

struct Editor {
    cfg: Config,
    lines: Vec<String>,
    cur: Pos,
    /// Column the cursor tries to keep while moving up and down.
    want_x: Option<usize>,
    /// Other end of the selection.
    anchor: Option<Pos>,
    /// Selection was started with the `select` command, so plain movement extends it.
    sticky: bool,
    /// First visible screen row: (line, row within the wrapped line).
    top: (usize, usize),
    /// Empty rows above the first line (typewriter scrolling near the start).
    lead: usize,
    path: Option<PathBuf>,
    crlf: bool,
    dirty: bool,
    /// Counts every change to the text.
    edits: u64,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Recently typed keys that could still become a command sequence.
    pending: String,
    /// The pending keys are being held back instead of shown (a selection is active).
    held: bool,
    seq_dirty: bool,
    seq_redo: Vec<Edit>,
    last_key: Instant,
    last_input: Instant,
    clipboard: String,
    query: String,
    replacement: String,
    message: Option<String>,
    prompt: Option<Prompt>,
    confirm: Option<Confirm>,
    /// Modification time and size of the file when last read or written.
    disk: Option<(SystemTime, u64)>,
    overwrite: bool,
    recovered_edits: u64,
    last_recovery: Instant,
    autosave_tried: u64,
    rows: usize,
    cols: usize,
    quit: bool,
}

impl Editor {
    fn new(cfg: Config, path: Option<PathBuf>, piped: Option<String>) -> Result<Editor, String> {
        let (rows, cols) = term_size();
        let mut ed = Editor {
            cfg,
            lines: vec![String::new()],
            cur: START,
            want_x: None,
            anchor: None,
            sticky: false,
            top: (0, 0),
            lead: 0,
            path,
            crlf: false,
            dirty: false,
            edits: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: String::new(),
            held: false,
            seq_dirty: false,
            seq_redo: Vec::new(),
            last_key: Instant::now(),
            last_input: Instant::now(),
            clipboard: String::new(),
            query: String::new(),
            replacement: String::new(),
            message: None,
            prompt: None,
            confirm: None,
            disk: None,
            overwrite: false,
            recovered_edits: 0,
            last_recovery: Instant::now(),
            autosave_tried: 0,
            rows,
            cols,
            quit: false,
        };
        if let Some(p) = ed.path.clone() {
            match fs::read(&p) {
                Ok(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|_| format!("{}: not a UTF-8 text file", p.display()))?;
                    ed.crlf = text.contains("\r\n");
                    ed.lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
                    ed.disk = stamp(&p);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", p.display())),
            }
        }
        if let Some(text) = piped {
            if !text.is_empty() {
                ed.lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
                ed.dirty = true;
                ed.edits = 1;
            }
        } else {
            ed.restore_position();
            ed.offer_recovery();
        }
        Ok(ed)
    }

    // ---- geometry ----------------------------------------------------------

    /// (left edge, text width, top edge, text height) of the page on screen.
    fn page(&self) -> (usize, usize, usize, usize) {
        let c = &self.cfg;
        let (ml, mr) = if self.cols > c.margin_left + c.margin_right + 1 { (c.margin_left, c.margin_right) } else { (0, 0) };
        let avail = self.cols.saturating_sub(ml + mr).max(1);
        let mut width = if c.width > 0 { c.width.min(avail) } else { avail };
        let left = ml + if c.center { (avail - width) / 2 } else { 0 };
        if left + width >= self.cols && width > 1 {
            width -= 1; // keep a column free for the cursor after the last letter
        }
        let (top, height) = if self.rows > c.margin_top + c.margin_bottom {
            (c.margin_top, self.rows - c.margin_top - c.margin_bottom)
        } else {
            (0, self.rows.max(1))
        };
        (left, width, top, height)
    }

    fn wrapped(&self, row: usize) -> Vec<(usize, usize)> {
        wrap(&self.lines[row], self.page().1, self.cfg.tab_width)
    }

    fn end(&self) -> Pos {
        let row = self.lines.len() - 1;
        Pos { row, col: self.lines[row].len() }
    }

    /// The nearest valid position to `p`.
    fn clamp(&self, p: Pos) -> Pos {
        let row = p.row.min(self.lines.len() - 1);
        let mut col = p.col.min(self.lines[row].len());
        while !self.lines[row].is_char_boundary(col) {
            col -= 1;
        }
        Pos { row, col }
    }

    fn blank(&self, row: usize) -> bool {
        self.lines[row].trim().is_empty()
    }

    /// First and last line of the paragraph around `row` (which must not be blank).
    fn paragraph(&self, row: usize) -> (usize, usize) {
        let (mut a, mut b) = (row, row);
        while a > 0 && !self.blank(a - 1) {
            a -= 1;
        }
        while b + 1 < self.lines.len() && !self.blank(b + 1) {
            b += 1;
        }
        (a, b)
    }

    // ---- editing primitives ------------------------------------------------

    /// Replaces the text between `start` and `end`, returning what was there.
    fn splice(&mut self, start: Pos, end: Pos, text: &str) -> String {
        let removed = self.text_between(start, end);
        let tail = self.lines[end.row][end.col..].to_string();
        self.lines[start.row].truncate(start.col);
        self.lines.drain(start.row + 1..end.row + 1);
        let mut parts = text.split('\n');
        self.lines[start.row].push_str(parts.next().unwrap_or(""));
        let rest: Vec<String> = parts.map(String::from).collect();
        let last = start.row + rest.len();
        self.lines.splice(start.row + 1..start.row + 1, rest);
        self.lines[last].push_str(&tail);
        self.edits += 1;
        removed
    }

    fn text_between(&self, s: Pos, e: Pos) -> String {
        if s.row == e.row {
            return self.lines[s.row][s.col..e.col].to_string();
        }
        let mut text = self.lines[s.row][s.col..].to_string();
        for r in s.row + 1..e.row {
            text.push('\n');
            text.push_str(&self.lines[r]);
        }
        text.push('\n');
        text.push_str(&self.lines[e.row][..e.col]);
        text
    }

    /// An undoable replacement. Consecutive typing is merged word by word.
    fn edit(&mut self, start: Pos, end: Pos, text: &str, typing: bool) {
        if start == end && text.is_empty() {
            return;
        }
        let before = self.cur;
        let removed = self.splice(start, end, text);
        let after = end_of(start, text);
        self.cur = after;
        let typing = typing && removed.is_empty();
        let merged = typing
            && self.undo.last_mut().is_some_and(|top| {
                let joins = top.typing
                    && end_of(top.at, &top.inserted) == start
                    && !(top.inserted.ends_with(char::is_whitespace) && !text.starts_with(char::is_whitespace));
                if joins {
                    top.inserted.push_str(text);
                    top.after = after;
                }
                joins
            });
        if !merged {
            self.undo.push(Edit { at: start, removed, inserted: text.to_string(), before, after, typing });
        }
        self.redo.clear();
        self.touched();
    }

    /// Moves the cursor somewhere other than the end of the edit just made.
    fn settle(&mut self, p: Pos) {
        self.cur = self.clamp(p);
        if let Some(top) = self.undo.last_mut() {
            top.after = self.cur;
        }
    }

    fn undo(&mut self) {
        if let Some(e) = self.undo.pop() {
            self.splice(e.at, end_of(e.at, &e.inserted), &e.removed);
            self.cur = e.before;
            self.redo.push(e);
            self.touched();
        }
    }

    fn redo(&mut self) {
        if let Some(mut e) = self.redo.pop() {
            self.splice(e.at, end_of(e.at, &e.removed), &e.inserted);
            self.cur = e.after;
            e.typing = false; // later typing starts a fresh undo step
            self.undo.push(e);
            self.touched();
        }
    }

    fn touched(&mut self) {
        self.dirty = true;
        self.anchor = None;
        self.sticky = false;
        self.want_x = None;
    }

    fn selection(&self) -> Option<(Pos, Pos)> {
        self.anchor.filter(|a| *a != self.cur).map(|a| (a.min(self.cur), a.max(self.cur)))
    }

    fn selected_text(&self) -> Option<String> {
        self.selection().map(|(s, e)| self.text_between(s, e))
    }

    /// Inserts at the cursor, replacing the selection if there is one.
    fn insert(&mut self, text: &str, typing: bool) {
        let (s, e) = self.selection().unwrap_or((self.cur, self.cur));
        self.edit(s, e, text, typing);
    }

    // ---- movement ----------------------------------------------------------

    fn left_of(&self, p: Pos) -> Pos {
        if let Some((i, _)) = self.lines[p.row][..p.col].char_indices().next_back() {
            Pos { row: p.row, col: i }
        } else if p.row > 0 {
            Pos { row: p.row - 1, col: self.lines[p.row - 1].len() }
        } else {
            p
        }
    }

    fn right_of(&self, p: Pos) -> Pos {
        if let Some(c) = self.lines[p.row][p.col..].chars().next() {
            Pos { row: p.row, col: p.col + c.len_utf8() }
        } else if p.row + 1 < self.lines.len() {
            Pos { row: p.row + 1, col: 0 }
        } else {
            p
        }
    }

    fn word_left(&self, p: Pos) -> Pos {
        if p.col == 0 {
            return self.left_of(p);
        }
        let head = &self.lines[p.row][..p.col];
        let trimmed = head.trim_end();
        let Some(last) = trimmed.chars().next_back() else {
            return Pos { row: p.row, col: 0 };
        };
        let kind = class(last);
        let col = trimmed.char_indices().rev().take_while(|(_, c)| class(*c) == kind).last().map_or(0, |(i, _)| i);
        Pos { row: p.row, col }
    }

    fn word_right(&self, p: Pos) -> Pos {
        let tail = &self.lines[p.row][p.col..];
        let Some(first) = tail.chars().next() else {
            return self.right_of(p);
        };
        let kind = class(first);
        let mut skipping_word = kind != Class::Space;
        for (i, c) in tail.char_indices() {
            if skipping_word && class(c) != kind {
                skipping_word = false;
            }
            if !skipping_word && class(c) != Class::Space {
                return Pos { row: p.row, col: p.col + i };
            }
        }
        Pos { row: p.row, col: self.lines[p.row].len() }
    }

    /// Byte offset in screen row `idx` of line `row` closest to column `want`.
    fn col_at(&self, row: usize, rows: &[(usize, usize)], idx: usize, want: usize) -> usize {
        let (s, e) = rows[idx];
        let (mut x, mut last) = (0, s);
        for (i, c) in self.lines[row][s..e].char_indices() {
            if x >= want {
                return s + i;
            }
            x += char_width(c, x, self.cfg.tab_width);
            last = s + i;
        }
        // The end of a wrapped row belongs to the row below it.
        if idx + 1 == rows.len() {
            e
        } else {
            last
        }
    }

    fn move_vertical(&mut self, down: bool) {
        let rows = self.wrapped(self.cur.row);
        let idx = row_of(&rows, self.cur.col);
        let tab = self.cfg.tab_width;
        let want = self.want_x.unwrap_or_else(|| text_width(&self.lines[self.cur.row][rows[idx].0..self.cur.col], tab));
        let (row, rows, idx) = if down && idx + 1 < rows.len() {
            (self.cur.row, rows, idx + 1)
        } else if down && self.cur.row + 1 < self.lines.len() {
            (self.cur.row + 1, self.wrapped(self.cur.row + 1), 0)
        } else if !down && idx > 0 {
            (self.cur.row, rows, idx - 1)
        } else if !down && self.cur.row > 0 {
            let above = self.wrapped(self.cur.row - 1);
            let last = above.len() - 1;
            (self.cur.row - 1, above, last)
        } else {
            self.cur.col = if down { self.lines[self.cur.row].len() } else { 0 };
            return;
        };
        self.cur = Pos { row, col: self.col_at(row, &rows, idx, want) };
        self.want_x = Some(want);
    }

    /// Before the cursor moves: start, keep or drop the selection.
    fn begin_move(&mut self, shift: bool) {
        if shift {
            self.anchor.get_or_insert(self.cur);
        } else if !self.sticky {
            self.anchor = None;
        }
    }

    fn navigate(&mut self, key: &Key, shift: bool, ctrl: bool) {
        self.begin_move(shift);
        let vertical = matches!(key, Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::WheelUp | Key::WheelDown);
        match key {
            Key::Left if ctrl => self.cur = self.word_left(self.cur),
            Key::Right if ctrl => self.cur = self.word_right(self.cur),
            Key::Left => self.cur = self.left_of(self.cur),
            Key::Right => self.cur = self.right_of(self.cur),
            Key::Up => self.move_vertical(false),
            Key::Down => self.move_vertical(true),
            Key::WheelUp | Key::WheelDown => {
                for _ in 0..3 {
                    self.move_vertical(*key == Key::WheelDown);
                }
            }
            Key::PageUp | Key::PageDown => {
                for _ in 1..self.page().3.max(2) {
                    self.move_vertical(*key == Key::PageDown);
                }
            }
            Key::Home if ctrl => self.cur = START,
            Key::End if ctrl => self.cur = self.end(),
            Key::Home => {
                let rows = self.wrapped(self.cur.row);
                self.cur.col = rows[row_of(&rows, self.cur.col)].0;
            }
            Key::End => {
                let rows = self.wrapped(self.cur.row);
                let idx = row_of(&rows, self.cur.col);
                self.cur.col = self.col_at(self.cur.row, &rows, idx, usize::MAX);
            }
            _ => {}
        }
        if !vertical {
            self.want_x = None;
        }
    }

    /// The text position under screen cell (x, y).
    fn pos_at(&self, x: usize, y: usize) -> Pos {
        let (left, _, top, height) = self.page();
        let mut down = y.saturating_sub(top).min(height - 1).saturating_sub(self.lead);
        let (mut row, mut idx) = self.top;
        let mut rows = self.wrapped(row);
        while down > 0 {
            if idx + 1 < rows.len() {
                idx += 1;
            } else if row + 1 < self.lines.len() {
                row += 1;
                idx = 0;
                rows = self.wrapped(row);
            } else {
                return self.end();
            }
            down -= 1;
        }
        Pos { row, col: self.col_at(row, &rows, idx, x.saturating_sub(left)) }
    }

    // ========================================================================
    // 5. Commands and prompts
    // ========================================================================

    fn warn(&mut self, text: impl Into<String>) {
        if self.cfg.messages {
            self.message = Some(text.into());
        }
    }

    fn ask(&mut self, ask: Ask, text: String, then_quit: bool) {
        self.prompt = Some(Prompt { ask, text, then_quit });
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Cmd(cmd) => self.run(cmd),
            Action::Text(template) => self.insert(&expand(&template), false),
            Action::Shell(command) => self.shell(&command),
        }
    }

    fn run(&mut self, cmd: Cmd) {
        let row = self.cur.row;
        match cmd {
            Cmd::Save => {
                if self.path.is_some() {
                    self.save(false);
                } else {
                    self.ask(Ask::SaveAs, String::new(), false);
                }
            }
            Cmd::SaveAs => {
                let current = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                self.ask(Ask::SaveAs, current, false);
            }
            Cmd::SaveQuit => {
                if self.path.is_none() {
                    self.ask(Ask::SaveAs, String::new(), true);
                } else if self.save(false) {
                    self.quit = true;
                }
            }
            Cmd::Quit => {
                if self.dirty {
                    let hint = self.cfg.sequence_for(Cmd::ForceQuit).map(|s| format!("  ({s} quits anyway)")).unwrap_or_default();
                    self.warn(format!("unsaved changes{hint}"));
                } else {
                    self.quit = true;
                }
            }
            Cmd::ForceQuit => {
                self.remove_recovery();
                self.dirty = false;
                self.quit = true;
            }
            Cmd::Undo => self.undo(),
            Cmd::Redo => self.redo(),
            Cmd::Find => self.ask(Ask::Find, String::new(), false),
            Cmd::FindNext => {
                self.find(true);
            }
            Cmd::FindPrev => {
                self.find(false);
            }
            Cmd::Replace => self.ask(Ask::Replace, String::new(), false),
            Cmd::GotoLine => self.ask(Ask::Line, String::new(), false),
            Cmd::Select => {
                if self.anchor.is_some() {
                    (self.anchor, self.sticky) = (None, false);
                } else {
                    (self.anchor, self.sticky) = (Some(self.cur), true);
                }
            }
            Cmd::SelectAll => {
                self.anchor = Some(START);
                self.cur = self.end();
                self.sticky = false;
            }
            Cmd::Copy | Cmd::Cut => {
                if let Some(text) = self.selected_text() {
                    self.copy_out(&text);
                    self.clipboard = text;
                    if cmd == Cmd::Cut {
                        self.insert("", false);
                    }
                    (self.anchor, self.sticky) = (None, false);
                }
            }
            Cmd::Paste => {
                let text = self.paste_in();
                self.insert(&text, false);
            }
            Cmd::DeleteLine => {
                let len = self.lines[row].len();
                if row + 1 < self.lines.len() {
                    self.edit(Pos { row, col: 0 }, Pos { row: row + 1, col: 0 }, "", false);
                } else if row > 0 {
                    let above = Pos { row: row - 1, col: self.lines[row - 1].len() };
                    self.edit(above, Pos { row, col: len }, "", false);
                    self.settle(Pos { row: row - 1, col: 0 });
                } else {
                    self.edit(START, Pos { row, col: len }, "", false);
                }
            }
            Cmd::DuplicateLine => {
                let (at, col) = (Pos { row, col: self.lines[row].len() }, self.cur.col);
                let copy = format!("\n{}", self.lines[row]);
                self.edit(at, at, &copy, false);
                self.settle(Pos { row: row + 1, col });
            }
            Cmd::LineUp | Cmd::LineDown => {
                let upper = if cmd == Cmd::LineUp { row.wrapping_sub(1) } else { row };
                if upper < self.lines.len() - 1 {
                    let col = self.cur.col;
                    let swapped = format!("{}\n{}", self.lines[upper + 1], self.lines[upper]);
                    let end = Pos { row: upper + 1, col: self.lines[upper + 1].len() };
                    self.edit(Pos { row: upper, col: 0 }, end, &swapped, false);
                    self.settle(Pos { row: if cmd == Cmd::LineUp { upper } else { upper + 1 }, col });
                }
            }
            Cmd::ParagraphUp => {
                self.begin_move(false);
                let mut r = row;
                while r > 0 {
                    r -= 1;
                    if self.blank(r) {
                        break;
                    }
                }
                self.cur = Pos { row: r, col: 0 };
                self.want_x = None;
            }
            Cmd::ParagraphDown => {
                self.begin_move(false);
                let mut r = row;
                while r + 1 < self.lines.len() {
                    r += 1;
                    if self.blank(r) {
                        break;
                    }
                }
                self.cur = if self.blank(r) && r != row { Pos { row: r, col: 0 } } else { self.end() };
                self.want_x = None;
            }
            Cmd::Reflow => self.reflow(),
            Cmd::WordCount => {
                let text = self.selected_text().unwrap_or_else(|| self.lines.join("\n"));
                let words = text.split_whitespace().count();
                let chars = text.chars().filter(|c| *c != '\n').count();
                self.message = Some(format!("{words} words  {chars} characters"));
            }
            Cmd::Typewriter => self.cfg.typewriter = !self.cfg.typewriter,
            Cmd::Focus => {
                // Off remembers nothing, so switching on falls back to paragraphs.
                self.cfg.focus = if self.cfg.focus == "off" { "paragraph".into() } else { "off".into() };
            }
        }
    }

    /// Re-wraps the paragraph under the cursor with real line breaks.
    fn reflow(&mut self) {
        if self.blank(self.cur.row) {
            return;
        }
        let (a, b) = self.paragraph(self.cur.row);
        let tab = self.cfg.tab_width;
        let width = if self.cfg.reflow_width > 0 { self.cfg.reflow_width } else { self.page().1 };
        let first = &self.lines[a];
        let indent = &first[..first.len() - first.trim_start().len()];
        let mut out: Vec<String> = Vec::new();
        let mut line = indent.to_string();
        for word in self.lines[a..=b].iter().flat_map(|l| l.split_whitespace()) {
            if line.len() == indent.len() {
                line.push_str(word);
            } else if text_width(&line, tab) + 1 + text_width(word, tab) > width {
                out.push(std::mem::replace(&mut line, format!("{indent}{word}")));
            } else {
                line.push(' ');
                line.push_str(word);
            }
        }
        out.push(line);
        let text = out.join("\n");
        let end = Pos { row: b, col: self.lines[b].len() };
        if text != self.text_between(Pos { row: a, col: 0 }, end) {
            self.edit(Pos { row: a, col: 0 }, end, &text, false);
        }
    }

    /// Sends the selection (or everything) through a shell command.
    fn shell(&mut self, command: &str) {
        let (s, e) = self.selection().unwrap_or((START, self.end()));
        let input = self.text_between(s, e);
        match pipe(command, &input, SHELL_LIMIT) {
            Ok(mut output) => {
                if !input.ends_with('\n') && output.ends_with('\n') {
                    output.pop();
                }
                if output != input {
                    self.edit(s, e, &output, false);
                }
            }
            Err(e) => self.warn(format!("{command}: {e}")),
        }
    }

    /// All matches of the last query in one line, as byte ranges.
    fn matches_in(&self, row: usize) -> Vec<(usize, usize)> {
        let line = &self.lines[row];
        let exact = self.query.chars().any(char::is_uppercase);
        let same = |a: char, b: char| a == b || (!exact && a.to_lowercase().eq(b.to_lowercase()));
        let mut found = Vec::new();
        for (start, _) in line.char_indices() {
            let mut text = line[start..].char_indices();
            let mut end = start;
            let hit = self.query.chars().all(|q| match text.next() {
                Some((i, c)) if same(c, q) => {
                    end = start + i + c.len_utf8();
                    true
                }
                _ => false,
            });
            if hit {
                found.push((start, end));
            }
        }
        found
    }

    /// Selects the next (or previous) match. Says so when there is none.
    fn find(&mut self, forward: bool) -> bool {
        if self.query.is_empty() {
            return false;
        }
        let n = self.lines.len();
        let from = if forward { self.cur } else { self.selection().map_or(self.cur, |s| s.0) };
        for step in 0..=n {
            let row = if forward { (from.row + step) % n } else { (from.row + n - step % n) % n };
            let hits = self.matches_in(row);
            let hit = match (forward, step == 0) {
                (true, true) => hits.into_iter().find(|h| h.0 >= from.col),
                (true, false) => hits.into_iter().next(),
                (false, true) => hits.into_iter().rfind(|h| h.0 < from.col),
                (false, false) => hits.into_iter().next_back(),
            };
            if let Some((s, e)) = hit {
                self.anchor = Some(Pos { row, col: s });
                self.cur = Pos { row, col: e };
                self.sticky = false;
                self.want_x = None;
                return true;
            }
        }
        self.warn("not found");
        false
    }

    fn replace_all(&mut self) {
        let mut count = 0;
        let replaced: Vec<String> = (0..self.lines.len())
            .map(|row| {
                let line = &self.lines[row];
                let (mut out, mut at) = (String::new(), 0);
                for (s, e) in self.matches_in(row) {
                    if s >= at {
                        out.push_str(&line[at..s]);
                        out.push_str(&self.replacement);
                        at = e;
                        count += 1;
                    }
                }
                out.push_str(&line[at..]);
                out
            })
            .collect();
        if count > 0 {
            let cur = self.cur;
            self.edit(START, self.end(), &replaced.join("\n"), false);
            self.settle(cur);
        }
        self.message = Some(format!("{count} replaced"));
    }

    fn submit(&mut self, p: Prompt) {
        match p.ask {
            Ask::Find => {
                if !p.text.is_empty() {
                    self.query = p.text;
                }
                self.find(true);
            }
            Ask::Replace => {
                if !p.text.is_empty() {
                    self.query = p.text;
                }
                if !self.query.is_empty() {
                    self.ask(Ask::ReplaceWith, String::new(), false);
                }
            }
            Ask::ReplaceWith => {
                self.replacement = p.text;
                if self.find(true) {
                    self.confirm = Some(Confirm::Replace);
                }
            }
            Ask::Line => {
                if let Ok(n) = p.text.trim().parse::<usize>() {
                    self.cur = Pos { row: n.clamp(1, self.lines.len()) - 1, col: 0 };
                    (self.anchor, self.sticky, self.want_x) = (None, false, None);
                }
            }
            Ask::SaveAs => {
                let name = p.text.trim();
                if name.is_empty() {
                    return;
                }
                let name = match (name.strip_prefix("~/"), std::env::var_os("HOME")) {
                    (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
                    _ => PathBuf::from(name),
                };
                if self.path.as_ref() != Some(&name) {
                    // A different file: forget what we knew about the old one.
                    self.remove_recovery();
                    self.path = Some(name);
                    (self.disk, self.overwrite) = (None, false);
                }
                if self.save(false) && p.then_quit {
                    self.quit = true;
                }
            }
        }
    }

    fn prompt_key(&mut self, press: Press) {
        let Some(p) = self.prompt.as_mut() else {
            return;
        };
        match press.key {
            Key::Char(c) => p.text.push(c),
            Key::Paste(t) => p.text.push_str(t.lines().next().unwrap_or("")),
            Key::Backspace => {
                p.text.pop();
            }
            Key::BackWord => p.text.clear(),
            Key::Esc => self.prompt = None,
            Key::Enter => {
                if let Some(p) = self.prompt.take() {
                    self.submit(p);
                }
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, confirm: Confirm, press: Press) {
        let answer = match press.key {
            Key::Char(c) => c.to_ascii_lowercase(),
            Key::Esc => 'q',
            _ => '?',
        };
        match (confirm, answer) {
            (Confirm::Recover(text), 'y') => {
                self.edit(START, self.end(), &text, false);
                self.settle(START);
            }
            (Confirm::Recover(_), 'n') => self.remove_recovery(),
            (Confirm::Recover(_), 'q') => {} // decide another time
            (keep @ Confirm::Recover(_), _) => self.confirm = Some(keep),
            (Confirm::Replace, 'y' | 'n') => {
                if answer == 'y' {
                    let with = self.replacement.clone();
                    self.insert(&with, false);
                }
                if self.find(true) {
                    self.confirm = Some(Confirm::Replace);
                }
            }
            (Confirm::Replace, 'a') => self.replace_all(),
            (Confirm::Replace, '?') => self.confirm = Some(Confirm::Replace),
            (Confirm::Replace, _) => {}
        }
    }

    // ========================================================================
    // 6. Key sequences and key handling
    // ========================================================================

    /// Puts held-back sequence keys into the text after all.
    fn flush_held(&mut self) {
        if self.held && !self.pending.is_empty() {
            let text = self.pending.clone();
            self.insert(&text, true);
        }
        self.held = false;
    }

    fn forget_sequence(&mut self) {
        self.flush_held();
        self.pending.clear();
    }

    /// Takes back the last `n` typed characters as if they were never typed.
    fn untype(&mut self, n: usize) {
        let line = &self.lines[self.cur.row];
        let col = line[..self.cur.col].char_indices().rev().nth(n - 1).map_or(0, |(i, _)| i);
        let start = Pos { row: self.cur.row, col };
        let mut left = self.splice(start, self.cur, "").len();
        self.cur = start;
        while left > 0 {
            let Some(top) = self.undo.last_mut().filter(|t| t.typing) else {
                break;
            };
            let take = left.min(top.inserted.len());
            top.inserted.truncate(top.inserted.len() - take);
            top.after = end_of(top.at, &top.inserted);
            left -= take;
            if top.inserted.is_empty() {
                self.undo.pop();
            }
        }
        self.dirty = self.seq_dirty;
        self.redo = std::mem::take(&mut self.seq_redo);
    }

    /// A typed character: either text, or part of a command sequence.
    ///
    /// Keys that could begin a sequence appear as text straight away; when the
    /// sequence completes they are taken back out and the command runs.
    fn type_char(&mut self, c: char) {
        let now = Instant::now();
        let limit = self.cfg.sequence_timeout;
        if limit > 0 && now.duration_since(self.last_key) > Duration::from_millis(limit) {
            self.forget_sequence();
        }
        self.last_key = now;

        // Longest tail of what was typed that is still the start of a sequence.
        let mut candidate = format!("{}{c}", self.pending);
        let mut shifted = false;
        while !candidate.is_empty() && !self.cfg.keys.iter().any(|(s, _)| s.starts_with(&candidate)) {
            candidate.remove(0);
            shifted = true;
        }
        if shifted {
            // Earlier keys turned out to be plain text and stay in the file.
            self.flush_held();
            self.seq_dirty = true;
            self.seq_redo.clear();
        }
        let count = candidate.chars().count();
        if count == 1 {
            // A sequence may start here. With a selection, hold the keys back
            // so the selection survives for commands like copy.
            self.held = self.anchor.is_some();
            self.seq_dirty = self.dirty;
        }

        if let Some(action) = self.cfg.keys.iter().find(|(s, _)| *s == candidate).map(|(_, a)| a.clone()) {
            if !self.held && count > 1 {
                self.untype(count - 1);
            }
            self.held = false;
            self.pending.clear();
            self.act(action);
            return;
        }
        if candidate.is_empty() {
            self.pending.clear();
            self.insert(&c.to_string(), true);
            return;
        }
        if !self.held {
            if count == 1 {
                self.seq_redo = std::mem::take(&mut self.redo);
            }
            self.insert(&c.to_string(), true);
        }
        self.pending = candidate;
    }

    fn key(&mut self, press: Press) {
        self.message = None;
        self.last_input = Instant::now();
        if let Some(confirm) = self.confirm.take() {
            return self.confirm_key(confirm, press);
        }
        if self.prompt.is_some() {
            return self.prompt_key(press);
        }
        if let Key::Char(c) = press.key {
            return self.type_char(c);
        }
        self.forget_sequence();
        match press.key {
            Key::Paste(text) => self.insert(&text, false),
            Key::Enter => {
                let line = &self.lines[self.cur.row][..self.cur.col];
                let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
                let text = if self.cfg.auto_indent { format!("\n{}", &line[..indent]) } else { "\n".into() };
                self.insert(&text, true);
            }
            Key::Tab => {
                if self.cfg.soft_tabs {
                    let done = self.lines[self.cur.row][..self.cur.col].chars().count();
                    self.insert(&" ".repeat(self.cfg.tab_width - done % self.cfg.tab_width), true);
                } else {
                    self.insert("\t", true);
                }
            }
            Key::Backspace | Key::BackWord | Key::Delete => {
                let (s, e) = self.selection().unwrap_or_else(|| match press.key {
                    Key::Backspace => (self.left_of(self.cur), self.cur),
                    Key::BackWord => (self.word_left(self.cur), self.cur),
                    _ if press.ctrl => (self.cur, self.word_right(self.cur)),
                    _ => (self.cur, self.right_of(self.cur)),
                });
                self.edit(s, e, "", false);
            }
            Key::Esc => (self.anchor, self.sticky) = (None, false),
            Key::Click(x, y) => {
                (self.anchor, self.sticky, self.want_x) = (None, false, None);
                self.cur = self.pos_at(x, y);
            }
            Key::Drag(x, y) => {
                self.anchor.get_or_insert(self.cur);
                self.cur = self.pos_at(x, y);
                self.want_x = None;
            }
            Key::Ignore | Key::Char(_) => {}
            key => self.navigate(&key, press.shift, press.ctrl),
        }
    }

    // ---- things that happen by themselves ------------------------------------

    fn held_due(&self) -> Option<Duration> {
        let limit = Duration::from_millis(self.cfg.sequence_timeout);
        (self.held && !self.pending.is_empty() && !limit.is_zero()).then(|| limit.saturating_sub(self.last_key.elapsed()))
    }

    fn autosave_due(&self) -> Option<Duration> {
        let wanted = self.cfg.autosave
            && self.dirty
            && self.path.is_some()
            && self.edits != self.autosave_tried
            && self.prompt.is_none()
            && self.confirm.is_none();
        wanted.then(|| Duration::from_secs(self.cfg.autosave_delay).saturating_sub(self.last_input.elapsed()))
    }

    fn recovery_due(&self) -> Option<Duration> {
        let wanted = self.cfg.recovery && self.dirty && self.edits != self.recovered_edits;
        wanted.then(|| Duration::from_secs(self.cfg.recovery_interval).saturating_sub(self.last_recovery.elapsed()))
    }

    /// How long the main loop may sleep before something needs doing.
    fn next_wake(&self) -> Option<Duration> {
        [self.held_due(), self.autosave_due(), self.recovery_due()].into_iter().flatten().min()
    }

    fn tick(&mut self) {
        if self.held_due().is_some_and(|d| d.is_zero()) {
            self.forget_sequence();
        }
        if self.autosave_due().is_some_and(|d| d.is_zero()) {
            self.autosave_tried = self.edits;
            self.save(true);
        }
        if self.recovery_due().is_some_and(|d| d.is_zero()) {
            self.write_recovery();
        }
    }

    // ========================================================================
    // 7. Files, recovery, clipboard
    // ========================================================================

    fn save(&mut self, auto: bool) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let on_disk = stamp(&path);
        if self.cfg.check_disk && !self.overwrite && on_disk.is_some() && on_disk != self.disk {
            let what = if self.disk.is_some() { "file changed on disk" } else { "file already exists" };
            self.overwrite = !auto;
            self.warn(format!("{what} — save again to overwrite"));
            return false;
        }
        if self.cfg.final_newline && self.lines.last().is_some_and(|l| !l.is_empty()) {
            self.lines.push(String::new());
        }
        let data = self.lines.join(if self.crlf { "\r\n" } else { "\n" });
        match write_file(&path, data.as_bytes()) {
            Ok(()) => {
                self.dirty = false;
                self.overwrite = false;
                self.disk = stamp(&path);
                self.remove_recovery();
                true
            }
            Err(e) => {
                self.warn(format!("cannot save {}: {e}", path.display()));
                false
            }
        }
    }

    /// Where unsaved text is parked: a hidden file beside the real one.
    fn recovery_path(&self) -> Option<PathBuf> {
        match &self.path {
            Some(p) => Some(p.with_file_name(format!(".{}.zen-recover", p.file_name()?.to_string_lossy()))),
            None => Some(state_dir()?.join("unnamed.zen-recover")),
        }
    }

    fn write_recovery(&mut self) {
        self.recovered_edits = self.edits;
        self.last_recovery = Instant::now();
        if let Some(path) = self.recovery_path() {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(path, self.lines.join("\n"));
        }
    }

    fn remove_recovery(&mut self) {
        if let Some(path) = self.recovery_path() {
            let _ = fs::remove_file(path);
        }
        self.recovered_edits = self.edits;
    }

    fn offer_recovery(&mut self) {
        if !self.cfg.recovery {
            return;
        }
        let Some(text) = self.recovery_path().and_then(|p| fs::read_to_string(p).ok()) else {
            return;
        };
        if text == self.lines.join("\n") {
            self.remove_recovery();
        } else {
            self.confirm = Some(Confirm::Recover(text));
        }
    }

    fn positions_file() -> Option<PathBuf> {
        Some(state_dir()?.join("positions"))
    }

    fn restore_position(&mut self) {
        if !self.cfg.remember_position {
            return;
        }
        let Some(path) = self.path.as_ref().and_then(|p| fs::canonicalize(p).ok()) else {
            return;
        };
        let saved = Self::positions_file().and_then(|f| fs::read_to_string(f).ok()).unwrap_or_default();
        for line in saved.lines() {
            let mut parts = line.splitn(3, '\t');
            let (row, col, name) = (parts.next(), parts.next(), parts.next());
            if name.map(Path::new) == Some(path.as_path()) {
                let number = |s: Option<&str>| s.and_then(|s| s.parse().ok()).unwrap_or(0);
                self.cur = self.clamp(Pos { row: number(row), col: number(col) });
            }
        }
    }

    fn remember_position(&self) {
        if !self.cfg.remember_position {
            return;
        }
        let (Some(path), Some(file)) = (self.path.as_ref().and_then(|p| fs::canonicalize(p).ok()), Self::positions_file()) else {
            return;
        };
        let name = path.to_string_lossy();
        let saved = fs::read_to_string(&file).unwrap_or_default();
        let mut entries: Vec<&str> = saved.lines().filter(|l| l.splitn(3, '\t').nth(2) != Some(&*name)).collect();
        let entry = format!("{}\t{}\t{name}", self.cur.row, self.cur.col);
        entries.push(&entry);
        let keep = entries.len().saturating_sub(500);
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(file, entries[keep..].join("\n") + "\n");
    }

    /// Programs to try for the desktop clipboard, best first.
    fn clipboard_commands(&self, copy: bool) -> Vec<String> {
        let custom = if copy { &self.cfg.copy_command } else { &self.cfg.paste_command };
        if !custom.is_empty() {
            return vec![custom.clone()];
        }
        let set = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
        let mut found = Vec::new();
        if set("WAYLAND_DISPLAY") {
            found.push(if copy { "wl-copy" } else { "wl-paste --no-newline --type text" });
        }
        if set("DISPLAY") {
            found.push(if copy { "xclip -selection clipboard" } else { "xclip -selection clipboard -o" });
            found.push(if copy { "xsel -ib" } else { "xsel -ob" });
        }
        found.into_iter().map(String::from).collect()
    }

    fn copy_out(&self, text: &str) {
        if !self.cfg.system_clipboard {
            return;
        }
        // Ask the terminal as well; it works over SSH where no program can.
        put(&format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
        for command in self.clipboard_commands(true) {
            let mut tool = Command::new("sh");
            tool.args(["-c", &command]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
            // Clipboard programs stay behind to hand the text out. Cut them
            // loose from this terminal so closing it does not take them, and
            // the copied text, down with it.
            unsafe {
                tool.pre_exec(|| {
                    setsid();
                    Ok(())
                });
            }
            let Ok(mut child) = tool.spawn() else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            // Most of these hand over to a background copy of themselves and
            // return at once. One that is still here after the limit is taken
            // to be serving the clipboard itself, and is left to it.
            let started = Instant::now();
            let taken = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status.success(),
                    Ok(None) if started.elapsed() > CLIPBOARD_LIMIT => break true,
                    Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                    Err(_) => break false,
                }
            };
            if taken {
                break;
            }
        }
    }

    fn paste_in(&self) -> String {
        if self.cfg.system_clipboard {
            for command in self.clipboard_commands(false) {
                match pipe(&command, "", CLIPBOARD_LIMIT) {
                    Ok(text) if !text.is_empty() => return text.replace("\r\n", "\n"),
                    // Not installed: try the next program.
                    Err(e) if e == NOT_FOUND => continue,
                    // It ran and had no text for us (an image, say, or nothing
                    // at all). Asking another program could turn up something
                    // stale, so stop here and use zen's own clipboard.
                    _ => break,
                }
            }
        }
        self.clipboard.clone()
    }

    // ========================================================================
    // 8. Drawing
    // ========================================================================

    /// The screen row `back` rows above row `idx` of line `row`, and how many
    /// rows were missing because the text starts before that.
    fn rows_back(&self, mut row: usize, mut idx: usize, mut back: usize) -> ((usize, usize), usize) {
        loop {
            if back <= idx {
                return ((row, idx - back), 0);
            }
            if row == 0 {
                return ((0, 0), back - idx);
            }
            back -= idx + 1;
            row -= 1;
            idx = self.wrapped(row).len() - 1;
        }
    }

    /// Scrolls to keep the cursor on the page; returns its row on the page.
    fn scroll(&mut self, height: usize) -> usize {
        let cur_idx = row_of(&self.wrapped(self.cur.row), self.cur.col);
        if self.cfg.typewriter {
            // The cursor's row never moves; the text slides under it.
            let target = (height * self.cfg.typewriter_position / 100).min(height - 1);
            (self.top, self.lead) = self.rows_back(self.cur.row, cur_idx, target);
            return target;
        }
        self.lead = 0;
        self.top.0 = self.top.0.min(self.lines.len() - 1);
        self.top.1 = self.top.1.min(self.wrapped(self.top.0).len() - 1);
        if (self.cur.row, cur_idx) < self.top {
            self.top = (self.cur.row, cur_idx);
            return 0;
        }
        // Screen rows between the top of the page and the cursor.
        let mut dist = 0;
        if self.top.0 == self.cur.row {
            dist = cur_idx - self.top.1;
        } else {
            dist += self.wrapped(self.top.0).len() - self.top.1;
            for row in self.top.0 + 1..self.cur.row {
                if dist >= height {
                    break;
                }
                dist += self.wrapped(row).len();
            }
            dist += cur_idx;
        }
        if dist < height {
            return dist;
        }
        // Too far down: put the cursor on the bottom row.
        self.top = self.rows_back(self.cur.row, cur_idx, height - 1).0;
        height - 1
    }

    /// The stretch of text that stays bright in focus mode.
    fn focus_range(&self) -> Option<(Pos, Pos)> {
        let row = self.cur.row;
        let line_only = (Pos { row, col: 0 }, Pos { row: row + 1, col: 0 });
        match self.cfg.focus.as_str() {
            "line" => Some(line_only),
            "paragraph" | "sentence" if self.blank(row) => Some(line_only),
            "paragraph" => {
                let (a, b) = self.paragraph(row);
                Some((Pos { row: a, col: 0 }, Pos { row: b + 1, col: 0 }))
            }
            "sentence" => {
                let (a, b) = self.paragraph(row);
                let (mut start, mut end) = (Pos { row: a, col: 0 }, Pos { row: b + 1, col: 0 });
                'scan: for r in a..=b {
                    let mut chars = self.lines[r].char_indices().peekable();
                    while let Some((i, c)) = chars.next() {
                        let closes = matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|(_, next)| next.is_whitespace());
                        if closes {
                            let stop = Pos { row: r, col: i + 1 };
                            if stop < self.cur {
                                start = stop;
                            } else {
                                end = stop;
                                break 'scan;
                            }
                        }
                    }
                }
                Some((start, end))
            }
            _ => None,
        }
    }

    fn draw_row(&self, out: &mut String, row: usize, (s, e): (usize, usize), last: bool, focus: Option<(Pos, Pos)>, dim: &(String, String)) {
        let within = |range: Option<(Pos, Pos)>, col: usize| range.is_some_and(|(a, b)| Pos { row, col } >= a && Pos { row, col } < b);
        let sel = self.selection();
        let (mut x, mut lit, mut faded) = (0, false, false);
        for (i, c) in self.lines[row][s..e].char_indices() {
            if (focus.is_some() && !within(focus, s + i)) != faded {
                faded = !faded;
                out.push_str(if faded { &dim.0 } else { &dim.1 });
            }
            if within(sel, s + i) != lit {
                lit = !lit;
                out.push_str(if lit { "\x1b[7m" } else { "\x1b[27m" });
            }
            let w = char_width(c, x, self.cfg.tab_width);
            match c {
                '\t' => out.push_str(&" ".repeat(w)),
                '\0'..='\x1f' => out.push(char::from_u32(0x2400 + c as u32).unwrap_or(' ')),
                '\x7f' => out.push('\u{2421}'),
                _ => out.push(c),
            }
            x += w;
        }
        if last && within(sel, e) {
            out.push_str("\x1b[7m \x1b[27m"); // a selected line break
        } else if lit {
            out.push_str("\x1b[27m");
        }
        if faded {
            out.push_str(&dim.1);
        }
    }

    fn draw(&mut self) {
        let (left, _, top, height) = self.page();
        let cursor_row = self.scroll(height);
        let colour = |code: u8, rgb: Option<Rgb>| rgb.map(|(r, g, b)| format!("\x1b[{code};2;{r};{g};{b}m"));
        let fg = colour(38, self.cfg.foreground);
        let dim = match colour(38, self.cfg.dim_color) {
            Some(on) => (on, fg.clone().unwrap_or_else(|| "\x1b[39m".into())),
            None => ("\x1b[2m".to_string(), "\x1b[22m".to_string()),
        };
        let mut out = String::from("\x1b[?2026h\x1b[?25l\x1b[0m");
        out.push_str(&fg.unwrap_or_default());
        out.push_str(&colour(48, self.cfg.background).unwrap_or_default());

        let focus = self.focus_range();
        let pad = " ".repeat(left);
        let (mut row, mut idx) = self.top;
        let mut rows = self.wrapped(row);
        for y in 0..self.rows {
            out.push_str(&format!("\x1b[{};1H", y + 1));
            if y >= top + self.lead && y < top + height && row < self.lines.len() {
                out.push_str(&pad);
                self.draw_row(&mut out, row, rows[idx], idx + 1 == rows.len(), focus, &dim);
                idx += 1;
                if idx == rows.len() {
                    row += 1;
                    idx = 0;
                    if row < self.lines.len() {
                        rows = self.wrapped(row);
                    }
                }
            }
            out.push_str("\x1b[K");
        }

        let cur_rows = self.wrapped(self.cur.row);
        let seg = cur_rows[row_of(&cur_rows, self.cur.col)];
        let x = text_width(&self.lines[self.cur.row][seg.0..self.cur.col], self.cfg.tab_width);
        let mut cursor = (top + cursor_row + 1, left + x + 1);

        // The bottom row is borrowed only while something needs saying or asking.
        let question = match &self.confirm {
            Some(Confirm::Recover(_)) => Some("unsaved text from last time found — recover it?  y / n"),
            Some(Confirm::Replace) => Some("replace?  y = yes  n = skip  a = all  q = stop"),
            None => None,
        };
        if let Some(text) = question.or(self.message.as_deref()) {
            let shown: String = text.chars().take(self.cols.saturating_sub(left + 1)).collect();
            out.push_str(&format!("\x1b[{};1H{pad}\x1b[2m{shown}\x1b[22m\x1b[K", self.rows));
        } else if let Some(p) = &self.prompt {
            let label = match p.ask {
                Ask::Find => "find: ",
                Ask::Replace => "replace: ",
                Ask::ReplaceWith => "with: ",
                Ask::SaveAs => "save as: ",
                Ask::Line => "line: ",
            };
            let room = self.cols.saturating_sub(left + label.len() + 2);
            let mut shown: Vec<char> = p.text.chars().rev().take(room).collect();
            shown.reverse();
            out.push_str(&format!("\x1b[{};1H{pad}{label}", self.rows));
            out.extend(shown.iter());
            out.push_str("\x1b[K");
            cursor = (self.rows, left + label.len() + shown.len() + 1);
        }
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h\x1b[?2026l", cursor.0, cursor.1));
        put(&out);
    }
}

/// Modification time and size, used to notice changes made by other programs.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Writes through a temporary file so a failed save never destroys the original.
fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.zen-tmp"));
    let attempt = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &target)
    })();
    if attempt.is_err() {
        let _ = fs::remove_file(&tmp);
        fs::write(&target, data)?; // e.g. the directory is read-only but the file is not
    }
    Ok(())
}

/// What `pipe` reports when the shell could not find the program.
const NOT_FOUND: &str = "program not found";
/// How long a clipboard program may take before zen stops waiting for it.
const CLIPBOARD_LIMIT: Duration = Duration::from_secs(2);
/// How long a `[shell]` command may run.
const SHELL_LIMIT: Duration = Duration::from_secs(30);

/// Runs a shell command with `input` on its stdin and returns what it prints.
/// A command still running after `limit` is killed, so a stuck program can
/// never freeze the editor.
fn pipe(command: &str, input: &str, limit: Duration) -> Result<String, String> {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0) // its own group, so everything it starts can be stopped together
        .spawn()
        .map_err(|e| e.to_string())?;
    // Feed and drain on other threads so a full pipe cannot stall us.
    let mut stdin = child.stdin.take();
    let data = input.as_bytes().to_vec();
    std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(&data);
        }
    });
    let collect = |stream: Option<Box<dyn Read + Send>>| {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut bytes);
            }
            let _ = send.send(bytes);
        });
        receive
    };
    let stdout = collect(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let stderr = collect(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));

    let started = Instant::now();
    let stop = |child: &mut process::Child| {
        unsafe {
            kill(-(child.id() as i32), 9);
        }
        let _ = child.wait();
        Err("took too long and was stopped".to_string())
    };
    // The output has to be read while the command runs, or a large one would block it.
    let printed = loop {
        match stdout.recv_timeout(Duration::from_millis(20)) {
            Ok(bytes) => break bytes,
            Err(_) if started.elapsed() > limit => return stop(&mut child),
            Err(_) => {}
        }
    };
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > limit => return stop(&mut child),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    if status.code() == Some(127) {
        return Err(NOT_FOUND.to_string());
    }
    if !status.success() {
        let complaint = stderr.recv_timeout(Duration::from_millis(200)).unwrap_or_default();
        let complaint = String::from_utf8_lossy(&complaint);
        return Err(complaint.lines().next().unwrap_or("failed").to_string());
    }
    String::from_utf8(printed).ok().filter(|text| !text.contains('\0')).ok_or_else(|| "output is not text".to_string())
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| n << 8 | *b as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

const HELP: &str = "\
zen — a text editor with nothing in it but a cursor

usage: zen [options] [file]
       some-command | zen [file]

  -c, --config <path>   use this config file
      --print-config    print the default config (a starting point for your own)
  -h, --help            show this help
  -V, --version         show the version

Commands are typed as short key sequences (defaults: ;w save, ;q quit,
;x save and quit, ;Q quit without saving). Every sequence and every feature
can be changed in ~/.config/zen/config.toml; see `man zen`.";

fn fail(message: &str) -> ! {
    eprintln!("zen: {message}");
    process::exit(1);
}

fn main() {
    let mut file = None;
    let mut config_path = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => return println!("{HELP}"),
            Some("-V" | "--version") => return println!("zen {}", env!("CARGO_PKG_VERSION")),
            Some("--print-config") => return print!("{DEFAULT_CONFIG}"),
            Some("-c" | "--config") => match args.next() {
                Some(p) => config_path = Some(PathBuf::from(p)),
                None => fail("--config needs a path"),
            },
            Some(s) if s.starts_with('-') && s.len() > 1 => fail(&format!("unknown option {s} (try --help)")),
            _ if file.is_some() => fail("only one file at a time"),
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    let cfg = Config::load(config_path).unwrap_or_else(|e| fail(&e));

    // Text piped in becomes the buffer; the keyboard is then read from the terminal.
    let mut piped = None;
    if unsafe { isatty(0) } == 0 {
        let mut text = String::new();
        if io::stdin().read_to_string(&mut text).is_err() {
            fail("the piped input is not UTF-8 text");
        }
        let tty = fs::File::open("/dev/tty").unwrap_or_else(|_| fail("zen needs to run in a terminal"));
        unsafe {
            dup2(tty.as_raw_fd(), 0);
        }
        piped = Some(text);
    }

    let mut editor = Editor::new(cfg, file, piped).unwrap_or_else(|e| fail(&e));
    if let Err(e) = term_enter(&editor.cfg) {
        fail(&e);
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term_leave();
        hook(info);
    }));

    let mut input = Input::default();
    while !editor.quit {
        editor.tick();
        editor.draw();
        let wait = editor.next_wake().map(|d| d + Duration::from_millis(1));
        match input.next(wait) {
            Event::Press(press) => {
                editor.key(press);
                while !editor.quit {
                    match input.buffered() {
                        Some(press) => editor.key(press),
                        None => break,
                    }
                }
            }
            Event::Resize => (editor.rows, editor.cols) = term_size(),
            Event::Timeout => {}
            Event::Closed => break,
        }
    }
    // Killed or the terminal went away: park unsaved text for next time.
    if editor.dirty && editor.cfg.recovery {
        editor.write_recovery();
    }
    editor.remember_position();
    term_leave();
}
