use ratatui::prelude::*;
use ratatui::symbols;
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Sparkline};
use tokscale_core::{ClientId, CodexActivityPhase};

use super::widgets::{format_tokens, get_model_color, get_provider_display_name};
use crate::tui::app::App;

pub fn render(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(
            " live // codex monitor ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(app.theme.background));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content = centered_content_rect(inner, 156);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Min(0),
        ])
        .split(content);

    render_main_panel(frame, app, chunks[0]);
    render_focus_strip(frame, app, chunks[1]);
    render_trace_deck(frame, app, chunks[2]);
}

fn render_main_panel(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    render_throughput_chart(frame, app, chunks[0]);
    render_live_gauges(frame, app, chunks[1]);
}

fn render_throughput_chart(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(
            " throughput ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chart_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(2)])
        .split(inner);

    let history: Vec<u64> = if app.now_global_history.is_empty() {
        vec![0]
    } else {
        app.now_global_history.iter().copied().collect()
    };
    let max_value = history.iter().copied().max().unwrap_or(1).max(1);

    let sparkline = Sparkline::default()
        .data(&history)
        .max(max_value)
        .bar_set(symbols::bar::NINE_LEVELS)
        .style(Style::default().fg(app.theme.highlight))
        .absent_value_style(Style::default().fg(app.theme.border));
    frame.render_widget(sparkline, chart_chunks[0]);

    let total_recent: u64 = app
        .data
        .now_sessions
        .iter()
        .map(|session| session.recent_tokens)
        .sum();
    let lead = app.get_sorted_now_sessions().first().copied();
    let footer = vec![Line::from(vec![
        Span::styled("burst ", Style::default().fg(app.theme.muted)),
        Span::styled(
            format_tokens(total_recent),
            Style::default()
                .fg(app.theme.foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  •  ", Style::default().fg(app.theme.border)),
        Span::styled("lead ", Style::default().fg(app.theme.muted)),
        Span::styled(
            truncate(lead.map(|s| s.model.as_str()).unwrap_or("-"), 20),
            Style::default().fg(app.theme.highlight),
        ),
        Span::styled("  •  ", Style::default().fg(app.theme.border)),
        Span::styled("phase ", Style::default().fg(app.theme.muted)),
        Span::styled(
            phase_label(lead.map(|s| s.phase).unwrap_or(CodexActivityPhase::Idle)),
            Style::default()
                .fg(phase_color(
                    lead.map(|s| s.phase).unwrap_or(CodexActivityPhase::Idle),
                ))
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    frame.render_widget(Paragraph::new(footer), chart_chunks[1]);
}

fn render_live_gauges(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(
            " live gauges ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    let total_recent: u64 = app
        .data
        .now_sessions
        .iter()
        .map(|session| session.recent_tokens)
        .sum();
    let max_recent = app
        .now_global_history
        .iter()
        .copied()
        .max()
        .unwrap_or(1)
        .max(1);
    let live_ratio = ((app.data.now_sessions.len() as f64) / 6.0).min(1.0);
    let burst_ratio = (total_recent as f64 / max_recent as f64).min(1.0);
    let freshness_ratio = freshness_ratio(app);

    frame.render_widget(
        gauge(
            "Sessions",
            live_ratio,
            format!("{} live", app.data.now_sessions.len()),
            app.theme.highlight,
        ),
        chunks[0],
    );
    frame.render_widget(
        gauge(
            "Burst",
            burst_ratio,
            format_tokens(total_recent),
            app.theme.accent,
        ),
        chunks[1],
    );
    frame.render_widget(
        gauge(
            "Fresh",
            freshness_ratio,
            format!("{}s", app.last_now_refresh.elapsed().as_secs()),
            app.theme.foreground,
        ),
        chunks[2],
    );

    let updated = app
        .data
        .now_updated_at_ms
        .map(format_updated_at)
        .unwrap_or_else(|| "-".to_string());
    let lead_repo = app
        .get_sorted_now_sessions()
        .first()
        .and_then(|session| session.repo_name.as_deref())
        .map(|repo| truncate(repo, 18))
        .unwrap_or_else(|| "-".to_string());
    let meta = vec![
        Line::from(vec![
            Span::styled("updated ", Style::default().fg(app.theme.muted)),
            Span::styled(updated, Style::default().fg(app.theme.foreground)),
        ]),
        Line::from(vec![
            Span::styled("root    ", Style::default().fg(app.theme.muted)),
            Span::styled(lead_repo, Style::default().fg(app.theme.foreground)),
        ]),
    ];
    frame.render_widget(Paragraph::new(meta), chunks[3]);
}

fn render_focus_strip(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = if app.is_narrow() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(32),
                Constraint::Percentage(34),
                Constraint::Percentage(34),
            ])
            .split(area)
    };

    let lead = app.get_sorted_now_sessions().first().copied();
    let model_label = lead
        .map(|session| truncate(&session.model, 24))
        .unwrap_or_else(|| "-".to_string());
    let repo_label = lead
        .map(|session| {
            truncate(
                session
                    .repo_name
                    .as_deref()
                    .or(session.cwd.as_deref())
                    .unwrap_or("-"),
                24,
            )
        })
        .unwrap_or_else(|| "-".to_string());
    let provider_label = lead
        .map(|session| get_provider_display_name(&session.provider))
        .unwrap_or_else(|| "-".to_string());

    let model_color = lead
        .map(|session| get_model_color(&session.model))
        .unwrap_or(app.theme.foreground);

    render_focus_box(frame, chunks[0], " lead model ", &model_label, model_color);
    render_focus_box(
        frame,
        chunks[1],
        " worktree ",
        &repo_label,
        app.theme.highlight,
    );
    render_focus_box(
        frame,
        chunks[2],
        " provider ",
        &provider_label,
        app.theme.foreground,
    );
}

fn render_focus_box(frame: &mut Frame, area: Rect, title: &str, value: &str, color: Color) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(value.to_string())
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center),
        inner,
    );
}

fn render_trace_deck(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(
            " active traces ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.data.now_sessions.is_empty() {
        frame.render_widget(Clear, inner);
        frame.render_widget(
            Paragraph::new(get_empty_message(app))
                .style(Style::default().fg(app.theme.muted))
                .alignment(Alignment::Center),
            inner,
        );
        app.max_visible_items = 1;
        return;
    }

    let session_count = app.data.now_sessions.len();
    let card_height = if session_count <= 3 { 7 } else { 6 };
    let columns = if app.terminal_width >= 170 && session_count >= 3 {
        2
    } else if session_count <= 3 || app.terminal_width < 120 {
        1
    } else {
        2
    };
    let rows_visible = (inner.height / card_height).max(1) as usize;
    let visible_cards = (rows_visible * columns).max(1);
    app.max_visible_items = visible_cards;

    let sessions = app.get_sorted_now_sessions();

    let mut start = app.scroll_offset.min(
        sessions
            .len()
            .saturating_sub(visible_cards.min(sessions.len())),
    );
    if columns > 1 {
        start -= start % columns;
    }
    let end = (start + visible_cards).min(sessions.len());
    let visible = &sessions[start..end];

    let row_constraints = vec![Constraint::Length(card_height); rows_visible];
    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(inner);

    for (row_index, row_area) in row_chunks.iter().enumerate() {
        let base = row_index * columns;
        if base >= visible.len() {
            break;
        }

        let col_chunks = if columns == 2 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(*row_area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(100)])
                .split(*row_area)
        };

        for col_index in 0..columns {
            let slot = base + col_index;
            if slot >= visible.len() {
                break;
            }
            render_trace_card(
                frame,
                app,
                col_chunks[col_index],
                visible[slot],
                start + slot,
            );
        }
    }

    if sessions.len() > end {
        let footer = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("+", Style::default().fg(app.theme.muted)),
                Span::styled(
                    (sessions.len() - end).to_string(),
                    Style::default().fg(app.theme.foreground),
                ),
                Span::styled(" more traces", Style::default().fg(app.theme.muted)),
            ]))
            .alignment(Alignment::Right),
            footer,
        );
    }
}

