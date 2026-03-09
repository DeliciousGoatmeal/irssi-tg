use unicode_width::UnicodeWidthStr;
use chrono::Local;
use clap::Parser;
use crossterm::{
    event::{Event as CEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use dotenvy::dotenv;
use futures_util::StreamExt;
use grammers_client::{client::UpdatesConfiguration, update::Update, Client, PeerSearchItem};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs,
    io::{self, Write},
    sync::Arc,
    time::Duration,
};
use tokio::time::{interval, MissedTickBehavior};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

// ── Theme Engine ──────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Theme {
    bar_bg: Color,
    bar_fg: Color,
    sys_msg: Color,
    timestamp: Color,
    highlight_bg: Color,
    highlight_fg: Color,
    text_fg: Color,
    nick_colors: Vec<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bar_bg: Color::Blue,
            bar_fg: Color::White,
            sys_msg: Color::Cyan,
            timestamp: Color::DarkGray,
            highlight_bg: Color::Magenta,
            highlight_fg: Color::Black,
            text_fg: Color::Reset,
            nick_colors: vec![
                Color::Cyan, Color::Green, Color::Yellow, Color::Magenta, Color::Red,
                Color::LightCyan, Color::LightGreen, Color::LightYellow, Color::LightMagenta, Color::LightRed,
            ],
        }
    }
}

fn load_theme(requested_theme: &str) -> Theme {
    let mut theme = Theme::default();
    let config_path = "themes.ini";

    if !std::path::Path::new(config_path).exists() {
        let default_ini = r#"; irssi-tg Theme Configuration
; Named colors (Blue, LightMagenta, DarkGray) or hex codes (#1e1e2e)

[default]
bar_bg = Blue
bar_fg = White
sys_msg = Cyan
timestamp = DarkGray
highlight_bg = Magenta
highlight_fg = Black
text_fg = Reset
nick_colors = Cyan, Green, Yellow, Magenta, Red, LightCyan, LightGreen, LightYellow, LightMagenta, LightRed

[matrix]
bar_bg = Black
bar_fg = Green
sys_msg = LightGreen
timestamp = DarkGray
highlight_bg = Green
highlight_fg = Black
text_fg = Green
nick_colors = Green, LightGreen

[dracula]
bar_bg = #282a36
bar_fg = #f8f8f2
sys_msg = #ff79c6
timestamp = #6272a4
highlight_bg = #ff79c6
highlight_fg = #f8f8f2
text_fg = #f8f8f2
nick_colors = #8be9fd, #50fa7b, #ffb86c, #ff79c6, #bd93f9, #ff5555, #f1fa8c
"#;
        let _ = fs::write(config_path, default_ini);
    }

    if let Ok(contents) = fs::read_to_string(config_path) {
        let mut in_target = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') { continue; }
            if line.starts_with('[') && line.ends_with(']') {
                in_target = &line[1..line.len()-1] == requested_theme;
                continue;
            }
            if !in_target { continue; }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                if key == "nick_colors" {
                    let parsed: Vec<Color> = val.split(',')
                        .filter_map(|s| s.trim().parse::<Color>().ok())
                        .collect();
                    if !parsed.is_empty() { theme.nick_colors = parsed; }
                    continue;
                }
                if let Ok(color) = val.parse::<Color>() {
                    match key {
                        "bar_bg"       => theme.bar_bg = color,
                        "bar_fg"       => theme.bar_fg = color,
                        "sys_msg"      => theme.sys_msg = color,
                        "timestamp"    => theme.timestamp = color,
                        "highlight_bg" => theme.highlight_bg = color,
                        "highlight_fg" => theme.highlight_fg = color,
                        "text_fg"      => theme.text_fg = color,
                        _ => {}
                    }
                }
            }
        }
    }
    theme
}

fn nick_color(name: &str, theme: &Theme) -> Color {
    if theme.nick_colors.is_empty() { return theme.bar_fg; }
    let hash: usize = name.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize));
    theme.nick_colors[hash % theme.nick_colors.len()]
}

// ── CLI ───────────────────────────────────────────────────────────────────────
#[derive(Parser, Debug)]
#[command(author, version, about = "irssi-style Telegram CLI")]
struct Args {
    /// Theme name defined in themes.ini (e.g. default, matrix, dracula)
    #[arg(short, long, default_value = "default")]
    theme: String,
}

// ── Data model ────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct ChatLine {
    ts: String,
    prefix: String,
    text: String,
    is_sys: bool,
    is_highlight: bool,
    nick: Option<String>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum WindowActivity { None, Text, Highlight }

#[derive(Clone)]
struct Window {
    peer: grammers_client::peer::Peer,
    name: String,
    unread: usize,
    activity: WindowActivity,
    lines: VecDeque<ChatLine>,
    /// Known nicks in this channel (for @ completion)
    nicks: Vec<String>,
}

struct TabState {
    candidates: Vec<String>,
    idx: usize,
    cmd_prefix: String,
    word_start: String,
}

struct App {
    windows: Vec<Window>,
    active: Option<usize>,
    input: String,
    status_lines: VecDeque<ChatLine>,
    all_chats: HashMap<String, grammers_client::peer::Peer>,
    ordered_chats: Vec<(String, grammers_client::peer::Peer)>,
    tab: Option<TabState>,
    theme: Theme,
}

impl App {
    fn new(theme: Theme) -> Self {
        Self {
            windows: vec![],
            active: None,
            input: String::new(),
            status_lines: VecDeque::new(),
            all_chats: HashMap::new(),
            ordered_chats: vec![],
            tab: None,
            theme,
        }
    }

    fn active_name(&self) -> &str {
        self.active
            .and_then(|i| self.windows.get(i))
            .map(|w| w.name.as_str())
            .unwrap_or("(status)")
    }

    fn make_sys_line(text: impl Into<String>) -> ChatLine {
        ChatLine {
            ts: Local::now().format("%H:%M").to_string(),
            prefix: "-!-".to_string(),
            text: text.into(),
            is_sys: true,
            is_highlight: false,
            nick: None,
        }
    }

