//! Interactive `furmi ls --watch`: a live task list with a cursor and one-key
//! actions, for when a terminal is all you have (say, over SSH). It calls the
//! same `control::` functions as `furmi done|fail|cancel|retry|logs|attach`.
//! Raw mode comes from termios through `libc`, so there is no TUI dependency.

use std::io::Write;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::cmd::{fmt_time, load_tasks, table_lines, task_rows};
use crate::config::Config;
use crate::control;
use crate::home::Home;
use crate::task::now;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Tab,
    Esc,
    Quit,
    Char(char),
}

/// Decodes the bytes of one terminal read into keys. Escape sequences we
/// don't use (function keys, Home, ...) are swallowed whole so their
/// letters don't trigger actions.
pub fn parse_keys(buf: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x03 => keys.push(Key::Quit),
            b'\t' => keys.push(Key::Tab),
            b'\r' | b'\n' => keys.push(Key::Enter),
            0x1b => match buf.get(i + 1) {
                Some(b'[') => {
                    let mut j = i + 2;
                    while buf.get(j).is_some_and(|b| (0x20..=0x3f).contains(b)) {
                        j += 1;
                    }
                    match buf.get(j) {
                        Some(b'A') => keys.push(Key::Up),
                        Some(b'B') => keys.push(Key::Down),
                        _ => {}
                    }
                    i = j;
                }
                Some(b'O') => {
                    match buf.get(i + 2) {
                        Some(b'A') => keys.push(Key::Up),
                        Some(b'B') => keys.push(Key::Down),
                        _ => {}
                    }
                    i += 2;
                }
                _ => keys.push(Key::Esc),
            },
            b if b.is_ascii_graphic() || b == b' ' => keys.push(Key::Char(b as char)),
            _ => {}
        }
        i += 1;
    }
    keys
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Done,
    Fail,
    Cancel,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Done => "Finish",
            Action::Fail => "Fail",
            Action::Cancel => "Cancel",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    Quit,
    ToggleArchive,
    Logs(String),
    Attach(String),
    Retry(String),
    Run(Action, String),
}

/// What the screen shows and what the cursor is on.
#[derive(Default)]
pub struct Ui {
    pub ids: Vec<String>,
    pub selected: usize,
    pub pending: Option<(Action, String)>,
    pub message: Option<String>,
}

impl Ui {
    /// Replaces the task list, keeping the cursor on the same task if it's
    /// still there.
    pub fn set_ids(&mut self, ids: Vec<String>) {
        let keep = self.current().cloned();
        self.ids = ids;
        self.selected = keep
            .and_then(|id| self.ids.iter().position(|i| *i == id))
            .unwrap_or(self.selected)
            .min(self.ids.len().saturating_sub(1));
    }

    fn current(&self) -> Option<&String> {
        self.ids.get(self.selected)
    }

    pub fn step(&mut self, key: Key) -> Outcome {
        if let Some((action, id)) = self.pending.take() {
            self.message = None;
            return match key {
                Key::Char('y' | 'Y') => Outcome::Run(action, id),
                _ => Outcome::Nothing,
            };
        }
        self.message = None;
        let id = self.current().cloned();
        match key {
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                if self.selected + 1 < self.ids.len() {
                    self.selected += 1;
                }
            }
            Key::Char('q') | Key::Quit | Key::Esc => return Outcome::Quit,
            Key::Tab => return Outcome::ToggleArchive,
            Key::Enter | Key::Char('l') => return id.map_or(Outcome::Nothing, Outcome::Logs),
            Key::Char('a') => return id.map_or(Outcome::Nothing, Outcome::Attach),
            Key::Char('r') => return id.map_or(Outcome::Nothing, Outcome::Retry),
            Key::Char(c @ ('d' | 'f' | 'c')) => {
                if let Some(id) = id {
                    let action = match c {
                        'd' => Action::Done,
                        'f' => Action::Fail,
                        _ => Action::Cancel,
                    };
                    self.pending = Some((action, id));
                }
            }
            _ => {}
        }
        Outcome::Nothing
    }
}

/// Cuts a line to `width` characters.
fn clip(line: &str, width: usize) -> String {
    line.chars().take(width).collect()
}