fn render_trace_card(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    session: &crate::tui::data::CurrentSession,
    absolute_index: usize,
) {
    let is_selected = absolute_index == app.selected_index;
    let color = phase_color(session.phase);
    let border = if is_selected { app.theme.accent } else { color };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {} ", truncate(&session.model, 22)),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(if is_selected {
            app.theme.selection
        } else {
            app.theme.background
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(inner);

    let repo = session
        .repo_name
        .as_deref()
        .or(session.cwd.as_deref())
        .unwrap_or("-");
    let header = vec![Line::from(vec![
        Span::styled(
            truncate(repo, 34),
            Style::default().fg(app.theme.foreground),
        ),
        Span::styled("  •  ", Style::default().fg(app.theme.border)),
        Span::styled(
            phase_label(session.phase),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])];
    frame.render_widget(Paragraph::new(header), chunks[0]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("current ", Style::default().fg(app.theme.muted)),
            Span::styled(
                render_current_wave(session, app.spinner_frame, chunks[1].width as usize),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[1],
    );

    let bottom_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(chunks[2]);

    let left = vec![Line::from(vec![
        Span::styled("burst ", Style::default().fg(app.theme.muted)),
        Span::styled(
            format_tokens(session.recent_tokens),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  total ", Style::default().fg(app.theme.muted)),
        Span::styled(
            format_tokens(session.total_tokens),
            Style::default().fg(app.theme.foreground),
        ),
    ])];
    frame.render_widget(Paragraph::new(left), bottom_chunks[0]);

    let history = app
        .now_session_history
        .get(&session.session_id)
        .cloned()
        .unwrap_or_default();
    let history_vec: Vec<u64> = if history.is_empty() {
        vec![0]
    } else {
        history.iter().copied().collect()
    };
    let spark = Sparkline::default()
        .data(&history_vec)
        .max(history_vec.iter().copied().max().unwrap_or(1).max(1))
        .bar_set(symbols::bar::NINE_LEVELS)
        .style(Style::default().fg(color))
        .absent_value_style(Style::default().fg(app.theme.border));
    let history_block = Block::default().title(Span::styled(
        "history",
        Style::default().fg(app.theme.muted),
    ));
    let history_inner = history_block.inner(bottom_chunks[1]);
    frame.render_widget(history_block, bottom_chunks[1]);
    frame.render_widget(spark, history_inner);
}

fn gauge<'a>(title: &'a str, ratio: f64, label: String, color: Color) -> Gauge<'a> {
    Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(Color::DarkGray))),
        )
        .ratio(ratio.clamp(0.0, 1.0))
        .use_unicode(true)
        .label(label)
        .gauge_style(Style::default().fg(color).bg(Color::Rgb(18, 24, 20)))
}