    fn push_status_sys(&mut self, text: impl Into<String>) {
        let l = Self::make_sys_line(text);
        self.status_lines.push_back(l);
        while self.status_lines.len() > 2000 { self.status_lines.pop_front(); }
    }

    fn push_sys(&mut self, text: impl Into<String>) {
        let line = Self::make_sys_line(text);
        if let Some(ai) = self.active {
            if let Some(w) = self.windows.get_mut(ai) {
                w.lines.push_back(line);
                while w.lines.len() > 2000 { w.lines.pop_front(); }
                return;
            }
        }
        self.status_lines.push_back(line);
        while self.status_lines.len() > 2000 { self.status_lines.pop_front(); }
    }

    fn add_peer(&mut self, peer: grammers_client::peer::Peer) {
        let name = peer.name().unwrap_or("Unknown").to_string();
        let lc = name.to_lowercase();
        self.all_chats.insert(lc.clone(), peer.clone());
        if !self.ordered_chats.iter().any(|(k, _)| k == &lc) {
            self.ordered_chats.push((lc, peer));
        }
    }

    fn find_best_match_key(&self, content: &str) -> Option<String> {
        let lc = content.to_lowercase();
        self.all_chats.keys()
            .filter(|k| lc.starts_with(k.as_str()))
            .max_by_key(|k| k.len())
            .cloned()
    }

    fn focus_window(&mut self, peer: grammers_client::peer::Peer) -> usize {
        let pid = peer.id();
        if let Some(i) = self.windows.iter().position(|w| w.peer.id() == pid) {
            return i;
        }
        let name = peer.name().unwrap_or("Unknown").to_string();
        self.windows.push(Window {
            peer, name, unread: 0, activity: WindowActivity::None,
            lines: VecDeque::new(), nicks: Vec::new(),
        });
        self.windows.len() - 1
    }

    // ── Incoming message: always create window if new, mark activity ──────────
    fn push_incoming(
        &mut self,
        from_peer: &grammers_client::peer::Peer,
        ts: String,
        nick: String,
        text: String,
        highlight: bool,
    ) {
        let chat_id = from_peer.id();
        // Track nick for @ completion
        if let Some(i) = self.windows.iter().position(|w| w.peer.id() == chat_id) {
            if !self.windows[i].nicks.contains(&nick) {
                self.windows[i].nicks.push(nick.clone());
            }
        }
        let line = ChatLine {
            ts,
            prefix: format!("<{}>", nick),
            text,
            is_sys: false,
            is_highlight: highlight,
            nick: Some(nick.clone()),
        };

        // Find existing window by peer ID, or fuzzy name match
        let win_idx = self.windows.iter().position(|w| w.peer.id() == chat_id)
            .or_else(|| {
                let nick_lc = nick.to_lowercase();
                self.windows.iter().position(|w| {
                    let wn = w.name.to_lowercase();
                    wn == nick_lc || wn.starts_with(&nick_lc) || nick_lc.starts_with(&wn)
                })
            });

        let i = if let Some(i) = win_idx {
            i
        } else {
            // Brand-new sender — open a window for them
            self.add_peer(from_peer.clone());
            let name = from_peer.name().unwrap_or("Unknown").to_string();
            self.windows.push(Window {
                peer: from_peer.clone(),
                name,
                unread: 0,
                activity: WindowActivity::None,
                lines: VecDeque::new(),
                nicks: Vec::new(),
            });
            self.windows.len() - 1
        };

        let is_active = self.active == Some(i);
        if !is_active {
            let lvl = if highlight { WindowActivity::Highlight } else { WindowActivity::Text };
            if lvl > self.windows[i].activity { self.windows[i].activity = lvl; }
            self.windows[i].unread += 1;
        }
        self.windows[i].lines.push_back(line);
        while self.windows[i].lines.len() > 2000 { self.windows[i].lines.pop_front(); }
    }

    // ── Tab completion ────────────────────────────────────────────────────────
    fn do_tab(&mut self) {
        let raw = self.input.clone();

        // ── @ mention completion (mid-message) ────────────────────────────────
        // Triggered when the last word starts with @
        if let Some(at_pos) = raw.rfind('@') {
            let partial = &raw[at_pos + 1..];
            let partial_lc = partial.to_lowercase();
            let before = raw[..at_pos].to_string();

            // Collect nicks from current window, fall back to all_chats
            let candidates: Vec<String> = if let Some(ai) = self.active {
                let win_nicks = self.windows[ai].nicks.clone();
                if !win_nicks.is_empty() {
                    win_nicks.into_iter()
                        .filter(|n| n.to_lowercase().starts_with(&partial_lc))
                        .collect()
                } else {
                    self.ordered_chats.iter()
                        .filter(|(lc, _)| lc.starts_with(&partial_lc))
                        .map(|(_, p)| p.name().unwrap_or("Unknown").to_string())
                        .collect()
                }
            } else {
                return;
            };

            let reuse = self.tab.as_ref()
                .map(|t| t.cmd_prefix == before && t.word_start == partial.to_string())
                .unwrap_or(false);

            if !reuse {
                let mut cands = candidates;
                cands.sort_by_key(|s| s.to_lowercase());
                cands.dedup();
                if cands.is_empty() { return; }
                self.tab = Some(TabState {
                    candidates: cands,
                    idx: 0,
                    cmd_prefix: before.clone(),
                    word_start: partial.to_string(),
                });
            } else if let Some(ref mut t) = self.tab {
                t.idx = (t.idx + 1) % t.candidates.len();
            }

            if let Some(ref t) = self.tab {
                self.input = format!("{}@{}", t.cmd_prefix, t.candidates[t.idx]);
            }
            return;
        }

        // ── Command argument completion ────────────────────────────────────────
        let (cmd_prefix, partial) = if let Some(r) = raw.strip_prefix("/join ") {
            ("/join ".to_string(), r.to_string())
        } else if let Some(r) = raw.strip_prefix("/msg ") {
            ("/msg ".to_string(), r.to_string())
        } else if let Some(r) = raw.strip_prefix("/query ") {
            ("/query ".to_string(), r.to_string())
        } else if let Some(r) = raw.strip_prefix("/privmsg ") {
            ("/privmsg ".to_string(), r.to_string())
        } else if let Some(r) = raw.strip_prefix("/whois ") {
            ("/whois ".to_string(), r.to_string())
        } else if let Some(r) = raw.strip_prefix("/names ") {
            ("/names ".to_string(), r.to_string())
        } else {
            return;
        };

        let reuse = self.tab.as_ref()
            .map(|t| t.cmd_prefix == cmd_prefix && t.word_start == partial)
            .unwrap_or(false);

        if !reuse {
            let partial_lc = partial.to_lowercase();
            let mut candidates: Vec<String> = self.ordered_chats.iter()
                .filter(|(lc, _)| lc.starts_with(&partial_lc))
                .map(|(_, p)| p.name().unwrap_or("Unknown").to_string())
                .collect();
            candidates.sort_by_key(|s| s.to_lowercase());
            candidates.dedup();
            if candidates.is_empty() { return; }
            self.tab = Some(TabState { candidates, idx: 0, cmd_prefix, word_start: partial });
        } else if let Some(ref mut t) = self.tab {
            t.idx = (t.idx + 1) % t.candidates.len();
        }

        if let Some(ref t) = self.tab {
            self.input = format!("{}{}", t.cmd_prefix, t.candidates[t.idx]);
        }
    }

