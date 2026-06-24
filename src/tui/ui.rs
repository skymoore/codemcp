//! Ratatui rendering for the admin TUI: a two-pane servers/tools view with a
//! status/footer bar and a help overlay.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::{App, Pane};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);
    let body = chunks[0];
    let footer = chunks[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Min(1)])
        .split(body);
    render_servers(frame, app, cols[0]);
    render_tools(frame, app, cols[1]);
    render_footer(frame, app, footer);

    if app.show_help {
        render_help(frame, area);
    }
}

fn active_border(active: bool) -> Color {
    if active {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

fn render_servers(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(active_border(app.pane == Pane::Servers)))
        .title(" Servers ");

    let items: Vec<ListItem> = app
        .servers
        .iter()
        .map(|s| {
            let mark = if s.connected { "●" } else { "○" };
            let def = if s.default { "on" } else { "off" };
            let text = format!(
                "{} {:<22} {:<6} def:{} {:>2}t",
                mark, s.name, s.kind, def, s.tools
            );
            let style = if s.connected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.server_sel));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_tools(frame: &mut Frame, app: &App, area: Rect) {
    let srv = app.selected_server_name().unwrap_or("(none)");
    let title = format!(" Tools — {} ", srv);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(active_border(app.pane == Pane::Tools)))
        .title(title);

    let tools = app.tools_for_selected();
    let items: Vec<ListItem> = if tools.is_empty() {
        vec![ListItem::new(
            "  (no tools — server disconnected or all disabled)",
        )]
    } else {
        tools
            .iter()
            .map(|t| {
                let on = if t.enabled { "ON " } else { "off" };
                let marker = match t.session {
                    Some(_) => "(s)",
                    None if !t.default => "(d)",
                    _ => "   ",
                };
                let text = format!("{} {} {:<28} {}", on, marker, t.tool, t.summary);
                let style = if t.enabled {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };
                ListItem::new(text).style(style)
            })
            .collect()
    };

    let mut state = ListState::default();
    state.select(Some(app.tool_sel));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let status = format!(
        "[{}] pid {} {}",
        app.instance.launcher, app.instance.pid, app.instance.config
    );
    let keys = "Space:session  D:default  Tab:pane  j/k:move  r:refresh  ?:help  q:quit";

    let line1 = if let Some(e) = &app.error {
        Line::from(vec![
            Span::raw(format!("{status}  ")),
            Span::styled(format!("error: {e}"), Style::default().fg(Color::Red)),
        ])
    } else {
        Line::from(vec![
            Span::raw(format!("{status}  ")),
            Span::styled(app.message.clone(), Style::default().fg(Color::Yellow)),
        ])
    };
    let line2 = Line::from(keys);

    let p = Paragraph::new(vec![line1, line2]).block(block);
    frame.render_widget(p, area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(area, 58, 18);
    let lines = vec![
        Line::from("codemcp tui — keybindings").alignment(Alignment::Center),
        Line::from(""),
        Line::from(" j / ↑       move up"),
        Line::from(" k / ↓       move down"),
        Line::from(" Tab         switch pane (servers <-> tools)"),
        Line::from(" Space       toggle session state (this run only)"),
        Line::from(" D           toggle + persist default to mcp.json"),
        Line::from(" Enter       toggle session state"),
        Line::from(" r           refresh now"),
        Line::from(" ?           toggle this help"),
        Line::from(" q / Ctrl+C  quit"),
        Line::from(""),
        Line::from("Left: servers (● connected, ○ disconnected)."),
        Line::from("Right: tools of the selected server."),
        Line::from("ON/off = effective state; (s)=session override, (d)=default off."),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Help "))
        .alignment(Alignment::Left);
    frame.render_widget(Clear, popup);
    frame.render_widget(p, popup);
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(h) / 2),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .split(area);
    let hz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(area.width.saturating_sub(w) / 2),
            Constraint::Length(w),
            Constraint::Min(0),
        ])
        .split(v[1]);
    hz[1]
}
