//! The original DOOM engine (id Software, via doomgeneric) running in a terminal.
//!
//! The C engine renders into a 640x400 XRGB buffer; this frontend downsamples it to
//! truecolor half-blocks (▀) and feeds keyboard events back to the engine.

use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::ResetColor,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

const RESX: usize = 640;
const RESY: usize = 400;

// doomkeys.h
const KEY_RIGHTARROW: u8 = 0xae;
const KEY_LEFTARROW: u8 = 0xac;
const KEY_UPARROW: u8 = 0xad;
const KEY_DOWNARROW: u8 = 0xaf;
const KEY_STRAFE_L: u8 = 0xa0;
const KEY_STRAFE_R: u8 = 0xa1;
const KEY_USE: u8 = 0xa2;
const KEY_FIRE: u8 = 0xa3;
const KEY_ESCAPE: u8 = 27;
const KEY_ENTER: u8 = 13;
const KEY_TAB: u8 = 9;
const KEY_BACKSPACE: u8 = 0x7f;

unsafe extern "C" {
    static mut DG_ScreenBuffer: *mut u32;
    fn doomgeneric_Create(argc: c_int, argv: *mut *mut c_char);
    fn doomgeneric_Tick();

    fn dup(fd: c_int) -> c_int;
    fn dup2(src: c_int, dst: c_int) -> c_int;
    fn atexit(cb: extern "C" fn()) -> c_int;
    fn fflush(stream: *mut c_void) -> c_int;
}

/// The real terminal, saved before fd 1/2 are redirected to the log (the engine printf()s a lot).
static TTY_FD: AtomicI32 = AtomicI32::new(-1);
static ENHANCED: AtomicBool = AtomicBool::new(false);
static RESTORED: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static TERM: Mutex<Option<Term>> = Mutex::new(None);

fn log_path() -> PathBuf {
    std::env::temp_dir().join("doom-terminal.log")
}

struct Term {
    tty: File,
    enhanced: bool,
    start: Instant,
    queue: VecDeque<(bool, u8)>,
    /// doom key -> time at which we synthesize its release
    held: HashMap<u8, Instant>,
    size: (usize, usize),
    prev: Vec<(u32, u32)>,
    fb: Vec<u32>,
    out: Vec<u8>,
}

/// A terminal key can drive both a game action and the raw character (menus, cheats like IDDQD).
fn map_key(code: KeyCode) -> Vec<u8> {
    match code {
        KeyCode::Up => vec![KEY_UPARROW],
        KeyCode::Down => vec![KEY_DOWNARROW],
        KeyCode::Left => vec![KEY_LEFTARROW],
        KeyCode::Right => vec![KEY_RIGHTARROW],
        KeyCode::Enter => vec![KEY_ENTER],
        KeyCode::Esc => vec![KEY_ESCAPE],
        KeyCode::Tab => vec![KEY_TAB],
        KeyCode::Backspace => vec![KEY_BACKSPACE],
        KeyCode::F(n @ 1..=10) => vec![0x80 + 0x3a + n],
        KeyCode::F(11) => vec![0x80 + 0x57],
        KeyCode::F(12) => vec![0x80 + 0x58],
        KeyCode::Char(c) if c.is_ascii() => {
            let c = c.to_ascii_lowercase() as u8;
            let action = match c {
                b'w' | b'z' => Some(KEY_UPARROW),
                b's' => Some(KEY_DOWNARROW),
                b'a' | b'q' => Some(KEY_STRAFE_L),
                b'd' => Some(KEY_STRAFE_R),
                b'e' | b' ' => Some(KEY_USE),
                b'f' | b'k' => Some(KEY_FIRE),
                _ => None,
            };
            action.into_iter().chain([c]).collect()
        }
        _ => vec![],
    }
}