/// The whole screen: a title, the table with the selected row inverted, and
/// a footer. `table` is the header line followed by one line per task.
fn render(
    title: &str,
    table: &[String],
    ui: &Ui,
    help: &str,
    (width, height): (usize, usize),
) -> String {
    let mut out = String::from("\x1B[2J\x1B[H");
    out.push_str(&clip(title, width));
    out.push_str("\r\n\r\n");
    if ui.ids.is_empty() {
        out.push_str("(none)\r\n");
    } else {
        out.push_str(&clip(&table[0], width));
        out.push_str("\r\n");
        let visible = height.saturating_sub(5).max(1);
        let top = (ui.selected + 1).saturating_sub(visible);
        for (i, line) in table[1..].iter().enumerate().skip(top).take(visible) {
            let line = clip(line, width);
            if i == ui.selected {
                out.push_str(&format!("\x1B[7m{line}\x1B[0m\r\n"));
            } else {
                out.push_str(&format!("{line}\r\n"));
            }
        }
    }
    let footer = match (&ui.pending, &ui.message) {
        (Some((action, id)), _) => format!("{} `{id}`? y/n", action.verb()),
        (None, Some(m)) => m.clone(),
        (None, None) => String::new(),
    };
    out.push_str(&format!(
        "\x1B[{};1H\x1B[K{}\x1B[{};1H\x1B[K{}",
        height.saturating_sub(1).max(1),
        clip(&footer, width),
        height.max(1),
        clip(help, width),
    ));
    out
}

/// Raw, unbuffered input on the alternate screen, put back on drop.
struct RawTerminal {
    saved: libc::termios,
}

impl RawTerminal {
    fn enter() -> Result<Self> {
        // SAFETY: termios is plain data; tcgetattr fills it in.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `saved` is a valid termios for tcgetattr to fill.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            bail!("stdin is not a terminal");
        }
        let mut raw = saved;
        // Output processing stays on so "\n" still returns the carriage.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a termios obtained from tcgetattr above.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            bail!("couldn't switch the terminal to raw mode");
        }
        print!("\x1B[?1049h\x1B[?25l");
        std::io::stdout().flush().ok();
        Ok(Self { saved })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        print!("\x1B[?25h\x1B[?1049l");
        std::io::stdout().flush().ok();
        // SAFETY: restores the termios saved by `enter`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

/// Waits up to `timeout` (forever if `None`) for keys.
fn read_keys(timeout: Option<Duration>) -> Vec<Key> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    // SAFETY: `fd` is one valid pollfd, matching the count of 1.
    if unsafe { libc::poll(&mut fd, 1, ms) } <= 0 {
        return Vec::new();
    }
    let mut buf = [0u8; 64];
    // SAFETY: reads at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return Vec::new();
    }
    parse_keys(&buf[..n as usize])
}

fn terminal_size() -> (usize, usize) {
    // SAFETY: winsize is plain data; the ioctl below fills it in.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize into `ws`.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 24)
    }
}

const HELP: &str =
    "j/k move  l logs  d done  f fail  c cancel  r retry  a attach  tab queue/archive  q quit";

/// Runs the interactive list until the user quits.
pub fn run(home: &Home, mut archive: bool, interval: Duration) -> Result<()> {
    home.ensure_initialized()?;
    let mut term = Some(RawTerminal::enter()?);
    let mut ui = Ui::default();
    loop {
        let config = Config::load(&home.config_path())?;
        let dir = if archive {
            home.archive()
        } else {
            home.tasks()
        };
        let (tasks, _) = load_tasks(&dir)?;
        ui.set_ids(tasks.iter().map(|t| t.id.clone()).collect());
        let table = table_lines(&task_rows(&config, &tasks));
        let title = format!(
            "furmi {} ({} tasks), as of {}",
            if archive { "archive" } else { "queue" },
            tasks.len(),
            fmt_time(now())
        );
        let screen = render(&title, &table, &ui, HELP, terminal_size());
        print!("{screen}");
        std::io::stdout().flush().ok();

        for key in read_keys(Some(interval)) {
            match ui.step(key) {
                Outcome::Nothing => {}
                Outcome::Quit => return Ok(()),
                Outcome::ToggleArchive => {
                    archive = !archive;
                    ui = Ui::default();
                    break;
                }
                Outcome::Logs(id) => match control::logs(home, &id, None) {
                    Ok(text) => show_logs(&id, &text),
                    Err(e) => ui.message = Some(format!("{e:#}")),
                },
                Outcome::Attach(id) => {
                    drop(term.take());
                    let result = control::attach(home, Some(id));
                    term = Some(RawTerminal::enter()?);
                    if let Err(e) = result {
                        ui.message = Some(format!("{e:#}"));
                    }
                }
                Outcome::Retry(id) => {
                    ui.message = Some(match control::retry(home, &id) {
                        Ok(()) => format!("`{id}` requeued"),
                        Err(e) => format!("{e:#}"),
                    });
                }
                Outcome::Run(action, id) => {
                    let result = match action {
                        Action::Done => control::done(home, Some(id.clone())),
                        Action::Fail => control::fail(home, Some(id.clone()), None),
                        Action::Cancel => control::cancel(home, Some(id.clone())),
                    };
                    ui.message = Some(match result {
                        Ok(()) => format!("`{id}`: {} done", action.verb().to_lowercase()),
                        Err(e) => format!("{e:#}"),
                    });
                }
            }
        }
    }
}