    fn reset_tab(&mut self) { self.tab = None; }

    // ── Session persistence ───────────────────────────────────────────────────
    // Format: "ACTIVE=N" on first line, then "peer_id|peer_name" per window.
    // peer_id is authoritative; name is a fallback for peers not yet in cache.
    fn save_windows(&self) {
        if let Ok(mut f) = fs::File::create(".saved_windows.txt") {
            if let Some(ai) = self.active {
                let _ = writeln!(f, "ACTIVE={}", ai);
            }
            for w in &self.windows {
                let _ = writeln!(f, "{}|{}", w.peer.id(), w.name);
            }
        }
    }
}

// ── History ───────────────────────────────────────────────────────────────────

// ── Media description ─────────────────────────────────────────────────────────
// Returns a human-readable string for a media attachment.
// Stickers show their emoji, photos/videos show a t.me deep-link style label.
fn describe_media(media: &grammers_client::types::Media) -> String {
    use grammers_client::types::Media;
    match media {
        Media::Sticker(s) => {
            let emoji = s.emoji().unwrap_or("?");
            format!("[Sticker {}]", emoji)
        }
        Media::Photo(_) => "[Photo]".to_string(),
        Media::Document(d) => {
            let name = d.name().unwrap_or("file");
            let mime = d.mime_type().unwrap_or("?");
            if mime.starts_with("video/") {
                format!("[Video: {}]", name)
            } else if mime.starts_with("audio/") {
                format!("[Audio: {}]", name)
            } else if mime == "image/gif" || mime == "image/webp" {
                format!("[GIF/Anim]")
            } else {
                format!("[File: {}]", name)
            }
        }
        Media::Contact(c) => format!("[Contact: {} {}]",
            c.first_name(), c.last_name().unwrap_or("")),
        Media::Geo(g) => format!("[Location: {:.4},{:.4}]", g.latitude(), g.longitude()),
        Media::GeoLive(g) => format!("[Live Location: {:.4},{:.4}]", g.latitude(), g.longitude()),
        Media::Poll(p) => format!("[Poll: {}]", p.question()),
        Media::Venue(v) => format!("[Venue: {}]", v.title()),
        _ => "[Media]".to_string(),
    }
}

async fn load_history(client: &Client, app: &mut App, win_idx: usize, limit: usize) -> Result {
    let peer = app.windows[win_idx].peer.clone();
    let peer_ref = match peer.to_ref().await { Some(r) => r, None => return Ok(()) };
    let mut it = client.iter_messages(peer_ref).limit(limit);
    let mut buf = Vec::new();
    while let Some(msg) = it.next().await? { buf.push(msg); }
    buf.reverse();
    app.windows[win_idx].lines.clear();
    for msg in buf {
        let ts = msg.date().with_timezone(&Local).format("%H:%M").to_string();
        let sender = msg.sender().and_then(|s| s.name()).unwrap_or("Unknown").to_string();
        let txt = msg.text().to_string();
        let (prefix, nick) = if msg.outgoing() {
            ("<You>".to_string(), Some("You".to_string()))
        } else {
            // Collect sender names for @ completion
            if !app.windows[win_idx].nicks.contains(&sender) {
                app.windows[win_idx].nicks.push(sender.clone());
            }
            (format!("<{}>", sender), Some(sender))
        };
        // Show sticker emoji / media type if no text caption
        let display_txt = if txt.is_empty() {
            msg.media().map(|m| describe_media(&m)).unwrap_or_else(|| "[media]".to_string())
        } else if msg.media().is_some() {
            // Has both text (caption) and media — prepend media label
            let label = describe_media(&msg.media().unwrap());
            format!("{} {}", label, txt)
        } else {
            txt
        };
        app.windows[win_idx].lines.push_back(ChatLine {
            ts, prefix, text: display_txt, is_sys: false, is_highlight: false, nick,
        });
    }
    Ok(())
}

async fn refresh_history(client: &Client, app: &mut App, win_idx: usize) -> Result {
    let peer = app.windows[win_idx].peer.clone();
    let peer_ref = match peer.to_ref().await { Some(r) => r, None => return Ok(()) };
    let mut it = client.iter_messages(peer_ref).limit(20);
    let mut buf = Vec::new();
    while let Some(msg) = it.next().await? { buf.push(msg); }
    buf.reverse();
    for msg in buf {
        let ts = msg.date().with_timezone(&Local).format("%H:%M").to_string();
        let txt = msg.text().to_string();
        let display_txt = if txt.is_empty() {
            match msg.media() {
                Some(m) => describe_media(&m),
                None => continue, // truly empty, skip
            }
        } else if msg.media().is_some() {
            format!("{} {}", describe_media(&msg.media().unwrap()), txt)
        } else {
            txt.clone()
        };
        let already = app.windows[win_idx].lines.iter().rev().take(30)
            .any(|l| l.ts == ts && l.text == display_txt);
        if already { continue; }
        let sender = msg.sender().and_then(|s| s.name()).unwrap_or("Unknown").to_string();
        let (prefix, nick) = if msg.outgoing() {
            ("<You>".to_string(), Some("You".to_string()))
        } else {
            (format!("<{}>", sender), Some(sender))
        };
        app.windows[win_idx].lines.push_back(ChatLine {
            ts, prefix, text: display_txt, is_sys: false, is_highlight: false, nick,
        });
        while app.windows[win_idx].lines.len() > 2000 { app.windows[win_idx].lines.pop_front(); }
    }
    Ok(())
}