impl Term {
    fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            QUIT.store(true, Ordering::SeqCst);
            return;
        }
        let now = Instant::now();
        for key in map_key(k.code) {
            match k.kind {
                KeyEventKind::Press | KeyEventKind::Repeat => {
                    // Legacy terminals never report releases: fake them with a timeout that
                    // bridges the OS key-repeat delay on first press.
                    let ms = if self.enhanced {
                        1000
                    } else if self.held.contains_key(&key) {
                        130
                    } else {
                        450
                    };
                    self.held.insert(key, now + Duration::from_millis(ms));
                    self.queue.push_back((true, key));
                }
                KeyEventKind::Release => {
                    if self.held.remove(&key).is_some() {
                        self.queue.push_back((false, key));
                    }
                }
            }
        }
    }

    fn pump(&mut self) {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) => self.on_key(k),
                Ok(Event::Resize(..)) => self.size = (0, 0),
                _ => {}
            }
        }
        let now = Instant::now();
        let expired: Vec<u8> = self.held.iter().filter(|(_, t)| now >= **t).map(|(k, _)| *k).collect();
        for key in expired {
            self.held.remove(&key);
            self.queue.push_back((false, key));
        }
    }

    fn draw(&mut self, src: &[u32]) {
        let (cols, rows) = terminal::size().map_or((80, 24), |(c, r)| (c as usize, r as usize));
        self.out.clear();
        self.out.extend_from_slice(b"\x1b[?2026h");
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.out.extend_from_slice(b"\x1b[0m\x1b[2J");
            self.prev = vec![(u32::MAX, u32::MAX); cols * rows];
        }

        // Fit a 4:3 picture (Doom's intended aspect) into cols x 2*rows square pixels.
        let (pw, ph) = (cols, rows * 2);
        let (w, h) = if pw * 3 / 4 <= ph { (pw, pw * 3 / 4) } else { (ph * 4 / 3, ph) };
        let h = h & !1;
        let (ox, oy) = ((pw - w) / 2, (rows - h / 2) / 2);

        // box-filter downsample
        self.fb.resize(w * h, 0);
        for y in 0..h {
            let (sy0, sy1) = (y * RESY / h, ((y + 1) * RESY / h).max(y * RESY / h + 1));
            for x in 0..w {
                let (sx0, sx1) = (x * RESX / w, ((x + 1) * RESX / w).max(x * RESX / w + 1));
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for sy in sy0..sy1 {
                    for &p in &src[sy * RESX + sx0..sy * RESX + sx1] {
                        r += (p >> 16) & 255;
                        g += (p >> 8) & 255;
                        b += p & 255;
                    }
                }
                let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
                self.fb[y * w + x] = ((r / n) << 16) | ((g / n) << 8) | (b / n);
            }
        }

        // emit only changed cells
        let out = &mut self.out;
        let (mut cur_fg, mut cur_bg) = (u32::MAX, u32::MAX);
        for r in 0..h / 2 {
            let mut cursor_ok = false;
            for c in 0..w {
                let top = self.fb[2 * r * w + c] & 0xFCFCFC;
                let bot = self.fb[(2 * r + 1) * w + c] & 0xFCFCFC;
                let idx = (r + oy) * cols + c + ox;
                if self.prev[idx] == (top, bot) {
                    cursor_ok = false;
                    continue;
                }
                self.prev[idx] = (top, bot);
                if !cursor_ok {
                    let _ = write!(out, "\x1b[{};{}H", r + oy + 1, c + ox + 1);
                    cursor_ok = true;
                }
                if bot != cur_bg {
                    let _ = write!(out, "\x1b[48;2;{};{};{}m", bot >> 16, (bot >> 8) & 255, bot & 255);
                    cur_bg = bot;
                }
                if top == bot {
                    out.push(b' ');
                } else {
                    if top != cur_fg {
                        let _ = write!(out, "\x1b[38;2;{};{};{}m", top >> 16, (top >> 8) & 255, top & 255);
                        cur_fg = top;
                    }
                    out.extend_from_slice("▀".as_bytes());
                }
            }
        }
        out.extend_from_slice(b"\x1b[0m\x1b[?2026l");
        let _ = self.tty.write_all(out);
        let _ = self.tty.flush();
    }
}

fn with_term<R>(f: impl FnOnce(&mut Term) -> R) -> Option<R> {
    TERM.lock().ok()?.as_mut().map(f)
}

// ---------------------------------------------------------------- doomgeneric platform API

#[unsafe(no_mangle)]
pub extern "C" fn DG_Init() {}

