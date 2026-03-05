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

// ── Theme Engine ─────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Theme {
    bar_bg: Color,
    bar_fg: Color,
    sys_msg: Color,
    timestamp: Color,
    highlight_bg: Color,
    highlight_fg: Color,
    text_fg: Color,             // NEW: Main message text color
    nick_colors: Vec<Color>,    // NEW: Palette for hashing usernames
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
; You can use named colors (Blue, LightMagenta, DarkGray) or Hex codes (#1e1e2e)

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
highlight_bg = #ff5555
highlight_fg = #f8f8f2
text_fg = #f8f8f2
nick_colors = #8be9fd, #50fa7b, #ffb86c, #ff79c6, #bd93f9, #ff5555, #f1fa8c
"#;
        let _ = fs::write(config_path, default_ini);
    }

    if let Ok(contents) = fs::read_to_string(config_path) {
        let mut in_target_theme = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') { continue; }
            
            if line.starts_with('[') && line.ends_with(']') {
                let section = &line[1..line.len()-1];
                in_target_theme = section == requested_theme;
                continue;
            }

            if in_target_theme {
                if let Some((k, v)) = line.split_once('=') {
                    let key = k.trim();
                    let val = v.trim();
                    
                    if key == "nick_colors" {
                        let parsed_colors: Vec<Color> = val.split(',')
                            .filter_map(|s| s.trim().parse::<Color>().ok())
                            .collect();
                        if !parsed_colors.is_empty() {
                            theme.nick_colors = parsed_colors;
                        }
                        continue;
                    }

                    if let Ok(color) = val.parse::<Color>() {
                        match key {
                            "bar_bg" => theme.bar_bg = color,
                            "bar_fg" => theme.bar_fg = color,
                            "sys_msg" => theme.sys_msg = color,
                            "timestamp" => theme.timestamp = color,
                            "highlight_bg" => theme.highlight_bg = color,
                            "highlight_fg" => theme.highlight_fg = color,
                            "text_fg" => theme.text_fg = color,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    theme
}

// ── CLI ───────────────────────────────────────────────────────────────────────
#[derive(Parser, Debug)]
#[command(author, version, about = "irssi-style Telegram CLI")]
struct Args {
    /// Defined in themes.ini (e.g. default, matrix, dracula)
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

    fn push_incoming(&mut self, from_peer: &grammers_client::peer::Peer, ts: String, nick: String, text: String, highlight: bool) {
        let chat_id = from_peer.id();
        let line = ChatLine {
            ts,
            prefix: format!("<{}>", nick),
            text,
            is_sys: false,
            is_highlight: highlight,
            nick: Some(nick.clone()),
        };

        let win_idx = self.windows.iter().position(|w| w.peer.id() == chat_id)
            .or_else(|| {
                let nick_lc = nick.to_lowercase();
                self.windows.iter().position(|w| {
                    let wn = w.name.to_lowercase();
                    wn == nick_lc || wn.starts_with(&nick_lc) || nick_lc.starts_with(&wn)
                })
            });

        if let Some(i) = win_idx {
            let is_active = self.active == Some(i);
            if !is_active {
                let lvl = if highlight { WindowActivity::Highlight } else { WindowActivity::Text };
                if lvl > self.windows[i].activity { self.windows[i].activity = lvl; }
                self.windows[i].unread += 1;
            }
            let w = &mut self.windows[i];
            w.lines.push_back(line);
            while w.lines.len() > 2000 { w.lines.pop_front(); }
        } else {
            self.add_peer(from_peer.clone());
            let peer_name = from_peer.name().unwrap_or("Unknown").to_string();
            self.push_status_sys(format!("New msg from {}: {}", peer_name, line.text));
        }
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
        self.windows.push(Window { peer, name, unread: 0, activity: WindowActivity::None, lines: VecDeque::new() });
        self.windows.len() - 1
    }

    fn do_tab(&mut self) {
        let raw = self.input.clone();
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

    fn save_windows(&self) {
        if let Ok(mut f) = fs::File::create(".saved_windows.txt") {
            if let Some(ai) = self.active {
                let _ = writeln!(f, "ACTIVE={}", ai);
            }
            for w in &self.windows {
                let _ = writeln!(f, "{}", w.name);
            }
        }
    }
}

// ── History ───────────────────────────────────────────────────────────────────

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
            (format!("<{}>", sender), Some(sender))
        };
        app.windows[win_idx].lines.push_back(ChatLine { ts, prefix, text: txt, is_sys: false, is_highlight: false, nick });
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
        if txt.is_empty() { continue; }
        let already = app.windows[win_idx].lines.iter().rev().take(30)
            .any(|l| l.ts == ts && l.text == txt);
        if already { continue; }
        let sender = msg.sender().and_then(|s| s.name()).unwrap_or("Unknown").to_string();
        let (prefix, nick) = if msg.outgoing() {
            ("<You>".to_string(), Some("You".to_string()))
        } else {
            (format!("<{}>", sender), Some(sender))
        };
        app.windows[win_idx].lines.push_back(ChatLine { ts, prefix, text: txt, is_sys: false, is_highlight: false, nick });
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

    if app.active.is_none() && app.status_lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No active window. /help for commands.",
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
        let plain_prefix = format!("{} {} ", l.ts, l.prefix);
        let text_len = UnicodeWidthStr::width(plain_prefix.as_str()) + UnicodeWidthStr::width(l.text.as_str());
        let lines_for_this_msg = if text_len == 0 { 1 } else { (text_len.saturating_sub(1) / w) + 1 };
        
        if total_lines + lines_for_this_msg > h {
            start_idx = i + 1;
            break; 
        }
        total_lines += lines_for_this_msg;
        start_idx = i;
    }

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
                Span::styled(format!("{} {}", l.prefix, l.text),
                    Style::default().fg(app.theme.highlight_fg).bg(app.theme.highlight_bg).add_modifier(Modifier::BOLD)),
            ]));
        } else {
            // Apply the theme's hashing palette to the username
            let col = l.nick.as_deref().map(|name| {
                let hash: usize = name.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize));
                if app.theme.nick_colors.is_empty() {
                    app.theme.bar_fg
                } else {
                    app.theme.nick_colors[hash % app.theme.nick_colors.len()]
                }
            }).unwrap_or(app.theme.bar_fg);
            
            lines.push(Line::from(vec![
                ts,
                Span::styled(format!("{} ", l.prefix), Style::default().fg(col).add_modifier(Modifier::BOLD)),
                // Apply the theme's text color to standard messages
                Span::styled(l.text.clone(), Style::default().fg(app.theme.text_fg)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), area);
}

fn draw_window_bar(f: &mut Frame, app: &App, area: Rect) {
    let bar_bg = Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg);
    let mut spans: Vec<Span> = Vec::new();

    let status_style = if app.active.is_none() {
        Style::default().bg(app.theme.bar_bg).fg(app.theme.bar_fg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(app.theme.bar_bg).fg(app.theme.timestamp)
    };
    let status_marker = if app.active.is_none() { "*" } else { "" };
    spans.push(Span::styled(format!("[1{}(status)] ", status_marker), status_style));

    for (i, w) in app.windows.iter().enumerate() {
        let num = i + 2;
        let is_active = app.active == Some(i);
        let marker = if is_active { "*" } else {
            match w.activity { WindowActivity::Highlight => "+", _ => "" }
        };
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
        spans.push(Span::styled(label, style));
    }

    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    spans.push(Span::styled(" ".repeat((area.width as usize).saturating_sub(used)), bar_bg));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input_line(f: &mut Frame, app: &App, area: Rect) {
    let chan_prefix = format!("[{}] ", app.active_name());
    let tab_hint = app.tab.as_ref().and_then(|t| {
        if t.candidates.len() > 1 {
            let others: Vec<&str> = t.candidates.iter().enumerate()
                .filter(|(i, _)| *i != t.idx).take(5)
                .map(|(_, s)| s.as_str()).collect();
            Some(format!("  [{}]", others.join(" | ")))
        } else { None }
    });

    let mut spans = vec![
        Span::styled(chan_prefix, Style::default().fg(app.theme.bar_fg).add_modifier(Modifier::BOLD)),
        Span::styled(app.input.clone(), Style::default().fg(app.theme.text_fg)),
    ];
    if let Some(h) = &tab_hint {
        spans.push(Span::styled(h.clone(), Style::default().fg(app.theme.timestamp)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let prefix_w = UnicodeWidthStr::width(format!("[{}] ", app.active_name()).as_str()) as u16;
    let input_w = UnicodeWidthStr::width(app.input.as_str()) as u16;
    let cx = (area.x + prefix_w + input_w).min(area.x + area.width.saturating_sub(1));
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
        let peer = if target.is_empty() {
            app.active.map(|ai| app.windows[ai].peer.clone())
        } else {
            app.find_best_match_key(target).and_then(|k| app.all_chats.get(&k).cloned())
        };
        if let Some(p) = peer {
            let label = p.name().unwrap_or("Unknown").to_string();
            app.push_sys(format!("-!- Fetching names for {}...", label));
            if let Some(p_ref) = p.to_ref().await {
                let mut participants = client.iter_participants(p_ref);
                let mut names = Vec::new();
                while let Ok(Some(part)) = participants.next().await {
                    let name_str = part.user.first_name()
                        .unwrap_or_else(|| part.user.username().unwrap_or("Unknown"))
                        .to_string();
                        
                    names.push(name_str);
                    if names.len() >= 100 { names.push("...".to_string()); break; }
                }
                app.push_sys(format!("-!- Names: {}", names.join(", ")));
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
            let name = peer.name().unwrap_or("Unknown").to_string();
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
                let name = peer.name().unwrap_or("Unknown").to_string();
                let uname = peer.username().unwrap_or("none").to_string();
                app.push_sys(format!("  {} (@{})", name, uname));
                app.add_peer(peer);
            }
        }
        return Ok(());
    }

    // /join /msg /privmsg /query /trout
    if text.starts_with("/join ") || text.starts_with("/msg ")
        || text.starts_with("/privmsg ") || text.starts_with("/query ")
        || text.starts_with("/trout ")
    {
        let is_trout = text.starts_with("/trout ");
        let is_msg = text.starts_with("/msg ") || text.starts_with("/privmsg ") || text.starts_with("/query ");
        let prefix_len = if text.starts_with("/join ")     { 6 }
                    else if text.starts_with("/msg ")      { 5 }
                    else if text.starts_with("/query ")    { 7 }
                    else if text.starts_with("/trout ")    { 7 }
                    else if text.starts_with("/privmsg ")  { 9 }
                    else { 0 };
        let rest = text[prefix_len..].trim();

        if let Some(key) = app.find_best_match_key(rest) {
            let peer = app.all_chats[&key].clone();
            let msg_text = rest[key.len()..].trim().to_string();
            let idx = app.focus_window(peer.clone());
            app.active = Some(idx);
            app.windows[idx].unread = 0;
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
            app.push_sys("-!- No match. /search <name> first.");
        }
        return Ok(());
    }

    // /me
    if let Some(rest) = text.strip_prefix("/me ") {
        if let Some(ai) = app.active {
            let peer = app.windows[ai].peer.clone();
            let action = format!("* {} *", rest.trim());
            let peer_ref = match peer.to_ref().await {
                Some(r) => r,
                None => return Ok(()),
            };
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
            "    /join <name>            open window  [Tab completes]",
            "    /msg <name> [text]      open & optionally send  [Tab completes]",
            "    /privmsg /query         aliases for /msg",
            "    /whois <name>           show user info  [Tab completes]",
            "    /me <action>            action in current window",
            "    /trout <name>           slap with a trout  [Tab completes]",
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

    // Plain text → send
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
        app.push_sys("-!- No active window. /join <name> or /win 2");
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
    let mut dialogs = client.iter_dialogs();
    while let Ok(Some(dialog)) = dialogs.next().await {
        app.add_peer(dialog.peer().clone());
        if app.ordered_chats.len() >= 500 { break; }
    }
    app.push_status_sys(format!("irssi-tg ready — {} contacts cached.", app.ordered_chats.len()));
    app.push_status_sys("/help for commands  |  Tab completes names in /join and /msg");

    // --- WORKSPACE RESTORE ENGINE ---
    if let Ok(contents) = fs::read_to_string(".saved_windows.txt") {
        let mut active_idx = None;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            if let Some(idx_str) = line.strip_prefix("ACTIVE=") {
                active_idx = idx_str.parse::<usize>().ok();
                continue;
            }
            if let Some(peer) = app.all_chats.get(&line.to_lowercase()) {
                app.focus_window(peer.clone());
            }
        }
        if !app.windows.is_empty() {
            let ai = active_idx.unwrap_or(0).min(app.windows.len() - 1);
            app.active = Some(ai);
            let _ = load_history(&client, &mut app, ai, 100).await;
            app.push_status_sys(format!("Restored {} open windows from last session.", app.windows.len()));
        }
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut ev = EventStream::new();
    let mut updates_stream = client
        .stream_updates(updates, UpdatesConfiguration { catch_up: true, ..Default::default() })
        .await;
    let mut tick = interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut refresh_tick = interval(Duration::from_secs(5));
    refresh_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let res: Result = loop {
        tokio::select! {
            _ = tick.tick() => { terminal.draw(|f| ui(f, &app))?; }

            _ = refresh_tick.tick() => {
                if let Some(ai) = app.active {
                    let _ = refresh_history(&client, &mut app, ai).await;
                }
            }

            maybe_ev = ev.next() => {
                if let Some(Ok(CEvent::Key(key_ev))) = maybe_ev {
                    
                    if key_ev.kind != KeyEventKind::Press {
                        continue;
                    }

                    match (key_ev.code, key_ev.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            app.save_windows();
                            break Ok(());
                        },
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

            upd = updates_stream.next() => {
                if let Ok(Update::NewMessage(message)) = upd {
                    let from_peer = match message.peer() { Some(p) => p, None => continue };
                    let ts = message.date().with_timezone(&Local).format("%H:%M").to_string();
                    let txt = message.text().to_string();
                    let highlight = message.mentioned();

                    if message.outgoing() {
                        let chat_id = from_peer.id();
                        let win_idx = app.windows.iter().position(|w| w.peer.id() == chat_id);
                        if let Some(i) = win_idx {
                            let already = app.windows[i].lines.back()
                                .map(|l| l.prefix == "<You>" && l.text == txt)
                                .unwrap_or(false);
                            if !already {
                                let line = ChatLine {
                                    ts, prefix: "<You>".to_string(), text: txt,
                                    is_sys: false, is_highlight: false, nick: Some("You".to_string()),
                                };
                                app.windows[i].lines.push_back(line);
                                while app.windows[i].lines.len() > 2000 { app.windows[i].lines.pop_front(); }
                            }
                        }
                    } else {
                        let sender = message.sender().and_then(|s| s.name()).unwrap_or("Unknown").to_string();
                        app.push_incoming(&from_peer, ts, sender, txt, highlight);
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