/// The tail of a task's output, until any key is pressed.
fn show_logs(id: &str, text: &str) {
    let (width, height) = terminal_size();
    let keep = height.saturating_sub(3).max(1);
    let lines: Vec<&str> = text.lines().collect();
    let tail = &lines[lines.len().saturating_sub(keep)..];
    let mut out = format!(
        "\x1B[2J\x1B[H{}\r\n\r\n",
        clip(&format!("logs: {id}"), width)
    );
    for line in tail {
        out.push_str(&clip(line, width));
        out.push_str("\r\n");
    }
    out.push_str(&format!("\x1B[{height};1H\x1B[Kany key to go back"));
    print!("{out}");
    std::io::stdout().flush().ok();
    while read_keys(None).is_empty() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(ids: &[&str]) -> Ui {
        let mut u = Ui::default();
        u.set_ids(ids.iter().map(|s| s.to_string()).collect());
        u
    }

    #[test]
    fn parses_plain_keys_and_arrows() {
        assert_eq!(
            parse_keys(b"jk\x1B[A\x1B[B\x1BOA\r\t\x03"),
            vec![
                Key::Char('j'),
                Key::Char('k'),
                Key::Up,
                Key::Down,
                Key::Up,
                Key::Enter,
                Key::Tab,
                Key::Quit
            ]
        );
        assert_eq!(parse_keys(b"\x1B"), vec![Key::Esc]);
    }

    #[test]
    fn swallows_sequences_it_does_not_use() {
        // Ctrl-Right ends in 'C' and F5 in '~': neither may leak a letter.
        assert_eq!(parse_keys(b"\x1B[1;5Cq"), vec![Key::Char('q')]);
        assert_eq!(parse_keys(b"\x1B[15~"), Vec::<Key>::new());
        assert_eq!(parse_keys(b"\x1B[1;5dd"), vec![Key::Char('d')]);
    }

    #[test]
    fn cursor_moves_and_stays_in_range() {
        let mut u = ui(&["a", "b", "c"]);
        u.step(Key::Up);
        assert_eq!(u.selected, 0);
        u.step(Key::Char('j'));
        u.step(Key::Down);
        u.step(Key::Down);
        assert_eq!(u.selected, 2);
        u.step(Key::Char('k'));
        assert_eq!(u.selected, 1);
    }

    #[test]
    fn destructive_actions_ask_first() {
        let mut u = ui(&["a", "b"]);
        u.step(Key::Down);
        assert_eq!(u.step(Key::Char('c')), Outcome::Nothing);
        assert_eq!(u.pending, Some((Action::Cancel, "b".into())));
        assert_eq!(
            u.step(Key::Char('y')),
            Outcome::Run(Action::Cancel, "b".into())
        );
        assert_eq!(u.pending, None);

        u.step(Key::Char('f'));
        assert_eq!(u.step(Key::Char('n')), Outcome::Nothing);
        assert_eq!(u.pending, None);
    }

    #[test]
    fn retry_logs_and_attach_target_the_selected_task() {
        let mut u = ui(&["a", "b"]);
        assert_eq!(u.step(Key::Char('r')), Outcome::Retry("a".into()));
        assert_eq!(u.step(Key::Enter), Outcome::Logs("a".into()));
        assert_eq!(u.step(Key::Char('a')), Outcome::Attach("a".into()));
        assert_eq!(u.step(Key::Tab), Outcome::ToggleArchive);
        assert_eq!(u.step(Key::Char('q')), Outcome::Quit);
    }

    #[test]
    fn actions_do_nothing_on_an_empty_list() {
        let mut u = ui(&[]);
        for k in ['d', 'f', 'c', 'r', 'l', 'a'] {
            assert_eq!(u.step(Key::Char(k)), Outcome::Nothing);
        }
        assert_eq!(u.pending, None);
    }

    #[test]
    fn cursor_follows_its_task_across_refreshes() {
        let mut u = ui(&["a", "b", "c"]);
        u.step(Key::Down);
        u.set_ids(vec!["x".into(), "a".into(), "b".into()]);
        assert_eq!(u.selected, 2);
        u.set_ids(vec!["x".into()]);
        assert_eq!(u.selected, 0);
    }

    #[test]
    fn render_inverts_the_selected_row_and_scrolls() {
        let ids: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut u = Ui::default();
        u.set_ids(ids.clone());
        u.selected = 9;
        let table: Vec<String> = std::iter::once("ID".to_string())
            .chain(ids.iter().cloned())
            .collect();
        let screen = render("title", &table, &u, "help", (40, 8));
        assert!(screen.contains("\x1B[7mt9\x1B[0m"));
        assert!(!screen.contains("t0\r\n"), "should have scrolled past t0");
        assert!(screen.contains("help"));
    }

    #[test]
    fn render_shows_the_confirmation_prompt() {
        let mut u = ui(&["a"]);
        u.step(Key::Char('d'));
        let screen = render("t", &["ID".into(), "a".into()], &u, "help", (60, 10));
        assert!(screen.contains("Finish `a`? y/n"));
    }
}