#[unsafe(no_mangle)]
pub extern "C" fn DG_DrawFrame() {
    let ptr = unsafe { std::ptr::addr_of!(DG_ScreenBuffer).read() };
    if ptr.is_null() {
        return;
    }
    let src = unsafe { std::slice::from_raw_parts(ptr, RESX * RESY) };
    with_term(|t| t.draw(src));
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_SleepMs(ms: u32) {
    std::thread::sleep(Duration::from_millis(ms as u64));
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    with_term(|t| t.start.elapsed().as_millis() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_GetKey(pressed: *mut c_int, key: *mut u8) -> c_int {
    let ev = with_term(|t| {
        if t.queue.is_empty() {
            t.pump();
        }
        t.queue.pop_front()
    })
    .flatten();
    match ev {
        Some((down, k)) => {
            unsafe {
                *pressed = down as c_int;
                *key = k;
            }
            1
        }
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_SetWindowTitle(title: *const c_char) {
    if title.is_null() {
        return;
    }
    let title = unsafe { CStr::from_ptr(title) }.to_string_lossy().into_owned();
    with_term(|t| {
        let _ = write!(t.tty, "\x1b]0;{title}\x07");
    });
}

// ---------------------------------------------------------------- setup / teardown

/// Idempotent: runs from main, from atexit (the engine calls exit() on quit/error) and on panic.
fn restore() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let fd = TTY_FD.load(Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    let mut tty = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    if ENHANCED.load(Ordering::SeqCst) {
        let _ = execute!(tty, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(tty, ResetColor, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();

    // Surface engine errors (I_Error) that went to the log.
    unsafe { fflush(std::ptr::null_mut()) };
    if let Ok(log) = std::fs::read_to_string(log_path())
        && let Some(pos) = log.rfind("Error")
    {
        let _ = writeln!(tty, "{}\r\n(log complet : {})", log[pos..].trim(), log_path().display());
    }
}

extern "C" fn on_exit() {
    restore();
}

fn find_iwad() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("wad/doom1.wad"), PathBuf::from("doom1.wad")];
    if let Ok(dir) = std::env::var("DOOMWADDIR") {
        candidates.push(Path::new(&dir).join("doom1.wad"));
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("wad/doom1.wad"));
    candidates.into_iter().find(|p| p.is_file())
}

fn main() -> io::Result<()> {
    let mut args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a.eq_ignore_ascii_case("-iwad")) {
        let Some(iwad) = find_iwad() else {
            eprintln!("doom1.wad introuvable : place-le dans ./wad/ ou passe -iwad <fichier.wad>");
            std::process::exit(1);
        };
        args.extend(["-iwad".to_string(), iwad.to_string_lossy().into_owned()]);
    }

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide, terminal::Clear(ClearType::All))?;
    let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
    }
    stdout.flush()?;

    // Keep the real terminal on a private fd, send the engine's stdout/stderr to a log file.
    let tty_fd = unsafe { dup(1) };
    if tty_fd < 0 {
        restore();
        return Err(io::Error::last_os_error());
    }
    TTY_FD.store(tty_fd, Ordering::SeqCst);
    ENHANCED.store(enhanced, Ordering::SeqCst);
    let log = File::create(log_path())?;
    unsafe {
        dup2(log.as_raw_fd(), 1);
        dup2(log.as_raw_fd(), 2);
        atexit(on_exit);
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default_hook(info);
    }));

    *TERM.lock().unwrap() = Some(Term {
        tty: unsafe { File::from_raw_fd(dup(tty_fd)) },
        enhanced,
        start: Instant::now(),
        queue: VecDeque::new(),
        held: HashMap::new(),
        size: (0, 0),
        prev: Vec::new(),
        fb: Vec::new(),
        out: Vec::with_capacity(1 << 20),
    });

    let cargs: Vec<CString> = args.into_iter().map(|a| CString::new(a).unwrap()).collect();
    let mut argv: Vec<*mut c_char> = cargs.iter().map(|a| a.as_ptr() as *mut c_char).collect();
    argv.push(std::ptr::null_mut());
    unsafe { doomgeneric_Create(cargs.len() as c_int, argv.as_mut_ptr()) };

    while !QUIT.load(Ordering::SeqCst) {
        unsafe { doomgeneric_Tick() };
    }
    restore();
    std::process::exit(0);
}