// ── UI ────────────────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    let size = f.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(size);
    draw_topic_bar(f, app, chunks[0]);
    draw_scrollback(f, app, chunks[1]);
    draw_window_bar(f, app, chunks[2]);
    draw_input_line(f, app, chunks[3]);
}

fn draw_topic_bar(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg).add_modifier(Modifier::BOLD);
    let time_str = Local::now().format("%H:%M").to_string();
    let act: Vec<usize> = app.windows.iter().enumerate()
        .filter(|(_, w)| w.activity != WindowActivity::None)
        .map(|(i, _)| i + 2)
        .collect();
    let act_str = if act.is_empty() { String::new() } else {
        format!(" Act: {}", act.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","))
    };
    let text = format!(" irssi-tg | {} | {}{}", app.active_name(), time_str, act_str);
    let padded = format!("{:<width$}", text, width = area.width as usize);
    f.render_widget(Paragraph::new(Span::styled(padded, bg)), area);
}

fn draw_scrollback(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    let src_vec: Vec<&ChatLine> = if let Some(ai) = app.active {
        app.windows.get(ai).map(|w| w.lines.iter()).into_iter().flatten().collect()
    } else {
        app.status_lines.iter().collect()
    };

    if src_vec.is_empty() {
        lines.push(Line::from(Span::styled(
            "No messages. /help for commands.",
            Style::default().fg(app.theme.timestamp),
        )));
        f.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }

    let h = area.height as usize;
    let w = area.width.max(1) as usize;
    let mut total_lines = 0usize;
    let mut start_idx = 0usize;
    for (i, l) in src_vec.iter().enumerate().rev() {
        let plain = format!("{} {} {}", l.ts, l.prefix, l.text);
        let char_w = UnicodeWidthStr::width(plain.as_str());
        let rendered = (char_w.saturating_sub(1) / w) + 1;
        if total_lines + rendered > h { start_idx = i + 1; break; }
        total_lines += rendered;
        start_idx = i;
    }

    // Pad top so messages anchor to the bottom (irssi-style)
    let padding = h.saturating_sub(total_lines);
    for _ in 0..padding { lines.push(Line::raw("")); }

    for l in src_vec.into_iter().skip(start_idx) {
        let ts = Span::styled(format!("{} ", l.ts), Style::default().fg(app.theme.timestamp));
        if l.is_sys {
            lines.push(Line::from(vec![
                ts,
                Span::styled(format!("{} ", l.prefix), Style::default().fg(app.theme.sys_msg).add_modifier(Modifier::BOLD)),
                Span::styled(l.text.clone(), Style::default().fg(app.theme.sys_msg)),
            ]));
        } else if l.is_highlight {
            lines.push(Line::from(vec![
                ts,
                Span::styled(
                    format!("{} {}", l.prefix, l.text),
                    Style::default().fg(app.theme.highlight_bg).add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            let col = l.nick.as_deref().map(|n| nick_color(n, &app.theme)).unwrap_or(app.theme.bar_fg);
            lines.push(Line::from(vec![
                ts,
                Span::styled(format!("{} ", l.prefix), Style::default().fg(col).add_modifier(Modifier::BOLD)),
                Span::styled(l.text.clone(), Style::default().fg(app.theme.text_fg)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), area);
}

fn draw_window_bar(f: &mut Frame, app: &App, area: Rect) {
    let bar_bg = Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg);
    let width = area.width as usize;

    // Build all tab labels + styles up front
    struct Tab { label: String, style: Style }
    let mut all_tabs: Vec<Tab> = Vec::new();

    let status_style = if app.active.is_none() {
        Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(app.theme.bar_bg).fg(app.theme.timestamp)
    };
    let status_marker = if app.active.is_none() { "*" } else { "" };
    all_tabs.push(Tab {
        label: format!("[1{}(status)] ", status_marker),
        style: status_style,
    });

    for (i, w) in app.windows.iter().enumerate() {
        let num = i + 2;
        let is_active = app.active == Some(i);
        let marker = if is_active { "*" }
                     else if w.activity != WindowActivity::None { "+" }
                     else { "" };
        let unread = if w.unread > 0 && !is_active { format!("({})", w.unread) } else { String::new() };
        let label = format!("[{}{}{}{}] ", num, marker, w.name, unread);
        let style = if is_active {
            Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg).add_modifier(Modifier::BOLD)
        } else if w.activity == WindowActivity::Highlight {
            Style::default().bg(app.theme.bar_bg).fg(app.theme.highlight_bg).add_modifier(Modifier::BOLD)
        } else if w.activity == WindowActivity::Text {
            Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg)
        } else {
            Style::default().bg(app.theme.bar_bg).fg(app.theme.timestamp)
        };
        all_tabs.push(Tab { label, style });
    }

    // Find which tab is active (0 = status, i+1 = window i)
    let active_tab = app.active.map(|i| i + 1).unwrap_or(0);

    // Figure out the scroll offset so the active tab is always visible.
    // Walk backwards from the active tab until we run out of space.
    let total_tabs = all_tabs.len();
    let mut visible_end = active_tab + 1; // exclusive, always include active
    let mut visible_start = active_tab;
    let mut used = all_tabs[active_tab].label.len();

    // Try to fill rightward first
    while visible_end < total_tabs {
        let next_w = all_tabs[visible_end].label.len();
        if used + next_w > width { break; }
        used += next_w;
        visible_end += 1;
    }
    // Then fill leftward
    while visible_start > 0 {
        let prev_w = all_tabs[visible_start - 1].label.len();
        if used + prev_w > width { break; }
        used += prev_w;
        visible_start -= 1;
    }

    // Scroll indicator prefix/suffix
    let show_left  = visible_start > 0;
    let show_right = visible_end < total_tabs;
    let left_indicator  = if show_left  { "<" } else { "" };
    let right_indicator = if show_right { ">" } else { "" };

    let mut spans: Vec<Span> = Vec::new();
    if show_left {
        spans.push(Span::styled("<", Style::default().bg(app.theme.bar_bg).fg(app.theme.highlight_bg).add_modifier(Modifier::BOLD)));
    }
    for tab in &all_tabs[visible_start..visible_end] {
        spans.push(Span::styled(tab.label.clone(), tab.style));
    }
    if show_right {
        spans.push(Span::styled(">", Style::default().bg(app.theme.bar_bg).fg(app.theme.highlight_bg).add_modifier(Modifier::BOLD)));
    }

    // Pad remainder
    let rendered: usize = left_indicator.len() + right_indicator.len()
        + all_tabs[visible_start..visible_end].iter().map(|t| t.label.len()).sum::<usize>();
    let pad = width.saturating_sub(rendered);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bar_bg));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input_line(f: &mut Frame, app: &App, area: Rect) {
    let chan_prefix = format!("[{}] ", app.active_name());
    let prefix_w = UnicodeWidthStr::width(chan_prefix.as_str()) as usize;
    let avail_w  = (area.width as usize).saturating_sub(prefix_w);

    // Scroll input so the cursor (end of input) is always visible.
    // We show a window of `avail_w` chars ending at the cursor.
    let input_chars: Vec<char> = app.input.chars().collect();
    let cursor_pos = input_chars.len(); // cursor always at end for now
    let view_end   = cursor_pos;
    let view_start = if view_end >= avail_w { view_end - avail_w } else { 0 };
    let visible_input: String = input_chars[view_start..view_end].iter().collect();

    // Scrolled indicator: show "‹" if we've scrolled right
    let scroll_marker = if view_start > 0 { "‹" } else { "" };

    let tab_hint = app.tab.as_ref().and_then(|t| {
        if t.candidates.len() > 1 {
            let others: Vec<&str> = t.candidates.iter().enumerate()
                .filter(|(i, _)| *i != t.idx).take(3)
                .map(|(_, s)| s.as_str()).collect();
            Some(format!("  [{}]", others.join(" | ")))
        } else { None }
    });

    let mut spans = vec![
        Span::styled(chan_prefix, Style::default().fg(app.theme.bar_fg).add_modifier(Modifier::BOLD)),
    ];
    if !scroll_marker.is_empty() {
        spans.push(Span::styled(scroll_marker.to_string(), Style::default().fg(app.theme.highlight_bg)));
    }
    spans.push(Span::styled(visible_input, Style::default().fg(app.theme.text_fg)));
    if let Some(h) = &tab_hint {
        // Only show tab hint if there's room
        let hint_space = (area.width as usize)
            .saturating_sub(prefix_w + UnicodeWidthStr::width(app.input.as_str()) + 1);
        if hint_space > 5 {
            spans.push(Span::styled(h.clone(), Style::default().fg(app.theme.timestamp)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // Cursor sits at end of visible input
    let visible_w = UnicodeWidthStr::width(visible_input.as_str()) as u16;
    let scroll_w  = if view_start > 0 { 1u16 } else { 0 };
    let cx = (area.x + prefix_w as u16 + scroll_w + visible_w)
        .min(area.x + area.width.saturating_sub(1));
    f.set_cursor(cx, area.y);
}

// ── Command handler ───────────────────────────────────────────────────────────

async fn handle_command(client: &Client, app: &mut App, text: String) -> Result {
    let text = text.trim().to_string();
    if text.is_empty() { return Ok(()); }

    if text == "/quit" || text.starts_with("/quit ") {
        app.save_windows();
        std::process::exit(0);
    }

    // /win N / /screen N
    let win_switch = text.strip_prefix("/win ")
        .or_else(|| text.strip_prefix("/screen "))
        .and_then(|r| r.trim().parse::<usize>().ok());
    if let Some(n) = win_switch {
        if n == 1 {
            app.active = None;
            app.push_status_sys("Switched to status window.");
        } else {
            let idx = n.saturating_sub(2);
            if idx < app.windows.len() {
                app.active = Some(idx);
                app.windows[idx].unread = 0;
                app.windows[idx].activity = WindowActivity::None;
                let nm = app.windows[idx].name.clone();
                app.push_sys(format!("-!- Now in: {}", nm));
                let _ = load_history(client, app, idx, 100).await;
            }
        }
        app.save_windows();
        return Ok(());
    }

    // /win (list)
    if text == "/win" || text == "/screen" {
        let mut out = vec!["  [1] (status)".to_string()];
        for (i, w) in app.windows.iter().enumerate() {
            let star = if app.active == Some(i) { "*" } else { " " };
            out.push(format!("  [{}{}] {}", i + 2, star, w.name));
        }
        for line in out { app.push_sys(line); }
        return Ok(());
    }

    // /close [N]
    if let Some(rest) = text.strip_prefix("/close") {
        let rest = rest.trim();
        let idx = if rest.is_empty() { app.active }
                  else { rest.parse::<usize>().ok().map(|n| n.saturating_sub(2)) };
        if let Some(idx) = idx {
            if idx < app.windows.len() {
                let was_active = app.active == Some(idx);
                let name = app.windows[idx].name.clone();
                app.windows.remove(idx);
                if app.windows.is_empty() {
                    app.active = None;
                } else if was_active {
                    app.active = Some(0);
                    app.windows[0].unread = 0;
                    let _ = load_history(client, app, 0, 100).await;
                } else if let Some(a) = app.active {
                    if a > idx { app.active = Some(a - 1); }
                }
                app.push_sys(format!("-!- Closed: {}", name));
                app.save_windows();
            }
        }
        return Ok(());
    }

    // /list [query]
    if let Some(rest) = text.strip_prefix("/list") {
        let q = rest.trim().to_lowercase();
        let mut out = vec!["-!- Channel List:".to_string()];
        for (lc, peer) in &app.ordered_chats {
            if q.is_empty() || lc.contains(&q) {
                out.push(format!("  {}", peer.name().unwrap_or("Unknown")));
            }
        }
        out.push("-!- End of /list".to_string());
        for line in out { app.push_sys(line); }
        return Ok(());
    }

    // /names [chat]
    if let Some(rest) = text.strip_prefix("/names") {
        let target = rest.trim();
        let (peer, win_idx) = if target.is_empty() {
            let wi = app.active;
            (wi.map(|ai| app.windows[ai].peer.clone()), wi)
        } else {
            let p = app.find_best_match_key(target).and_then(|k| app.all_chats.get(&k).cloned());
            let wi = p.as_ref().and_then(|pp| app.windows.iter().position(|w| w.peer.id() == pp.id()));
            (p, wi)
        };
        if let Some(p) = peer {
            let label = p.name().unwrap_or("Unknown").to_string();
            app.push_sys(format!("-!- Fetching names for {}...", label));
            if let Some(p_ref) = p.to_ref().await {
                let mut participants = client.iter_participants(p_ref);
                let mut names: Vec<String> = Vec::new();
                while let Ok(Some(part)) = participants.next().await {
                    let name_str = part.user.first_name()
                        .unwrap_or_else(|| part.user.username().unwrap_or("Unknown"))
                        .to_string();
                    names.push(name_str);
                    if names.len() >= 200 { break; }
                }
                names.sort_by_key(|s| s.to_lowercase());

                // Store nicks for @ completion
                if let Some(wi) = win_idx {
                    app.windows[wi].nicks = names.clone();
                }

                // Render as irssi-style columns: fixed width, sorted alphabetically
                let col_w = names.iter().map(|n| n.len()).max().unwrap_or(10) + 2;
                let col_w = col_w.max(12).min(28);
                // Use 80 chars as a safe terminal width estimate for the column grid
                let term_w = 78usize;
                let cols = (term_w / col_w).max(1);
                app.push_sys(format!("-!- {} ({} users):", label, names.len()));
                for chunk in names.chunks(cols) {
                    let row: String = chunk.iter()
                        .map(|n| format!("{:<width$}", n, width = col_w))
                        .collect::<Vec<_>>()
                        .join("");
                    app.push_sys(format!("    {}", row.trim_end()));
                }
                app.push_sys(format!("-!- Total: {} users  (Tab after @ to mention)", names.len()));
            }
        } else {
            app.push_sys("-!- Cannot find chat for /names");
        }
        return Ok(());
    }

    // /topic
    if text.starts_with("/topic") {
        app.push_sys("-!- /topic is not supported for standard Telegram chats.");
        return Ok(());
    }

    // /whois
    if let Some(rest) = text.strip_prefix("/whois ") {
        let target = rest.trim();
        if let Some(key) = app.find_best_match_key(target) {
            let peer = app.all_chats[&key].clone();
            let name  = peer.name().unwrap_or("Unknown").to_string();
            let uname = peer.username().unwrap_or("none").to_string();
            app.push_sys(format!("-!- WHOIS {} (@{}) ID:{}", name, uname, peer.id()));
        } else {
            app.push_sys(format!("-!- No such nick: {}", target));
        }
        return Ok(());
    }

    // /search
    if let Some(q) = text.strip_prefix("/search ") {
        let query = q.trim();
        app.push_sys(format!("-!- Searching '{}'...", query));
        let results = client.search_peer(query, 10).await?;
        if results.is_empty() {
            app.push_sys("-!- No results.");
        } else {
            for item in results {
                let peer = match item {
                    PeerSearchItem::Contact(p) => p,
                    PeerSearchItem::Dialog(p) => p,
                    PeerSearchItem::Global(p) => p,
                };
                let name  = peer.name().unwrap_or("Unknown").to_string();
                let uname = peer.username().unwrap_or("none").to_string();
                app.push_sys(format!("  {} (@{})", name, uname));
                app.add_peer(peer);
            }
        }
        return Ok(());
    }

    // /join /msg /privmsg /query /trout
    if text.starts_with("/join ")    || text.starts_with("/msg ")
    || text.starts_with("/privmsg ") || text.starts_with("/query ")
    || text.starts_with("/trout ")
    {
        let is_trout = text.starts_with("/trout ");
        let is_msg   = text.starts_with("/msg ") || text.starts_with("/privmsg ") || text.starts_with("/query ");
        let prefix_len = if text.starts_with("/join ")    { 6 }
                    else if text.starts_with("/msg ")     { 5 }
                    else if text.starts_with("/query ")   { 7 }
                    else if text.starts_with("/trout ")   { 7 }
                    else if text.starts_with("/privmsg ") { 9 }
                    else { 0 };
        let rest = text[prefix_len..].trim();

        if let Some(key) = app.find_best_match_key(rest) {
            let peer     = app.all_chats[&key].clone();
            let msg_text = rest[key.len()..].trim().to_string();
            let idx      = app.focus_window(peer.clone());
            app.active   = Some(idx);
            app.windows[idx].unread   = 0;
            app.windows[idx].activity = WindowActivity::None;
            let nm = app.windows[idx].name.clone();

            if is_trout {
                let peer_ref = match peer.to_ref().await {
                    Some(r) => r,
                    None => { app.push_sys("-!- Could not resolve peer."); return Ok(()); }
                };
                let action_text = format!("* slaps {} around a bit with a large trout *", nm);
                client.send_message(peer_ref, action_text.clone()).await?;
                let ts = Local::now().format("%H:%M").to_string();
                app.windows[idx].lines.push_back(ChatLine {
                    ts, prefix: "***".to_string(), text: action_text,
                    is_sys: true, is_highlight: false, nick: None,
                });
            } else if is_msg && !msg_text.is_empty() {
                app.push_sys(format!("-!- Messaging: {}", nm));
                let _ = load_history(client, app, idx, 100).await;
                let peer_ref = match peer.to_ref().await {
                    Some(r) => r,
                    None => { app.push_sys("-!- Could not resolve peer."); return Ok(()); }
                };
                client.send_message(peer_ref, msg_text.clone()).await?;
                let ts = Local::now().format("%H:%M").to_string();
                app.windows[idx].lines.push_back(ChatLine {
                    ts, prefix: "<You>".to_string(), text: msg_text,
                    is_sys: false, is_highlight: false, nick: Some("You".to_string()),
                });
            } else {
                app.push_sys(format!("-!- Joining: {}", nm));
                let _ = load_history(client, app, idx, 100).await;
            }
            app.save_windows();
        } else {
            app.push_sys("-!- No match. /search <n> first.");
        }
        return Ok(());
    }

    // /me
    if let Some(rest) = text.strip_prefix("/me ") {
        if let Some(ai) = app.active {
            let peer   = app.windows[ai].peer.clone();
            let action = format!("* {} *", rest.trim());
            let peer_ref = match peer.to_ref().await { Some(r) => r, None => return Ok(()) };
            client.send_message(peer_ref, action.clone()).await?;
            let ts = Local::now().format("%H:%M").to_string();
            app.windows[ai].lines.push_back(ChatLine {
                ts, prefix: "***".to_string(), text: action,
                is_sys: true, is_highlight: false, nick: None,
            });
        }
        return Ok(());
    }

    // /reload
    if text == "/reload" {
        if let Some(ai) = app.active {
            let nm = app.windows[ai].name.clone();
            app.push_sys(format!("-!- Reloading {}...", nm));
            let _ = load_history(client, app, ai, 100).await;
            app.push_sys("-!- Done.");
        } else {
            app.push_sys("-!- No active window.");
        }
        return Ok(());
    }

    // /help
    if text == "/help" {
        for line in &[
            "-!- Commands:",
            "    /search <query>         search contacts/chats",
            "    /list [query]           list cached dialogs",
            "    /names [chat]           list users in chat",
            "    /join <n>            open window  [Tab completes]",
            "    /msg <n> [text]      open & optionally send  [Tab completes]",
            "    /privmsg /query         aliases for /msg",
            "    /whois <n>           show user info  [Tab completes]",
            "    /me <action>            action in current window",
            "    /trout <n>           slap with a trout",
            "    /win [N]                list or switch windows (1=status)",
            "    /close [N]              close window",
            "    /reload                 re-fetch last 100 messages",
            "    /quit                   exit",
            "    Tab                     cycle name completions",
            "    Ctrl-C                  quit",
        ] { app.push_sys(*line); }
        return Ok(());
    }

    // Unknown slash command
    if text.starts_with('/') {
        app.push_sys(format!("-!- Unknown command: {}  (try /help)",
            text.split_whitespace().next().unwrap_or("")));
        return Ok(());
    }

    // Plain text → send to active window
    if let Some(ai) = app.active {
        let peer = app.windows[ai].peer.clone();
        let peer_ref = match peer.to_ref().await {
            Some(r) => r,
            None => { app.push_sys("-!- Could not resolve peer."); return Ok(()); }
        };
        client.send_message(peer_ref, text.clone()).await?;
        let ts = Local::now().format("%H:%M").to_string();
        app.windows[ai].lines.push_back(ChatLine {
            ts, prefix: "<You>".to_string(), text,
            is_sys: false, is_highlight: false, nick: Some("You".to_string()),
        });
    } else {
        app.push_sys("-!- No active window. /join <n> or /win 2");
    }

    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result {
    dotenv().ok();
    let args = Args::parse();
    let theme = load_theme(&args.theme);

    let api_id: i32 = env::var("TG_API_ID").expect("TG_API_ID not set").parse().unwrap();
    let api_hash = env::var("TG_API_HASH").expect("TG_API_HASH not set");

    let session = Arc::new(SqliteSession::open("client.session").await?);
    let SenderPool { runner, updates, handle } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle.clone());
    tokio::spawn(runner.run());

    if !client.is_authorized().await? {
        println!("*** Not authorized – login required");
        print!("Phone: "); io::stdout().flush()?;
        let mut phone = String::new(); io::stdin().read_line(&mut phone)?;
        let token = client.request_login_code(phone.trim(), &api_hash).await?;
        print!("Code: "); io::stdout().flush()?;
        let mut code = String::new(); io::stdin().read_line(&mut code)?;
        client.sign_in(&token, code.trim()).await?;
    }

    let mut app = App::new(theme);

    // Load recent dialogs into peer cache
    let mut dialogs = client.iter_dialogs();
    while let Ok(Some(dialog)) = dialogs.next().await {
        app.add_peer(dialog.peer().clone());
        if app.ordered_chats.len() >= 500 { break; }
    }
    app.push_status_sys(format!("irssi-tg ready — {} contacts cached.", app.ordered_chats.len()));
    app.push_status_sys("/help for commands  |  Tab completes names in /join and /msg");

    // ── Restore saved windows ─────────────────────────────────────────────────
    // New format: "ACTIVE=N" then "peer_id|name".
    // Old format (just name) still works as fallback.
    if let Ok(contents) = fs::read_to_string(".saved_windows.txt") {
        let mut active_idx: Option<usize> = None;
        let mut restored = 0usize;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }

            if let Some(idx_str) = line.strip_prefix("ACTIVE=") {
                active_idx = idx_str.parse::<usize>().ok();
                continue;
            }

            let found_peer: Option<grammers_client::peer::Peer> =
                if let Some((_id_str, name)) = line.split_once('|') {
                    // Look up by display name (lowercased key in all_chats)
                    app.all_chats.get(&name.to_lowercase()).cloned()
                } else {
                    // Old single-name format
                    app.all_chats.get(&line.to_lowercase()).cloned()
                };

            if let Some(peer) = found_peer {
                app.focus_window(peer);
                restored += 1;
            }
        }

        if restored > 0 {
            let ai = active_idx.unwrap_or(0).min(app.windows.len() - 1);
            app.active = Some(ai);
            let _ = load_history(&client, &mut app, ai, 100).await;
            app.push_status_sys(format!("Restored {} windows from last session.", restored));
        }
    }

    // ── TUI ───────────────────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut ev = EventStream::new();
    let mut updates_stream = client
        .stream_updates(updates, UpdatesConfiguration { catch_up: false, ..Default::default() })
        .await;
    let mut tick = interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut refresh_tick = interval(Duration::from_secs(5));
    refresh_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let res: Result = loop {
        tokio::select! {
            _ = tick.tick() => {
                terminal.draw(|f| ui(f, &app))?;
            }

            _ = refresh_tick.tick() => {
                if let Some(ai) = app.active {
                    let _ = refresh_history(&client, &mut app, ai).await;
                }
            }

            maybe_ev = ev.next() => {
                if let Some(Ok(CEvent::Key(key_ev))) = maybe_ev {
                    if key_ev.kind != KeyEventKind::Press { continue; }
                    match (key_ev.code, key_ev.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            app.save_windows();
                            break Ok(());
                        }

                        // Alt+1..9  → switch to window N
                        (KeyCode::Char(ch), KeyModifiers::ALT) if ch.is_ascii_digit() => {
                            let n = ch.to_digit(10).unwrap_or(0) as usize;
                            if n == 1 {
                                app.active = None;
                                app.push_status_sys("Switched to status window.");
                                app.save_windows();
                            } else if n >= 2 {
                                let idx = n - 2;
                                if idx < app.windows.len() {
                                    app.active = Some(idx);
                                    app.windows[idx].unread = 0;
                                    app.windows[idx].activity = WindowActivity::None;
                                    let nm = app.windows[idx].name.clone();
                                    app.push_sys(format!("-!- Now in: {}", nm));
                                    let _ = load_history(&client, &mut app, idx, 100).await;
                                    app.save_windows();
                                }
                            }
                        }

                        // Alt+Left / Alt+Right → cycle windows
                        (KeyCode::Left, KeyModifiers::ALT) => {
                            let new_active = match app.active {
                                None if !app.windows.is_empty() => Some(app.windows.len() - 1),
                                Some(0) => None,
                                Some(i) => Some(i - 1),
                                None => None,
                            };
                            app.active = new_active;
                            if let Some(i) = app.active {
                                app.windows[i].unread = 0;
                                app.windows[i].activity = WindowActivity::None;
                                let _ = load_history(&client, &mut app, i, 100).await;
                            }
                            app.save_windows();
                        }
                        (KeyCode::Right, KeyModifiers::ALT) => {
                            let new_active = match app.active {
                                None if !app.windows.is_empty() => Some(0),
                                Some(i) if i + 1 < app.windows.len() => Some(i + 1),
                                _ => None,
                            };
                            app.active = new_active;
                            if let Some(i) = app.active {
                                app.windows[i].unread = 0;
                                app.windows[i].activity = WindowActivity::None;
                                let _ = load_history(&client, &mut app, i, 100).await;
                            }
                            app.save_windows();
                        }

                        (KeyCode::Enter, _) => {
                            app.reset_tab();
                            let line = std::mem::take(&mut app.input);
                            if let Err(e) = handle_command(&client, &mut app, line).await {
                                app.push_sys(format!("-!- Error: {}", e));
                            }
                        }
                        (KeyCode::Tab, _) => { app.do_tab(); }
                        (KeyCode::Backspace, _) => { app.reset_tab(); app.input.pop(); }
                        (KeyCode::Char(ch), m)
                            if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT =>
                        {
                            app.reset_tab();
                            app.input.push(ch);
                        }
                        _ => {}
                    }
                }
            }

            // ── Live updates ───────────────────────────────────────────────────
            // CRITICAL FIX: updates_stream yields `Update` directly, not
            // `Result<Update>`. The old `if let Ok(Update::NewMessage(...)) = upd`
            // silently dropped every update because Ok() never matched a bare value.
            maybe_upd = updates_stream.next() => {
                let upd = match maybe_upd {
                    Ok(u)  => u,
                    Err(_) => continue,
                };

                if let Update::NewMessage(message) = upd {
                    let from_peer = match message.peer() { Some(p) => p, None => continue };
                    let chat_id   = from_peer.id();
                    let ts        = message.date().with_timezone(&Local).format("%H:%M").to_string();
                    let raw_txt   = message.text().to_string();
                    let txt = if raw_txt.is_empty() {
                        match message.media() {
                            Some(m) => describe_media(&m),
                            None    => continue, // truly empty update, skip
                        }
                    } else if message.media().is_some() {
                        format!("{} {}", describe_media(&message.media().unwrap()), raw_txt)
                    } else {
                        raw_txt
                    };
                    let highlight = message.mentioned();

                    if message.outgoing() {
                        // Find or create a window. Self-chat messages are always
                        // outgoing, so we must create the window here too.
                        let win_idx = app.windows.iter().position(|w| w.peer.id() == chat_id);
                        let i = if let Some(i) = win_idx {
                            i
                        } else {
                            // New window (e.g. Saved Messages / self-chat)
                            app.add_peer(from_peer.clone());
                            let name = from_peer.name().unwrap_or("Saved Messages").to_string();
                            app.windows.push(Window {
                                peer: from_peer.clone(),
                                name,
                                unread: 0,
                                activity: WindowActivity::None,
                                lines: VecDeque::new(),
                                nicks: Vec::new(),
                            });
                            let new_idx = app.windows.len() - 1;
                            // Load history so context is visible
                            let _ = load_history(&client, &mut app, new_idx, 100).await;
                            app.save_windows();
                            new_idx
                        };
                        // Dedup: skip only if this exact text is already the last line
                        // AND that line was added by us locally (not loaded from history).
                        // Compare by text only — ts has minute precision and would swallow
                        // fast consecutive messages.
                        let already = app.windows[i].lines.back()
                            .map(|l| l.prefix == "<You>" && l.text == txt && l.is_sys == false)
                            .unwrap_or(false);
                        if !already {
                            app.windows[i].lines.push_back(ChatLine {
                                ts, prefix: "<You>".to_string(), text: txt,
                                is_sys: false, is_highlight: false, nick: Some("You".to_string()),
                            });
                            while app.windows[i].lines.len() > 2000 {
                                app.windows[i].lines.pop_front();
                            }
                        }
                    } else {
                        let sender = message.sender()
                            .and_then(|s| s.name())
                            .unwrap_or("Unknown")
                            .to_string();
                        app.push_incoming(&from_peer, ts, sender, txt, highlight);
                        app.save_windows();
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}