fn freshness_ratio(app: &App) -> f64 {
    let age = app.last_now_refresh.elapsed().as_secs_f64();
    (1.0 - (age / 6.0)).clamp(0.0, 1.0)
}

fn render_current_wave(
    session: &crate::tui::data::CurrentSession,
    spinner_frame: usize,
    width: usize,
) -> String {
    let usable = width.saturating_sub(8).max(8);
    let amplitude = current_wave_amplitude(session);
    let center = usable / 2;
    let phase = spinner_frame % 4;
    let left_peak = center.saturating_sub(amplitude.min(center.saturating_sub(1)));
    let right_peak = (center + amplitude).min(usable.saturating_sub(1));

    let mut chars = vec!['─'; usable];
    chars[center] = '│';

    let shift = phase.min(amplitude);
    let pulse_left = left_peak.saturating_add(shift);
    let pulse_right = right_peak.saturating_sub(shift);
    chars[pulse_left.min(usable - 1)] = '╭';
    chars[center] = '│';
    chars[pulse_right.min(usable - 1)] = '╯';

    chars.into_iter().collect()
}

fn current_wave_amplitude(session: &crate::tui::data::CurrentSession) -> usize {
    let phase_amp = match session.phase {
        CodexActivityPhase::Streaming => 5,
        CodexActivityPhase::Settling => 4,
        CodexActivityPhase::Preparing => 3,
        CodexActivityPhase::Idle => 1,
    };
    let token_amp = match session.recent_tokens {
        0 => 1,
        1..=999 => 2,
        1000..=9999 => 3,
        10000..=99999 => 4,
        _ => 5,
    };
    phase_amp.max(token_amp)
}

fn get_empty_message(app: &App) -> String {
    let enabled_clients = app.enabled_clients.borrow();
    if !enabled_clients.contains(&ClientId::Codex) {
        "Codex is filtered out.\nPress 's' to enable it.".to_string()
    } else {
        "No live Codex traces in the last 10 minutes.\nKeep a Codex CLI session active and this panel will fill in.".to_string()
    }
}

fn phase_color(phase: CodexActivityPhase) -> Color {
    match phase {
        CodexActivityPhase::Streaming => Color::Rgb(0, 255, 163),
        CodexActivityPhase::Settling => Color::Rgb(255, 207, 90),
        CodexActivityPhase::Preparing => Color::Rgb(95, 195, 255),
        CodexActivityPhase::Idle => Color::Rgb(112, 123, 140),
    }
}

fn phase_label(phase: CodexActivityPhase) -> &'static str {
    match phase {
        CodexActivityPhase::Streaming => "streaming",
        CodexActivityPhase::Settling => "settling",
        CodexActivityPhase::Preparing => "preparing",
        CodexActivityPhase::Idle => "idle",
    }
}

fn format_updated_at(timestamp_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else if max_chars <= 3 {
        s.chars().take(max_chars).collect()
    } else {
        let head: String = s.chars().take(max_chars - 3).collect();
        format!("{}...", head)
    }
}

fn centered_content_rect(area: Rect, max_width: u16) -> Rect {
    if area.width <= max_width {
        area
    } else {
        let horizontal_margin = (area.width - max_width) / 2;
        Rect::new(
            area.x + horizontal_margin,
            area.y,
            max_width,
            area.height,
        )
    }
}
