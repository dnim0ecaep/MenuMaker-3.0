use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin};
use ratatui::prelude::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const MAX_COLUMNS: u16 = 6;
const CUSTOM_THEME_KEY: &str = "custom";
const SAVED_THEME_PREFIX: &str = "saved:";

fn main() -> Result<()> {
    let mut app = AppState::new()?;
    run_app(&mut app)
}

fn run_app(app: &mut AppState) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let result = event_loop(&mut terminal, app);

    restore_terminal(&mut terminal)?;
    result
}

fn event_loop<B>(terminal: &mut Terminal<B>, app: &mut AppState) -> Result<()>
where
    B: ratatui::backend::Backend + Write,
{
    let tick_rate = Duration::from_millis(200);
    loop {
        terminal.draw(|frame| render(frame, app))?;

        if event::poll(tick_rate)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    app.handle_mouse(mouse, size);
                }
                Event::Resize(_, _) => {}
                Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            };
        }

        if let Some(pending) = app.take_pending_command() {
            match run_command(terminal, &pending) {
                Ok(code) => {
                    app.set_status(Some(format!(
                        "Command exited with status {}",
                        code.unwrap_or_default()
                    )));
                }
                Err(err) => app.set_status(Some(format!("Command failed: {err}"))),
            }
        }

        if let Some(action) = app.take_pending_action() {
            app.execute_deferred_action(terminal, action)?;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn restore_terminal<B>(terminal: &mut Terminal<B>) -> Result<()>
where
    B: ratatui::backend::Backend + Write,
{
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn render(frame: &mut Frame, app: &AppState) {
    let size = frame.size();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.background)),
        size,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(size);

    let header = Paragraph::new(app.title.clone())
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(app.theme.text)
                .bg(app.theme.primary)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(header, chunks[0]);

    let shortcuts_bg = color_from_hex("#76B3C5").unwrap_or(app.theme.highlight);
    let shortcuts = Paragraph::new(app.footer_line())
        .alignment(Alignment::Center)
        .style(Style::default().bg(shortcuts_bg));
    frame.render_widget(shortcuts, chunks[1]);

    let content_area = chunks[2];
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.surface)),
        content_area,
    );
    render_columns(
        frame,
        content_area.inner(&Margin {
            vertical: 1,
            horizontal: 1,
        }),
        app,
    );

    let status = Paragraph::new(app.status_text())
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .bg(app.theme.primary)
                .fg(app.theme.text)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(status, chunks[3]);

    if let Some(popup) = &app.active_popup {
        render_popup(frame, popup, app);
    }
}

fn render_columns(frame: &mut Frame, area: Rect, app: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let column_count = app.column_count.max(1);
    let constraints = (0..column_count)
        .map(|_| Constraint::Ratio(1, column_count as u32))
        .collect::<Vec<_>>();
    let column_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (col_idx, chunk) in column_chunks.iter().enumerate() {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(entries) = app.column_map.get(col_idx) {
            for entry_index in entries {
                let (line, style) = app.entry_line(*entry_index);
                let (mut display_line, mut entry_style) = (line, style);
                if *entry_index == app.current_index {
                    entry_style = entry_style
                        .bg(app.theme.highlight)
                        .fg(app.theme.background)
                        .add_modifier(Modifier::BOLD);
                    display_line = app.highlight_entry_line(display_line);
                }
                items.push(ListItem::new(display_line).style(entry_style));
            }
        }

        if items.is_empty() {
            items.push(ListItem::new(""));
        }

        let list = List::new(items).block(
            Block::default().style(Style::default().bg(app.theme.surface).fg(app.theme.text)),
        );
        frame.render_widget(list, *chunk);
    }
}

fn render_popup(frame: &mut Frame, popup: &PopupState, app: &AppState) {
    match popup {
        PopupState::Info(info) => {
            let area = centered_rect(frame.size(), 60, 40);
            frame.render_widget(Clear, area);
            let text = format!(
                "Label: {}\nCommand: {}\nCategory: {}\nDescription: {}\n\nPress Enter or Esc to close.",
                info.label, info.command, info.category, info.description
            );
            let block = Paragraph::new(text)
                .style(Style::default().bg(app.theme.surface).fg(app.theme.text))
                .block(
                    Block::default()
                        .title("Item Info")
                        .borders(Borders::ALL)
                        .style(Style::default().bg(app.theme.surface)),
                );
            frame.render_widget(block, area);
        }
        PopupState::Message(msg) => {
            let area = centered_rect(frame.size(), 50, 30);
            frame.render_widget(Clear, area);
            let block = Paragraph::new(format!("{msg}\n\nPress Enter or Esc to close."))
                .style(Style::default().bg(app.theme.surface).fg(app.theme.text))
                .block(
                    Block::default()
                        .title("Message")
                        .borders(Borders::ALL)
                        .style(Style::default().bg(app.theme.surface)),
                );
            frame.render_widget(block, area);
        }
        PopupState::ItemForm(form) => {
            let area = frame.size();
            frame.render_widget(Clear, area);
            render_item_form_popup(frame, area, app, form);
        }
        PopupState::CategoryForm(form) => {
            let area = frame.size();
            frame.render_widget(Clear, area);
            render_category_form_popup(frame, area, app, form);
        }
        PopupState::SettingsForm(form) => {
            let area = frame.size();
            frame.render_widget(Clear, area);
            render_settings_form_popup(frame, area, app, form);
        }
    }
}

fn popup_sections(area: Rect) -> Option<[Rect; 4]> {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    if sections.len() < 4 {
        return None;
    }
    Some([sections[0], sections[1], sections[2], sections[3]])
}

fn popup_content_margin() -> Margin {
    Margin {
        horizontal: 3,
        vertical: 1,
    }
}

fn render_category_form_popup(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    form: &CategoryFormState,
) {
    let (lines, layout) = form.render_lines(app);
    if let Some(sections) = popup_sections(area) {
        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.background)),
            area,
        );
        let [header_area, shortcuts_area, content_area, status_area] = sections;
        let header = Paragraph::new(format!("{} - Edit Category", app.title))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, header_area);

        let shortcut_line = layout
            .shortcut_line
            .clone()
            .unwrap_or_else(|| Line::from(""));
        let shortcuts = Paragraph::new(shortcut_line)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.highlight)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(shortcuts, shortcuts_area);

        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.surface)),
            content_area,
        );
        let inner = content_area.inner(&popup_content_margin());
        let rendered_lines = materialize_form_lines(&lines, inner.width as usize, app);
        let paragraph = Paragraph::new(rendered_lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(app.theme.surface).fg(app.theme.text));
        frame.render_widget(paragraph, inner);

        let status = Paragraph::new(app.status_text())
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(status, status_area);
    }
}

fn render_item_form_popup(frame: &mut Frame, area: Rect, app: &AppState, form: &ItemFormState) {
    let mut lines: Vec<FormLine> = Vec::new();
    lines.push(plain_line(Line::from("Fill in the menu item details below.")));
    lines.push(make_field_line(
        "Label",
        &form.label,
        form.selected_field == ItemField::Label,
        app,
    ));
    lines.push(make_field_line(
        "Command",
        &form.command,
        form.selected_field == ItemField::Command,
        app,
    ));
    lines.push(make_field_line(
        "Description",
        &form.info,
        form.selected_field == ItemField::Description,
        app,
    ));
    lines.push(make_field_line(
        "Category",
        &form.category,
        form.selected_field == ItemField::Category,
        app,
    ));
    lines.push(make_toggle_line(
        "Pause After Run",
        form.pause,
        form.selected_field == ItemField::Pause,
        app,
    ));
    if let Some(error) = &form.error {
        lines.push(plain_line(Line::from(vec![Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )])));
    }
    if !form.available_categories.is_empty() {
        lines.push(plain_line(Line::from("")));
        lines.push(plain_line(Line::from(vec![Span::styled(
            "Available Categories:",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        )])));
        for category in &form.available_categories {
            lines.push(plain_line(Line::from(format!("  • {}", category))));
        }
    }

    let shortcut_line = Line::from(vec![
        Span::styled(
            "Tab",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("/"),
        Span::styled(
            "Shift+Tab",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Move    "),
        Span::styled(
            "Enter",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Save    "),
        Span::styled(
            "Esc",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Cancel    "),
        Span::styled(
            "Space",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Toggle Pause"),
    ]);

    if let Some(sections) = popup_sections(area) {
        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.background)),
            area,
        );
        let [header_area, shortcuts_area, content_area, status_area] = sections;
        let header = Paragraph::new(format!("{} - {}", app.title, form.mode_label))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, header_area);

        let shortcuts = Paragraph::new(shortcut_line)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.highlight)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(shortcuts, shortcuts_area);

        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.surface)),
            content_area,
        );
        let inner = content_area.inner(&popup_content_margin());
        let rendered_lines = materialize_form_lines(&lines, inner.width as usize, app);
        let paragraph = Paragraph::new(rendered_lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(app.theme.surface).fg(app.theme.text));
        frame.render_widget(paragraph, inner);

        let status = Paragraph::new(app.status_text())
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(status, status_area);
    }
}

fn render_settings_form_popup(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    form: &SettingsFormState,
) {
    let (lines, layout) = form.render_lines(app);
    if let Some(sections) = popup_sections(area) {
        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.background)),
            area,
        );
        let [header_area, shortcuts_area, content_area, status_area] = sections;
        let header = Paragraph::new(format!("{} - Application Settings", app.title))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, header_area);

        let shortcut_line = layout
            .shortcut_line
            .clone()
            .unwrap_or_else(|| Line::from(""));
        let shortcuts = Paragraph::new(shortcut_line)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.highlight)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(shortcuts, shortcuts_area);

        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.surface)),
            content_area,
        );
        let inner = content_area.inner(&popup_content_margin());
        let rendered_lines = materialize_form_lines(&lines, inner.width as usize, app);
        let paragraph = Paragraph::new(rendered_lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(app.theme.surface).fg(app.theme.text));
        frame.render_widget(paragraph, inner);

        let status = Paragraph::new(app.status_text())
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .bg(app.theme.primary)
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(status, status_area);
    }
}

fn build_category_shortcut_line(
    app: &AppState,
    include_delete: bool,
) -> (Line<'static>, Vec<CategoryShortcutSegment>, u16) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut segments: Vec<CategoryShortcutSegment> = Vec::new();
    let mut cursor: u16 = 0;
    let key_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(app.theme.surface);
    let entries: Vec<(&str, &str, CategoryShortcutAction)> = vec![
        ("Tab", " Move", CategoryShortcutAction::NextField),
        ("↵", " Save", CategoryShortcutAction::Submit),
        ("Esc", " Cancel", CategoryShortcutAction::Cancel),
    ];

    for (idx, (key, label, action)) in entries.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" | ", label_style));
            cursor = cursor.saturating_add(3);
        }
        let entry_start = cursor;
        spans.push(Span::styled(*key, key_style));
        cursor = cursor.saturating_add(key.chars().count() as u16);
        if !label.is_empty() {
            spans.push(Span::styled(*label, label_style));
            cursor = cursor.saturating_add(label.chars().count() as u16);
        }
        segments.push(CategoryShortcutSegment {
            start: entry_start,
            end: cursor,
            action: *action,
        });
    }

    if !entries.is_empty() {
        spans.push(Span::styled(" | ", label_style));
        cursor = cursor.saturating_add(3);
    }

    let left_start = cursor;
    spans.push(Span::styled("←", key_style));
    cursor = cursor.saturating_add("←".chars().count() as u16);
    segments.push(CategoryShortcutSegment {
        start: left_start,
        end: cursor,
        action: CategoryShortcutAction::PreviousPalette,
    });
    spans.push(Span::styled("/", label_style));
    cursor = cursor.saturating_add(1);
    let right_start = cursor;
    spans.push(Span::styled("→", key_style));
    cursor = cursor.saturating_add("→".chars().count() as u16);
    spans.push(Span::styled(" Select", label_style));
    cursor = cursor.saturating_add(" Select".len() as u16);
    segments.push(CategoryShortcutSegment {
        start: right_start,
        end: cursor,
        action: CategoryShortcutAction::NextPalette,
    });

    if include_delete {
        spans.push(Span::styled(" | ", label_style));
        cursor = cursor.saturating_add(3);
        let entry_start = cursor;
        spans.push(Span::styled("d", key_style));
        cursor = cursor.saturating_add(1);
        spans.push(Span::styled(" Delete Theme", label_style));
        cursor = cursor.saturating_add(" Delete Theme".len() as u16);
        segments.push(CategoryShortcutSegment {
            start: entry_start,
            end: cursor,
            action: CategoryShortcutAction::DeletePreset,
        });
    }

    (Line::from(spans), segments, cursor)
}

#[derive(Clone)]
struct FormLine {
    line: Line<'static>,
    highlight: bool,
}

impl FormLine {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            highlight: false,
        }
    }
    
    fn highlighted(line: Line<'static>) -> Self {
        Self {
            line,
            highlight: true,
        }
    }
}

fn materialize_form_lines(
    lines: &[FormLine],
    width: usize,
    app: &AppState,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|form_line| {
            if form_line.highlight {
                highlight_line_with_width(form_line.line.clone(), width, app)
            } else {
                form_line.line.clone()
            }
        })
        .collect()
}

fn highlight_line_with_width(
    mut line: Line<'static>,
    width: usize,
    app: &AppState,
) -> Line<'static> {
    let mut text_width = 0usize;
    let highlight_style = Style::default()
        .fg(app.theme.background)
        .bg(app.theme.highlight)
        .add_modifier(Modifier::BOLD);
    for span in &mut line.spans {
        span.style = highlight_style;
        text_width += UnicodeWidthStr::width(span.content.as_ref());
    }
    if width > text_width {
        line.spans
            .push(Span::styled(" ".repeat(width - text_width), highlight_style));
    }
    line
}

fn plain_line(line: impl Into<Line<'static>>) -> FormLine {
    FormLine::plain(line.into())
}

fn make_action_line(label: &str, selected: bool, app: &AppState) -> FormLine {
    let style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let line = Line::from(vec![Span::styled(label.to_string(), style)]);
    if selected {
        FormLine::highlighted(line)
    } else {
        FormLine::plain(line)
    }
}

fn make_field_line(label: &str, value: &str, selected: bool, app: &AppState) -> FormLine {
    let value_display = if value.trim().is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    };
    let label_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(app.theme.text);
    let label_span = Span::styled(format!("{label}: "), label_style);
    let value_span = Span::styled(value_display, value_style);
    if selected {
        FormLine::highlighted(Line::from(vec![label_span, value_span]))
    } else {
        FormLine::plain(Line::from(vec![label_span, value_span]))
    }
}

fn make_color_field_line(
    label: &str,
    value: &str,
    selected: bool,
    color: Option<Color>,
    app: &AppState,
) -> FormLine {
    let value_display = if value.trim().is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    };
    let label_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default()
        .fg(color.unwrap_or(app.theme.text))
        .add_modifier(Modifier::BOLD);
    let label_span = Span::styled(format!("{label}: "), label_style);
    let value_span = Span::styled(value_display, value_style);
    if selected {
        FormLine::highlighted(Line::from(vec![label_span, value_span]))
    } else {
        FormLine::plain(Line::from(vec![label_span, value_span]))
    }
}

fn make_toggle_line(label: &str, value: bool, selected: bool, app: &AppState) -> FormLine {
    let status = if value { "Yes" } else { "No" };
    let label_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let mut value_style = Style::default()
        .fg(if value { Color::Green } else { Color::Red })
        .add_modifier(Modifier::BOLD);
    let label_span = Span::styled(format!("{label}: "), label_style);
    let value_span = Span::styled(status, value_style);
    if selected {
        FormLine::highlighted(Line::from(vec![label_span, value_span]))
    } else {
        FormLine::plain(Line::from(vec![label_span, value_span]))
    }
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(area);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(horizontal[1]);
    vertical[1]
}

fn with_terminal_suspension<B, F, T>(terminal: &mut Terminal<B>, f: F) -> Result<T>
where
    B: ratatui::backend::Backend + Write,
    F: FnOnce() -> Result<T>,
{
    terminal.show_cursor()?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    let result = f();
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    result
}

fn run_command<B>(terminal: &mut Terminal<B>, pending: &PendingCommand) -> Result<Option<i32>>
where
    B: ratatui::backend::Backend + Write,
{
    with_terminal_suspension(terminal, || {
        let status = Command::new("sh").arg("-c").arg(&pending.command).status();

        let exit_code = match status {
            Ok(status) => {
                if pending.pause {
                    println!(
                        "\nCommand exited with code {:?}. Press Enter to return...",
                        status.code()
                    );
                    let _ = io::stdin().read_line(&mut String::new());
                }
                status.code()
            }
            Err(err) => {
                println!("Failed to run command: {err}");
                println!("Press Enter to continue...");
                let _ = io::stdin().read_line(&mut String::new());
                None
            }
        };
        Ok(exit_code)
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct NamedColorPair {
    name: Option<String>,
    background: Option<String>,
    text: Option<String>,
}

const DEFAULT_CATEGORY_COLOR_PRESETS: &[(&str, &str, &str)] = &[
    ("Teal Glow", "#034e68", "#caf0f8"),
    ("Amber Pop", "#6f1d1b", "#ffe5d9"),
    ("Purple Mist", "#240046", "#f8f9fa"),
    ("Forest Tones", "#283618", "#fefae0"),
    ("Slate Shine", "#2b2d42", "#edf2f4"),
];

#[derive(Clone)]
struct ColorPreset {
    name: String,
    background: String,
    text: String,
    custom_index: Option<usize>,
}

impl ColorPreset {
    fn new(name: impl Into<String>, background: &str, text: &str) -> Self {
        Self {
            name: name.into(),
            background: normalize_hex(background),
            text: normalize_hex(text),
            custom_index: None,
        }
    }

    fn from_custom(name: impl Into<String>, background: &str, text: &str, index: usize) -> Self {
        Self {
            name: name.into(),
            background: normalize_hex(background),
            text: normalize_hex(text),
            custom_index: Some(index),
        }
    }

    fn matches(&self, background: &str, text: &str) -> bool {
        let bg = normalize_hex(background);
        let txt = normalize_hex(text);
        self.background.eq_ignore_ascii_case(&bg) && self.text.eq_ignore_ascii_case(&txt)
    }
}

#[derive(Clone)]
struct ThemeOption {
    key: String,
    label: String,
    primary_hex: String,
    accent_hex: String,
    background_hex: String,
    surface_hex: String,
    text_hex: String,
    highlight_hex: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct AppSettings {
    title: Option<String>,
    columns: Option<u16>,
    #[serde(default)]
    theme_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct SavedTheme {
    name: String,
    primary: String,
    accent: String,
    background: String,
    surface: String,
    text: String,
    #[serde(default)]
    highlight: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct MenuFile {
    categories: BTreeMap<String, CategoryConfig>,
    #[serde(default)]
    app_settings: AppSettings,
    #[serde(default)]
    custom_colors: Vec<NamedColorPair>,
    #[serde(default)]
    saved_themes: Vec<SavedTheme>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CategoryConfig {
    #[serde(default = "default_true")]
    expanded: bool,
    column: Option<u16>,
    #[serde(default)]
    items: Vec<MenuItemConfig>,
    #[serde(default)]
    colors: Option<ColorConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ColorConfig {
    background: Option<String>,
    text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct MenuItemConfig {
    label: String,
    cmd: String,
    info: Option<String>,
    category: Option<String>,
    pause: Option<bool>,
}

fn default_true() -> bool {
    true
}

fn default_saved_theme() -> SavedTheme {
    let base_theme = Theme::from_name("nord").unwrap_or_else(|| {
        Theme::from_hexes(
            "default".to_string(),
            "#5E81AC",
            "#D08770",
            "#76B3C5",
            "#3B4252",
            "#4C566A",
            "#ECEFF4",
        )
    });
    SavedTheme {
        name: "default".to_string(),
        primary: base_theme.primary_hex.clone(),
        accent: base_theme.accent_hex.clone(),
        highlight: Some(base_theme.highlight_hex.clone()),
        background: base_theme.background_hex.clone(),
        surface: base_theme.surface_hex.clone(),
        text: base_theme.text_hex.clone(),
    }
}

impl MenuFile {
    fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = fs::read_to_string(path)?;
            let parsed: MenuFile = serde_json::from_str(&data)?;
            Ok(parsed)
        } else {
            let default = Self::default_data();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, serde_json::to_string_pretty(&default)?)?;
            Ok(default)
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        fs::write(path, data)?;
        Ok(())
    }

    fn default_data() -> Self {
        let mut categories = BTreeMap::new();
        categories.insert(
            "System Tools".to_string(),
            CategoryConfig {
                expanded: true,
                column: Some(1),
                items: vec![MenuItemConfig {
                    label: "System Monitor".into(),
                    cmd: "htop".into(),
                    info: Some("Interactive process viewer".into()),
                    category: Some("System Tools".into()),
                    pause: Some(false),
                }],
                colors: None,
            },
        );
        let saved_themes = vec![default_saved_theme()];

        MenuFile {
            categories,
            app_settings: AppSettings {
                title: Some("Menu Maker — Enhanced Categorized Menu System".into()),
                columns: Some(1),
                theme_key: Some(saved_theme_key(0)),
            },
            custom_colors: Vec::new(),
            saved_themes,
        }
    }
}

struct AppPaths {
    config_dir: PathBuf,
    menu_file: PathBuf,
    theme_file: PathBuf,
}

impl AppPaths {
    fn new() -> Result<Self> {
        let home = dirs::home_dir().context("Unable to determine home directory")?;
        let config_dir = home.join(".local/menu-maker");
        fs::create_dir_all(&config_dir)?;
        Ok(Self {
            menu_file: config_dir.join("menus.json"),
            theme_file: config_dir.join("theme.json"),
            config_dir,
        })
    }
}

struct AppState {
    categories: Vec<CategoryState>,
    custom_colors: Vec<NamedColorPair>,
    saved_themes: Vec<SavedTheme>,
    column_count: u16,
    current_index: usize,
    display_entries: Vec<DisplayEntry>,
    column_map: Vec<Vec<usize>>,
    should_quit: bool,
    pending_command: Option<PendingCommand>,
    pending_action: Option<DeferredAction>,
    status_message: Option<String>,
    paths: AppPaths,
    theme: Theme,
    theme_key: String,
    title: String,
    active_popup: Option<PopupState>,
}

impl AppState {
    fn resolve_theme_key(
        stored: Option<String>,
        theme: &Theme,
        saved_themes: &[SavedTheme],
    ) -> String {
        if let Some(key) = stored {
            if key == CUSTOM_THEME_KEY || is_preset_theme_key(&key) {
                return key;
            }
            if let Some(idx) = parse_saved_theme_key(&key) {
                if idx < saved_themes.len() {
                    return key;
                }
            }
        }
        if let Some(idx) = saved_themes
            .iter()
            .position(|saved| saved.name == theme.name)
        {
            return saved_theme_key(idx);
        }
        if is_preset_theme_key(&theme.name) {
            return theme.name.clone();
        }
        CUSTOM_THEME_KEY.to_string()
    }
    fn new() -> Result<Self> {
        let paths = AppPaths::new()?;
        let mut menu_file = MenuFile::load(&paths.menu_file)?;
        if !menu_file
            .saved_themes
            .iter()
            .any(|saved| saved.name.eq_ignore_ascii_case("default"))
        {
            menu_file.saved_themes.push(default_saved_theme());
            let _ = menu_file.save(&paths.menu_file);
        }
        let theme = Theme::load(&paths.theme_file)?;
        let saved_themes = menu_file.saved_themes.clone();

        let mut categories: Vec<CategoryState> = menu_file
            .categories
            .iter()
            .map(|(name, cfg)| CategoryState::from_config(name, cfg))
            .collect();
        categories.sort_by_key(|cat| (cat.column, cat.name.clone()));

        let mut column_count = menu_file
            .app_settings
            .columns
            .unwrap_or(1)
            .clamp(1, MAX_COLUMNS);
        if column_count == 0 {
            column_count = 1;
        }

        let stored_theme_key = AppState::resolve_theme_key(
            menu_file.app_settings.theme_key.clone(),
            &theme,
            &saved_themes,
        );
        let resolved_theme = if stored_theme_key == CUSTOM_THEME_KEY {
            theme.clone()
        } else if let Some(idx) = parse_saved_theme_key(&stored_theme_key) {
            saved_themes.get(idx).and_then(|saved| {
                let highlight = saved
                    .highlight
                    .as_deref()
                    .unwrap_or_else(|| saved.accent.as_str());
                Some(Theme::from_hexes(
                    saved.name.clone(),
                    &saved.primary,
                    &saved.accent,
                    highlight,
                    &saved.background,
                    &saved.surface,
                    &saved.text,
                ))
            })
            .unwrap_or_else(|| theme.clone())
        } else if is_preset_theme_key(&stored_theme_key) {
            Theme::from_name(&stored_theme_key).unwrap_or_else(|| theme.clone())
        } else {
            theme.clone()
        };

        let mut app = AppState {
            categories,
            custom_colors: menu_file.custom_colors,
            saved_themes,
            column_count,
            current_index: 0,
            display_entries: Vec::new(),
            column_map: Vec::new(),
            should_quit: false,
            pending_command: None,
            pending_action: None,
            status_message: None,
            paths,
            theme_key: stored_theme_key,
            theme: resolved_theme,
            title: menu_file
                .app_settings
                .title
                .unwrap_or_else(|| "Menu Maker".into()),
            active_popup: None,
        };
        app.rebuild_display();
        Ok(app)
    }

    fn rebuild_display(&mut self) {
        self.sort_categories();
        self.display_entries.clear();
        let columns = self.column_count.max(1);
        self.column_map = vec![Vec::new(); columns as usize];
        for (idx, category) in self.categories.iter().enumerate() {
            let column_index = ((category.column.saturating_sub(1)) as usize)
                .min(self.column_map.len().saturating_sub(1));
            let entry_index = self.display_entries.len();
            self.display_entries.push(DisplayEntry::Category {
                category_index: idx,
            });
            self.column_map[column_index].push(entry_index);
            if category.expanded {
                for item_index in 0..category.items.len() {
                    let entry_index = self.display_entries.len();
                    self.display_entries.push(DisplayEntry::Item {
                        category_index: idx,
                        item_index,
                    });
                    self.column_map[column_index].push(entry_index);
                }
            }
        }
        if self.current_index >= self.display_entries.len() {
            self.current_index = self.current_index.saturating_sub(1);
            if self.display_entries.is_empty() {
                self.current_index = 0;
            }
        }
    }

    fn entry_line(&self, entry_index: usize) -> (Line<'_>, Style) {
        match &self.display_entries[entry_index] {
            DisplayEntry::Category { category_index } => {
                let category = &self.categories[*category_index];
                let marker = if category.expanded { "▼" } else { "▶" };
                let mut style = Style::default()
                    .fg(self.theme.text)
                    .bg(self.theme.surface)
                    .add_modifier(Modifier::BOLD);
                if let Some(colors) = &category.colors {
                    if let Some(bg) = colors
                        .background
                        .as_ref()
                        .and_then(|hex| color_from_hex(hex))
                    {
                        style = style.bg(bg);
                    }
                    if let Some(text) = colors.text.as_ref().and_then(|hex| color_from_hex(hex)) {
                        style = style.fg(text);
                    }
                }
                (Line::from(format!("{marker} {}", category.name)), style)
            }
            DisplayEntry::Item {
                category_index,
                item_index,
            } => {
                let item = &self.categories[*category_index].items[*item_index];
                let mut style = Style::default().fg(self.theme.text).bg(self.theme.surface);
                if let Some(colors) = self.categories[*category_index].colors.as_ref() {
                    if let Some(bg) = colors
                        .background
                        .as_ref()
                        .and_then(|hex| color_from_hex(hex))
                    {
                        style = style.bg(bg);
                    }
                    if let Some(text) = colors.text.as_ref().and_then(|hex| color_from_hex(hex)) {
                        style = style.fg(text);
                    }
                }
                (Line::from(format!("    {}", item.label)), style)
            }
        }
    }

    fn highlight_entry_line(&self, line: Line<'_>) -> Line<'static> {
        let mut spans = Vec::new();
        for span in line.spans {
            let mut owned = Span::styled(span.content.to_string(), span.style);
            owned.style = owned
                .style
                .fg(self.theme.background)
                .bg(self.theme.highlight)
                .add_modifier(Modifier::BOLD);
            spans.push(owned);
        }
        Line::from(spans)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.active_popup.is_some() {
            let result = {
                let popup = self.active_popup.as_mut().unwrap();
                match popup {
                    PopupState::Info(_) | PopupState::Message(_) => match key.code {
                        KeyCode::Esc | KeyCode::Enter => PopupResult::Close(None),
                        _ => PopupResult::None,
                    },
                    PopupState::ItemForm(form) => match form.handle_key(key) {
                        ItemFormKeyResult::Continue => PopupResult::None,
                        ItemFormKeyResult::Cancel => {
                            PopupResult::Close(Some("Item edit cancelled".into()))
                        }
                        ItemFormKeyResult::Submit(data) => PopupResult::ItemSubmit(data),
                    },
                    PopupState::CategoryForm(form) => match form.handle_key(key) {
                        FormKeyResult::Continue => PopupResult::None,
                        FormKeyResult::Cancel => {
                            PopupResult::Close(Some("Category edit cancelled".into()))
                        }
                        FormKeyResult::Submit(data) => PopupResult::CategorySubmit(data),
                        FormKeyResult::DeletePreset(index) => {
                            PopupResult::CategoryDeletePreset(index)
                        }
                    },
                    PopupState::SettingsForm(form) => match form.handle_key(key) {
                        SettingsFormKeyResult::Continue => PopupResult::None,
                        SettingsFormKeyResult::Cancel => {
                            PopupResult::Close(Some("Settings update cancelled".into()))
                        }
                        SettingsFormKeyResult::Submit(data) => PopupResult::SettingsSubmit(data),
                        SettingsFormKeyResult::DeleteSavedTheme(index) => {
                            PopupResult::SettingsDeleteSavedTheme(index)
                        }
                    },
                }
            };
            match result {
                PopupResult::None => {}
                PopupResult::Close(status) => {
                    self.active_popup = None;
                    if let Some(msg) = status {
                        self.set_status(Some(msg));
                    }
                }
                PopupResult::ItemSubmit(data) => match self.apply_item_form_input(data) {
                    Ok(msg) => {
                        self.active_popup = None;
                        self.set_status(Some(msg));
                    }
                    Err(err_msg) => {
                        if let Some(PopupState::ItemForm(form)) = self.active_popup.as_mut() {
                            form.error = Some(err_msg);
                        }
                    }
                },
                PopupResult::CategorySubmit(data) => match self.process_category_submission(data) {
                    Ok(msg) => {
                        self.active_popup = None;
                        self.set_status(Some(msg));
                    }
                    Err(err_msg) => {
                        if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                            form.error = Some(err_msg);
                        }
                    }
                },
                PopupResult::CategoryDeletePreset(index) => {
                    let result = self.delete_custom_category_preset(index);
                    self.handle_category_preset_delete_result(result);
                }
                PopupResult::SettingsSubmit(data) => match self.apply_settings_form_input(data) {
                    Ok(msg) => {
                        self.active_popup = None;
                        self.set_status(Some(msg));
                    }
                    Err(err_msg) => {
                        if let Some(PopupState::SettingsForm(form)) = self.active_popup.as_mut() {
                            form.error = Some(err_msg);
                        }
                    }
                },
                PopupResult::SettingsDeleteSavedTheme(index) => {
                    self.handle_saved_theme_deletion(index);
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection_down(),
            KeyCode::Enter => self.activate_current_entry(),
            KeyCode::Char(' ') => {
                self.toggle_category();
            }
            KeyCode::Char('r') => {
                if let Err(err) = self.reload_from_disk() {
                    self.set_status(Some(format!("Reload failed: {err}")));
                } else {
                    self.set_status(Some("Configuration reloaded".into()));
                }
            }
            KeyCode::Char('i') => self.show_info_popup(),
            KeyCode::Char('n') => self.queue_new_item(),
            KeyCode::Char('e') => self.queue_edit_current(),
            KeyCode::Char('d') => self.delete_selected_item(),
            KeyCode::Char('s') => self.queue_settings(),
            KeyCode::Char('t') => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.queue_settings_with_focus(SettingsField::Title);
                } else {
                    self.queue_settings_with_focus(SettingsField::Theme);
                }
            }
            KeyCode::Char('b') => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.run_bin_scan();
                }
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, terminal_area: Rect) {
        if self.active_popup.is_some() {
            if let Some(action) = self.detect_popup_click(mouse, terminal_area) {
                self.apply_popup_click(action);
            }
            return;
        }
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        if let Some(entry_index) = self.entry_at_position(mouse.column, mouse.row, terminal_area) {
            self.current_index = entry_index;
            match self.display_entries[entry_index] {
                DisplayEntry::Category { .. } => self.toggle_category(),
                DisplayEntry::Item { .. } => self.prepare_command(),
            }
            return;
        }

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(terminal_area);
        if layout.len() < 4 {
            return;
        }
        let footer_area = layout[1];
        if mouse.row >= footer_area.y
            && mouse.row < footer_area.y + footer_area.height
            && mouse.column >= footer_area.x
            && mouse.column < footer_area.x + footer_area.width
        {
            if self.handle_footer_click(mouse.column, footer_area) {
                return;
            }
        }
    }

    fn entry_at_position(&self, column: u16, row: u16, terminal_area: Rect) -> Option<usize> {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(terminal_area);
        if layout.len() < 3 {
            return None;
        }
        let content_area = layout[2].inner(&Margin {
            vertical: 1,
            horizontal: 1,
        });
        if content_area.width == 0 || content_area.height == 0 {
            return None;
        }
        if column < content_area.x
            || column >= content_area.x + content_area.width
            || row < content_area.y
            || row >= content_area.y + content_area.height
        {
            return None;
        }

        let column_count = self.column_count.max(1);
        let constraints = (0..column_count)
            .map(|_| Constraint::Ratio(1, column_count as u32))
            .collect::<Vec<_>>();
        let column_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(content_area);
        for (idx, chunk) in column_chunks.iter().enumerate() {
            if column < chunk.x
                || column >= chunk.x + chunk.width
                || row < chunk.y
                || row >= chunk.y + chunk.height
            {
                continue;
            }
            let entries = self.column_map.get(idx)?;
            if entries.is_empty() {
                return None;
            }
            let line = row.saturating_sub(chunk.y);
            let line_idx = usize::from(line);
            if line_idx >= entries.len() {
                return None;
            }
            return Some(entries[line_idx]);
        }
        None
    }

    fn handle_footer_click(&mut self, column: u16, footer_area: Rect) -> bool {
        let line_data = self.footer_line_data();
        if line_data.segments.is_empty() || line_data.total_width == 0 {
            return false;
        }
        if footer_area.width == 0 || footer_area.height == 0 {
            return false;
        }
        let text_width = line_data.total_width.min(footer_area.width);
        let mut start_x = footer_area.x;
        if footer_area.width > text_width {
            start_x += (footer_area.width - text_width) / 2;
        }
        if column < start_x || column >= start_x + text_width {
            return false;
        }
        let relative = column - start_x;
        for segment in line_data.segments {
            if relative >= segment.start && relative < segment.end {
                self.execute_footer_action(segment.action);
                return true;
            }
        }
        false
    }

    fn detect_popup_click(
        &self,
        mouse: MouseEvent,
        terminal_area: Rect,
    ) -> Option<PopupClickAction> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        let popup = self.active_popup.as_ref()?;
        match popup {
            PopupState::CategoryForm(form) => {
                let (_lines, layout) = form.render_lines(self);
                let Some([_, shortcut_area, content_area, _]) = popup_sections(terminal_area)
                else {
                    return None;
                };
                if mouse.column >= shortcut_area.x
                    && mouse.column < shortcut_area.x + shortcut_area.width
                    && mouse.row >= shortcut_area.y
                    && mouse.row < shortcut_area.y + shortcut_area.height
                {
                    if layout.shortcut_total_width == 0
                        || layout.shortcut_segments.is_empty()
                        || shortcut_area.width == 0
                    {
                        return None;
                    }
                    let text_width = layout.shortcut_total_width.min(shortcut_area.width);
                    if text_width == 0 {
                        return None;
                    }
                    let mut start_x = shortcut_area.x;
                    if shortcut_area.width > text_width {
                        start_x += (shortcut_area.width - text_width) / 2;
                    }
                    if mouse.column < start_x || mouse.column >= start_x + text_width {
                        return None;
                    }
                    let relative = mouse.column - start_x;
                    for segment in &layout.shortcut_segments {
                        if relative >= segment.start && relative < segment.end {
                            return Some(PopupClickAction::Category(CategoryFormClick::Shortcut(
                                segment.action,
                            )));
                        }
                    }
                    return None;
                }
                let inner = content_area.inner(&popup_content_margin());
                if inner.width == 0
                    || inner.height == 0
                    || mouse.column < inner.x
                    || mouse.column >= inner.x + inner.width
                    || mouse.row < inner.y
                    || mouse.row >= inner.y + inner.height
                {
                    return None;
                }
                let line_idx = usize::from(mouse.row.saturating_sub(inner.y));
                if line_idx >= layout.line_count {
                    return None;
                }
                if layout.name_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::Name,
                    )));
                }
                if layout.column_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::Column,
                    )));
                }
                if layout.custom_name_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::CustomPresetName,
                    )));
                }
                if layout.custom_background_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::CustomPresetBackground,
                    )));
                }
                if layout.custom_text_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::CustomPresetText,
                    )));
                }
                if layout.presets_heading_line == Some(line_idx) {
                    return Some(PopupClickAction::Category(CategoryFormClick::SelectField(
                        CategoryField::Palette,
                    )));
                }
                if let Some(start) = layout.presets_start_line {
                    if line_idx >= start && line_idx < start + layout.presets_count {
                        let palette_idx = line_idx - start;
                        return Some(PopupClickAction::Category(
                            CategoryFormClick::SelectPalette(palette_idx),
                        ));
                    }
                }
                None
            }
            PopupState::SettingsForm(form) => {
                let (_lines, layout) = form.render_lines(self);
                let Some([_, shortcut_area, content_area, _]) = popup_sections(terminal_area)
                else {
                    return None;
                };
                if mouse.column >= shortcut_area.x
                    && mouse.column < shortcut_area.x + shortcut_area.width
                    && mouse.row >= shortcut_area.y
                    && mouse.row < shortcut_area.y + shortcut_area.height
                {
                    if layout.shortcut_total_width == 0
                        || layout.shortcut_segments.is_empty()
                        || shortcut_area.width == 0
                    {
                        return None;
                    }
                    let text_width = layout.shortcut_total_width.min(shortcut_area.width);
                    if text_width == 0 {
                        return None;
                    }
                    let mut start_x = shortcut_area.x;
                    if shortcut_area.width > text_width {
                        start_x += (shortcut_area.width - text_width) / 2;
                    }
                    if mouse.column < start_x || mouse.column >= start_x + text_width {
                        return None;
                    }
                    let relative = mouse.column - start_x;
                    for segment in &layout.shortcut_segments {
                        if relative >= segment.start && relative < segment.end {
                            return Some(PopupClickAction::Settings(SettingsFormClick::Shortcut(
                                segment.action,
                            )));
                        }
                    }
                    return None;
                }
                let inner = content_area.inner(&popup_content_margin());
                if inner.width == 0
                    || inner.height == 0
                    || mouse.column < inner.x
                    || mouse.column >= inner.x + inner.width
                    || mouse.row < inner.y
                    || mouse.row >= inner.y + inner.height
                {
                    return None;
                }
                let line_idx = usize::from(mouse.row.saturating_sub(inner.y));
                if line_idx >= layout.line_count {
                    return None;
                }
                if layout.title_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::Title,
                    )));
                }
                if layout.columns_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::Columns,
                    )));
                }
                if layout.theme_heading_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::Theme,
                    )));
                }
                if let Some(start) = layout.theme_list_start {
                    if line_idx >= start && line_idx < start + layout.theme_count {
                        let theme_idx = line_idx - start;
                        return Some(PopupClickAction::Settings(SettingsFormClick::SelectTheme(
                            theme_idx,
                        )));
                    }
                }
                if layout.delete_saved_theme_line == Some(line_idx) {
                    if let Some(idx) = layout.delete_saved_theme_index {
                        return Some(PopupClickAction::Settings(
                            SettingsFormClick::DeleteSavedTheme(idx),
                        ));
                    }
                }
                if layout.custom_heading_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomName,
                    )));
                }
                if layout.custom_name_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomName,
                    )));
                }
                if layout.custom_primary_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomPrimary,
                    )));
                }
                if layout.custom_accent_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomAccent,
                    )));
                }
                if layout.custom_background_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomBackground,
                    )));
                }
                if layout.custom_surface_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomSurface,
                    )));
                }
                if layout.custom_text_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomText,
                    )));
                }
                if layout.custom_highlight_line == Some(line_idx) {
                    return Some(PopupClickAction::Settings(SettingsFormClick::SelectField(
                        SettingsField::CustomHighlight,
                    )));
                }
                None
            }
            _ => None,
        }
    }

    fn apply_popup_click(&mut self, action: PopupClickAction) {
        match action {
            PopupClickAction::Category(category_click) => {
                let mut pending_submit: Option<CategorySubmitPayload> = None;
                let mut pending_delete: Option<usize> = None;
                let mut pending_cancel = false;
                if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                    match category_click {
                        CategoryFormClick::SelectField(field) => {
                            form.selected_field = field;
                        }
                        CategoryFormClick::SelectPalette(index) => {
                            if index < form.color_presets.len() {
                                form.selected_field = CategoryField::Palette;
                                form.palette_index = index;
                                form.apply_selected_palette();
                            }
                        }
                        CategoryFormClick::Shortcut(action) => match action {
                            CategoryShortcutAction::NextField => {
                                form.error = None;
                                form.next_field();
                            }
                            CategoryShortcutAction::PreviousField => {
                                form.error = None;
                                form.previous_field();
                            }
                            CategoryShortcutAction::Submit => {
                                match form.build_submission() {
                                    Ok(input) => pending_submit = Some(input),
                                    Err(err) => form.error = Some(err),
                                }
                            }
                            CategoryShortcutAction::Cancel => {
                                pending_cancel = true;
                            }
                            CategoryShortcutAction::PreviousPalette => {
                                form.error = None;
                                form.previous_palette();
                            }
                            CategoryShortcutAction::NextPalette => {
                                form.error = None;
                                form.next_palette();
                            }
                            CategoryShortcutAction::DeletePreset => {
                                if let Some(index) = form.current_custom_preset_index() {
                                    pending_delete = Some(index);
                                } else {
                                    form.error = Some("Select a custom theme to delete".into());
                                }
                            }
                        },
                    }
                }
                if let Some(index) = pending_delete {
                    let result = self.delete_custom_category_preset(index);
                    self.handle_category_preset_delete_result(result);
                }
                if let Some(input) = pending_submit {
                    let submit_result = self.process_category_submission(input);
                    match submit_result {
                        Ok(msg) => {
                            self.active_popup = None;
                            self.set_status(Some(msg));
                        }
                        Err(err) => {
                            if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut()
                            {
                                form.error = Some(err);
                            }
                        }
                    }
                }
                if pending_cancel {
                    self.active_popup = None;
                    self.set_status(Some("Category edit cancelled".into()));
                }
            }
            PopupClickAction::Settings(settings_click) => {
                let mut pending_delete_theme: Option<usize> = None;
                if let Some(PopupState::SettingsForm(form)) = self.active_popup.as_mut() {
                    match settings_click {
                        SettingsFormClick::SelectField(field) => {
                            form.selected_field = field;
                        }
                        SettingsFormClick::SelectTheme(index) => {
                            if index < form.theme_options.len() {
                                form.selected_field = SettingsField::Theme;
                                if form.theme_index != index {
                                    form.theme_index = index;
                                    form.populate_custom_fields_from_selection();
                                }
                            }
                        }
                        SettingsFormClick::DeleteSavedTheme(index) => {
                            pending_delete_theme = Some(index);
                        }
                        SettingsFormClick::Shortcut(action) => match action {
                            SettingsShortcutAction::NextField => {
                                if let Some(PopupState::SettingsForm(form)) =
                                    self.active_popup.as_mut()
                                {
                                    form.error = None;
                                    form.next_field();
                                }
                            }
                            SettingsShortcutAction::Submit => {
                                let input = if let Some(PopupState::SettingsForm(form)) =
                                    self.active_popup.as_mut()
                                {
                                    form.error = None;
                                    form.to_input()
                                } else {
                                    return;
                                };
                                match self.apply_settings_form_input(input) {
                                    Ok(msg) => {
                                        self.active_popup = None;
                                        self.set_status(Some(msg));
                                    }
                                    Err(err) => {
                                        if let Some(PopupState::SettingsForm(form)) =
                                            self.active_popup.as_mut()
                                        {
                                            form.error = Some(err);
                                        }
                                    }
                                }
                            }
                            SettingsShortcutAction::Cancel => {
                                self.active_popup = None;
                                self.set_status(Some("Settings update cancelled".into()));
                            }
                            SettingsShortcutAction::PreviousTheme => {
                                if let Some(PopupState::SettingsForm(form)) =
                                    self.active_popup.as_mut()
                                {
                                    form.error = None;
                                    form.previous_theme();
                                }
                            }
                            SettingsShortcutAction::NextTheme => {
                                if let Some(PopupState::SettingsForm(form)) =
                                    self.active_popup.as_mut()
                                {
                                    form.error = None;
                                    form.next_theme();
                                }
                            }
                            SettingsShortcutAction::DeleteTheme => {
                                if let Some(PopupState::SettingsForm(form)) =
                                    self.active_popup.as_mut()
                                {
                                    if let Some(index) = form.current_deletable_theme_index() {
                                        pending_delete_theme = Some(index);
                                    } else {
                                        form.error =
                                            Some("Select a custom theme to delete".into());
                                    }
                                }
                            }
                        },
                    }
                }
                if let Some(index) = pending_delete_theme {
                    self.handle_saved_theme_deletion(index);
                }
            }
        }
    }

    fn reload_from_disk(&mut self) -> Result<()> {
        let menu_file = MenuFile::load(&self.paths.menu_file)?;
        self.theme = Theme::load(&self.paths.theme_file)?;
        self.saved_themes = menu_file.saved_themes;
        self.theme_key = AppState::resolve_theme_key(
            menu_file.app_settings.theme_key.clone(),
            &self.theme,
            &self.saved_themes,
        );
        self.categories = menu_file
            .categories
            .iter()
            .map(|(name, cfg)| CategoryState::from_config(name, cfg))
            .collect();
        self.categories
            .sort_by_key(|category| (category.column, category.name.clone()));
        self.custom_colors = menu_file.custom_colors;
        self.column_count = menu_file
            .app_settings
            .columns
            .unwrap_or(self.column_count)
            .clamp(1, MAX_COLUMNS);
        if let Some(title) = menu_file.app_settings.title {
            self.title = title;
        }
        self.rebuild_display();
        Ok(())
    }

    fn toggle_category(&mut self) {
        if let Some(DisplayEntry::Category { category_index }) =
            self.display_entries.get(self.current_index)
        {
            if let Some(category) = self.categories.get_mut(*category_index) {
                category.expanded = !category.expanded;
                self.rebuild_display();
                let _ = self.save_menu();
            }
        }
    }

    fn move_selection_up(&mut self) {
        if self.display_entries.is_empty() {
            return;
        }
        if self.current_index == 0 {
            self.current_index = self.display_entries.len().saturating_sub(1);
        } else {
            self.current_index -= 1;
        }
    }

    fn move_selection_down(&mut self) {
        if self.display_entries.is_empty() {
            return;
        }
        self.current_index = (self.current_index + 1) % self.display_entries.len();
    }

    fn activate_current_entry(&mut self) {
        if let Some(entry) = self.display_entries.get(self.current_index) {
            match entry {
                DisplayEntry::Category { .. } => {
                    self.toggle_category();
                }
                DisplayEntry::Item { .. } => self.prepare_command(),
            }
        }
    }

    fn prepare_command(&mut self) {
        if let Some(DisplayEntry::Item {
            category_index,
            item_index,
        }) = self.display_entries.get(self.current_index)
        {
            let item = &self.categories[*category_index].items[*item_index];
            if item.cmd.trim().is_empty() {
                return;
            }
            self.status_message = Some(format!("Running {}", item.label));
            self.pending_command = Some(PendingCommand {
                command: item.cmd.clone(),
                pause: item.pause,
            });
        }
    }

    fn save_menu(&self) -> Result<()> {
        let mut categories_map = BTreeMap::new();
        for category in &self.categories {
            categories_map.insert(category.name.clone(), category.to_config());
        }
        let menu_file = MenuFile {
            categories: categories_map,
            app_settings: AppSettings {
                title: Some(self.title.clone()),
                columns: Some(self.column_count),
                theme_key: Some(self.theme_key.clone()),
            },
            custom_colors: self.custom_colors.clone(),
            saved_themes: self.saved_themes.clone(),
        };
        menu_file.save(&self.paths.menu_file)
    }

    fn take_pending_command(&mut self) -> Option<PendingCommand> {
        self.pending_command.take()
    }

    fn take_pending_action(&mut self) -> Option<DeferredAction> {
        self.pending_action.take()
    }

    fn set_status(&mut self, message: Option<String>) {
        self.status_message = message;
    }

    fn status_text(&self) -> String {
        let total = self.display_entries.len();
        let current = if total == 0 {
            0
        } else {
            self.current_index + 1
        };
        let mut text = format!("Item {}/{} | Theme: {}", current, total, self.theme.name);
        if let Some(msg) = &self.status_message {
            text.push_str(" | ");
            text.push_str(msg);
        }
        text
    }

    fn available_color_presets(&self) -> Vec<ColorPreset> {
        let mut presets: Vec<ColorPreset> = DEFAULT_CATEGORY_COLOR_PRESETS
            .iter()
            .map(|(name, bg, text)| ColorPreset::new(*name, *bg, *text))
            .collect();
        for (idx, pair) in self.custom_colors.iter().enumerate() {
            if let (Some(bg), Some(text)) = (pair.background.as_deref(), pair.text.as_deref()) {
                let name = pair
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Custom Theme {}", idx + 1));
                presets.push(ColorPreset::from_custom(name, bg, text, idx));
            }
        }
        if presets.is_empty() {
            presets.push(ColorPreset::new("Default", "#034e68", "#caf0f8"));
        }
        presets
    }

    fn theme_options(&self) -> Vec<ThemeOption> {
        let mut options: Vec<ThemeOption> = THEME_PRESETS
            .iter()
            .map(|(key, def)| ThemeOption::from_definition(key, def))
            .collect();
        for (idx, saved) in self.saved_themes.iter().enumerate() {
            options.push(ThemeOption {
                key: saved_theme_key(idx),
                label: saved.name.clone(),
                primary_hex: saved.primary.clone(),
                accent_hex: saved.accent.clone(),
                highlight_hex: saved
                    .highlight
                    .clone()
                    .unwrap_or_else(|| saved.accent.clone()),
                background_hex: saved.background.clone(),
                surface_hex: saved.surface.clone(),
                text_hex: saved.text.clone(),
            });
        }
        if self.theme_key == CUSTOM_THEME_KEY {
            options.push(ThemeOption {
                key: CUSTOM_THEME_KEY.to_string(),
                label: "Custom Theme".to_string(),
                primary_hex: self.theme.primary_hex.clone(),
                accent_hex: self.theme.accent_hex.clone(),
                highlight_hex: self.theme.highlight_hex.clone(),
                background_hex: self.theme.background_hex.clone(),
                surface_hex: self.theme.surface_hex.clone(),
                text_hex: self.theme.text_hex.clone(),
            });
        }
        options
    }

    fn theme_from_saved_index(&self, index: usize) -> Option<Theme> {
        self.saved_themes.get(index).map(|saved| {
            let highlight = saved
                .highlight
                .as_deref()
                .unwrap_or_else(|| saved.accent.as_str());
            Theme::from_hexes(
                saved.name.clone(),
                &saved.primary,
                &saved.accent,
                highlight,
                &saved.background,
                &saved.surface,
                &saved.text,
            )
        })
    }

    fn footer_line(&self) -> Line<'static> {
        self.footer_line_data().line
    }

    fn footer_line_data(&self) -> FooterLineData {
        let base_bg =
            color_from_hex("#76B3C5").unwrap_or_else(|| self.theme.highlight);
        let shortcut_fg =
            color_from_hex("#FDA009").unwrap_or_else(|| self.theme.accent);
        let label_fg =
            color_from_hex("#2E3544").unwrap_or_else(|| self.theme.surface);
        let shortcut_style = Style::default()
            .fg(shortcut_fg)
            .bg(base_bg)
            .add_modifier(Modifier::BOLD);
        let label_style = Style::default().fg(label_fg).bg(base_bg);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut segments = Vec::new();
        let mut cursor: u16 = 0;
        for (index, shortcut) in FOOTER_SHORTCUTS.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" | ", label_style));
                cursor = cursor.saturating_add(3);
            }
            let entry_start = cursor;
            spans.push(Span::styled(shortcut.key, shortcut_style));
            spans.push(Span::styled(shortcut.label, label_style));
            let key_len = shortcut.key.chars().count() as u16;
            let label_len = shortcut.label.chars().count() as u16;
            let entry_end = entry_start
                .saturating_add(key_len)
                .saturating_add(label_len);
            segments.push(FooterSegment {
                start: entry_start,
                end: entry_end,
                action: shortcut.action,
            });
            cursor = entry_end;
        }
        FooterLineData {
            line: Line::from(spans),
            segments,
            total_width: cursor,
        }
    }

    fn execute_footer_action(&mut self, action: FooterAction) {
        match action {
            FooterAction::Quit => self.should_quit = true,
            FooterAction::Edit => self.queue_edit_current(),
            FooterAction::Execute => self.prepare_command(),
            FooterAction::NewItem => self.queue_new_item(),
            FooterAction::Delete => self.delete_selected_item(),
            FooterAction::Settings => self.queue_settings(),
            FooterAction::ScanBin => self.run_bin_scan(),
        }
    }

    fn queue_new_item(&mut self) {
        self.pending_action = Some(DeferredAction::NewItem);
    }

    fn queue_edit_current(&mut self) {
        if let Some(entry) = self.display_entries.get(self.current_index) {
            match entry {
                DisplayEntry::Item {
                    category_index,
                    item_index,
                } => {
                    self.pending_action = Some(DeferredAction::EditItem {
                        category_index: *category_index,
                        item_index: *item_index,
                    });
                }
                DisplayEntry::Category { category_index } => {
                    self.pending_action = Some(DeferredAction::EditCategory {
                        category_index: *category_index,
                    });
                }
            }
        }
    }

    fn queue_settings(&mut self) {
        self.queue_settings_with_focus(SettingsField::Title);
    }

    fn queue_settings_with_focus(&mut self, focus: SettingsField) {
        self.pending_action = Some(DeferredAction::Settings(focus));
    }

    fn show_info_popup(&mut self) {
        if let Some((category_index, item_index)) = self.selected_item_indices() {
            let item = &self.categories[category_index].items[item_index];
            self.active_popup = Some(PopupState::Info(InfoPopup {
                label: item.label.clone(),
                command: item.cmd.clone(),
                category: self.categories[category_index].name.clone(),
                description: item.info.clone(),
            }));
        }
    }

    fn selected_item_indices(&self) -> Option<(usize, usize)> {
        if let Some(DisplayEntry::Item {
            category_index,
            item_index,
        }) = self.display_entries.get(self.current_index)
        {
            Some((*category_index, *item_index))
        } else {
            None
        }
    }

    fn delete_selected_item(&mut self) {
        if let Some((category_index, item_index)) = self.selected_item_indices() {
            self.categories[category_index].items.remove(item_index);
            if self.categories[category_index].items.is_empty() {
                self.categories.remove(category_index);
            }
            if self.current_index >= self.display_entries.len().saturating_sub(1) {
                self.current_index = self.current_index.saturating_sub(1);
            }
            let _ = self.save_menu();
            self.sort_categories();
            self.rebuild_display();
            self.set_status(Some("Item deleted".into()));
        }
    }

    fn sort_categories(&mut self) {
        self.categories
            .sort_by_key(|category| (category.column, category.name.clone()));
    }

    fn ensure_category(&mut self, name: &str) -> usize {
        if let Some(idx) = self.categories.iter().position(|c| c.name == name) {
            return idx;
        }
        self.categories.push(CategoryState {
            name: name.to_string(),
            expanded: true,
            column: 1,
            colors: None,
            items: Vec::new(),
        });
        let idx = self.categories.len() - 1;
        idx
    }

    fn upsert_saved_theme(&mut self, saved: SavedTheme) -> usize {
        if let Some(idx) = self
            .saved_themes
            .iter()
            .position(|theme| theme.name == saved.name)
        {
            self.saved_themes[idx] = saved;
            idx
        } else {
            self.saved_themes.push(saved);
            self.saved_themes.len() - 1
        }
    }

    fn delete_saved_theme(&mut self, index: usize) {
        if index >= self.saved_themes.len() {
            return;
        }
        self.saved_themes.remove(index);
        if let Some(old_index) = parse_saved_theme_key(&self.theme_key) {
            if old_index == index {
                if let Some(fallback) = Theme::from_name("nord") {
                    self.theme = fallback.clone();
                    self.theme_key = "nord".into();
                    let _ = self.theme.save(&self.paths.theme_file);
                }
            } else if old_index > index {
                self.theme_key = saved_theme_key(old_index - 1);
            }
        }
    }

    fn handle_saved_theme_deletion(&mut self, index: usize) {
        self.delete_saved_theme(index);
        let new_options = self.theme_options();
        let new_index = new_options
            .iter()
            .position(|opt| opt.key == self.theme_key)
            .unwrap_or(0);
        if let Some(PopupState::SettingsForm(form)) = self.active_popup.as_mut() {
            form.theme_options = new_options;
            form.theme_index = new_index;
            form.selected_field = SettingsField::Theme;
            form.populate_custom_fields_from_selection();
        }
        self.set_status(Some("Custom theme deleted".into()));
    }

    fn execute_deferred_action<B>(
        &mut self,
        _terminal: &mut Terminal<B>,
        action: DeferredAction,
    ) -> Result<()>
    where
        B: ratatui::backend::Backend + Write,
    {
        match action {
            DeferredAction::NewItem => self.open_item_form(None),
            DeferredAction::EditItem {
                category_index,
                item_index,
            } => self.open_item_form(Some((category_index, item_index))),
            DeferredAction::EditCategory { category_index } => {
                if let Some(category) = self.categories.get(category_index) {
                    let presets = self.available_color_presets();
                    self.active_popup = Some(PopupState::CategoryForm(CategoryFormState::new(
                        category_index,
                        category,
                        presets,
                    )));
                }
            }
            DeferredAction::Settings(focus) => self.prompt_settings(focus)?,
        }
        Ok(())
    }

    fn open_item_form(&mut self, target: Option<(usize, usize)>) {
        let default_categories = if self.categories.is_empty() {
            vec!["General".to_string()]
        } else {
            self.categories.iter().map(|c| c.name.clone()).collect()
        };

        let (default_label, default_cmd, default_info, default_category, default_pause) =
            if let Some((cat_idx, item_idx)) = target {
                let category_name = self.categories[cat_idx].name.clone();
                let item = &self.categories[cat_idx].items[item_idx];
                (
                    item.label.clone(),
                    item.cmd.clone(),
                    item.info.clone(),
                    category_name,
                    item.pause,
                )
            } else {
                (
                    String::new(),
                    String::new(),
                    String::new(),
                    default_categories
                        .get(0)
                        .cloned()
                        .unwrap_or_else(|| "General".into()),
                    false,
                )
            };

        let fallback_category = default_categories
            .get(0)
            .cloned()
            .unwrap_or_else(|| "General".into());
        let initial_category = if default_category.trim().is_empty() {
            fallback_category.clone()
        } else {
            default_category.clone()
        };

        let form = ItemFormState::new(
            target,
            default_label,
            default_cmd,
            default_info,
            initial_category,
            fallback_category,
            default_pause,
            default_categories,
        );
        self.active_popup = Some(PopupState::ItemForm(form));
    }

    fn prompt_settings(&mut self, focus: SettingsField) -> Result<()> {
        let options = self.theme_options();
        let is_custom = self.theme_key == CUSTOM_THEME_KEY;
        self.active_popup = Some(PopupState::SettingsForm(SettingsFormState::new(
            self.title.clone(),
            self.column_count,
            self.theme_key.clone(),
            options,
            focus,
            &self.theme,
            is_custom,
        )));
        Ok(())
    }

    fn run_bin_scan(&mut self) {
        if let Err(err) = self.scan_bin_directory() {
            self.set_status(Some(format!("Bin scan failed: {err}")));
        } else {
            self.set_status(Some("Bin directory scanned".into()));
        }
    }

    fn scan_bin_directory(&mut self) -> Result<()> {
        let source = Path::new("./import");
        if !source.exists() || !source.is_dir() {
            return Ok(());
        }
        let dest = self.paths.config_dir.join("bin");
        fs::create_dir_all(&dest)?;

        let mut existing_commands = HashSet::new();
        for category in &self.categories {
            for item in &category.items {
                existing_commands.insert(item.cmd.clone());
            }
        }

        let mut new_items = Vec::new();
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || !is_executable_file(&entry) {
                continue;
            }
            let filename = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            let dest_path = dest.join(&filename);
            if dest_path.exists() {
                continue;
            }
            fs::rename(&path, &dest_path)?;
            #[cfg(unix)]
            {
                let mut perms = fs::metadata(&dest_path)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&dest_path, perms)?;
            }
            let cmd_path = format!("~/.local/menu-maker/bin/{filename}");
            if existing_commands.contains(&cmd_path) {
                continue;
            }
            existing_commands.insert(cmd_path.clone());
            let label = filename_to_label(&filename);
            new_items.push(MenuItem {
                label,
                cmd: cmd_path,
                info: format!("Executable: {filename}"),
                pause: false,
            });
        }

        if new_items.is_empty() {
            return Ok(());
        }

        let idx = self.ensure_category("Bin Executables");
        self.categories[idx].expanded = true;
        self.categories[idx].items.extend(new_items);
        self.rebuild_display();
        let _ = self.save_menu();
        Ok(())
    }

    fn apply_item_form_input(&mut self, input: ItemFormInput) -> Result<String, String> {
        let label = input.label.trim();
        if label.is_empty() {
            return Err("Label is required".into());
        }
        let command = input.command.trim();
        if command.is_empty() {
            return Err("Command is required".into());
        }
        let info = input.info.trim().to_string();
        let mut category_name = input.category.trim().to_string();
        if category_name.is_empty() {
            category_name = input.fallback_category.trim().to_string();
        }
        if category_name.is_empty() {
            category_name = "General".into();
        }

        let new_item = MenuItem {
            label: label.to_string(),
            cmd: command.to_string(),
            info,
            pause: input.pause,
        };

        match input.target {
            Some((category_index, item_index)) => {
                if category_index >= self.categories.len() {
                    return Err("Item no longer exists".into());
                }
                if item_index >= self.categories[category_index].items.len() {
                    return Err("Item no longer exists".into());
                }
                let same_category = self.categories[category_index].name == category_name;
                if same_category {
                    self.categories[category_index].items[item_index] = new_item;
                } else {
                    self.categories[category_index].items.remove(item_index);
                    if self.categories[category_index].items.is_empty() {
                        self.categories.remove(category_index);
                    }
                    let dest_idx = self.ensure_category(&category_name);
                    self.categories[dest_idx].expanded = true;
                    self.categories[dest_idx].items.push(new_item);
                }
                self.rebuild_display();
                let _ = self.save_menu();
                Ok("Item updated".into())
            }
            None => {
                let idx = self.ensure_category(&category_name);
                self.categories[idx].expanded = true;
                self.categories[idx].items.push(new_item);
                self.rebuild_display();
                let _ = self.save_menu();
                Ok("Item added".into())
            }
        }
    }

    fn apply_category_form_input(&mut self, input: CategoryFormInput) -> Result<String, String> {
        if input.category_index >= self.categories.len() {
            return Err("Category no longer exists".into());
        }
        let current_name = self.categories[input.category_index].name.clone();
        let new_name = if input.name.trim().is_empty() {
            current_name.clone()
        } else {
            input.name.trim().to_string()
        };
        if new_name != current_name
            && self
                .categories
                .iter()
                .enumerate()
                .any(|(idx, cat)| idx != input.category_index && cat.name == new_name)
        {
            return Err("Category name already exists".into());
        }
        let column_value = if input.column_value.trim().is_empty() {
            self.categories[input.category_index].column
        } else {
            input
                .column_value
                .trim()
                .parse::<u16>()
                .map_err(|_| "Column must be a number".to_string())?
        }
        .clamp(1, MAX_COLUMNS);

        let background = parse_color_field(&input.background)?;
        let text = parse_color_field(&input.text_color)?;

        let category = &mut self.categories[input.category_index];
        category.name = new_name;
        category.column = column_value;
        category.colors = match (background, text) {
            (None, None) => None,
            (bg, txt) => Some(ColorConfig {
                background: bg,
                text: txt,
            }),
        };

        self.rebuild_display();
        let _ = self.save_menu();
        Ok("Category updated".into())
    }

    fn process_category_submission(
        &mut self,
        payload: CategorySubmitPayload,
    ) -> Result<String, String> {
        let mut messages: Vec<String> = Vec::new();
        if let Some(preset_input) = payload.new_preset {
            match self.add_custom_category_preset(preset_input) {
                Ok(msg) => {
                    self.handle_category_preset_add_result(Ok(msg.clone()));
                    messages.push(msg);
                }
                Err(err_msg) => {
                    self.handle_category_preset_add_result(Err(err_msg.clone()));
                    return Err(err_msg);
                }
            }
        }
        match self.apply_category_form_input(payload.form) {
            Ok(msg) => {
                messages.push(msg.clone());
                Ok(messages.join(" | "))
            }
            Err(err_msg) => Err(err_msg),
        }
    }

    fn add_custom_category_preset(&mut self, input: CustomPresetInput) -> Result<String, String> {
        let mut name = input.name.trim().to_string();
        if name.is_empty() {
            name = format!("Custom Theme {}", self.custom_colors.len() + 1);
        }
        self.custom_colors.push(NamedColorPair {
            name: Some(name.clone()),
            background: Some(input.background.clone()),
            text: Some(input.text.clone()),
        });
        let _ = self.save_menu();
        Ok(format!("Theme '{name}' added"))
    }

    fn delete_custom_category_preset(&mut self, index: usize) -> Result<String, String> {
        if index >= self.custom_colors.len() {
            return Err("Custom theme not found".into());
        }
        let removed = self.custom_colors.remove(index);
        let name = removed
            .name
            .clone()
            .unwrap_or_else(|| format!("Custom Theme {}", index + 1));
        let _ = self.save_menu();
        Ok(format!("Theme '{name}' deleted"))
    }

    fn handle_category_preset_add_result(
        &mut self,
        result: Result<String, String>,
    ) {
        match result {
            Ok(msg) => {
                let presets = self.available_color_presets();
                if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                    form.refresh_presets(presets);
                    if !form.color_presets.is_empty() {
                        form.focus_palette_index(form.color_presets.len() - 1);
                    }
                }
                self.set_status(Some(msg));
            }
            Err(err_msg) => {
                if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                    form.error = Some(err_msg);
                }
            }
        }
    }

    fn handle_category_preset_delete_result(
        &mut self,
        result: Result<String, String>,
    ) {
        match result {
            Ok(msg) => {
                let presets = self.available_color_presets();
                if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                    let target_index = form
                        .palette_index
                        .min(presets.len().saturating_sub(1));
                    form.refresh_presets(presets);
                    if !form.color_presets.is_empty() {
                        form.focus_palette_index(target_index);
                    } else {
                        form.selected_field = CategoryField::Name;
                    }
                }
                self.set_status(Some(msg));
            }
            Err(err_msg) => {
                if let Some(PopupState::CategoryForm(form)) = self.active_popup.as_mut() {
                    form.error = Some(err_msg);
                }
            }
        }
    }

    fn apply_settings_form_input(&mut self, input: SettingsFormInput) -> Result<String, String> {
        let mut title = input.title.trim().to_string();
        if title.is_empty() {
            title = self.title.clone();
        }
        let columns = if input.columns.trim().is_empty() {
            self.column_count
        } else {
            input
                .columns
                .trim()
                .parse::<u16>()
                .map_err(|_| "Columns must be a number".to_string())?
        }
        .clamp(1, MAX_COLUMNS);
        let mut theme_key = input.theme_key.trim().to_string();
        if theme_key.is_empty() {
            theme_key = self.theme_key.clone();
        }
        let custom_primary = input.custom_primary.trim();
        let custom_accent = input.custom_accent.trim();
        let custom_background = input.custom_background.trim();
        let custom_surface = input.custom_surface.trim();
        let custom_text = input.custom_text.trim();
        let custom_highlight = input.custom_highlight.trim();
        let custom_theme_name = input.custom_theme_name.trim();
        let theme_options_snapshot = self.theme_options();
        let selected_option = theme_options_snapshot
            .iter()
            .find(|opt| opt.key == theme_key);
        let has_any_color_input = [
            custom_primary,
            custom_accent,
            custom_highlight,
            custom_background,
            custom_surface,
            custom_text,
        ]
        .iter()
        .any(|value| !value.is_empty());
        let colors_match_selected = selected_option
            .map(|option| {
                hex_strings_equal(custom_primary, &option.primary_hex)
                    && hex_strings_equal(custom_accent, &option.accent_hex)
                    && hex_strings_equal(custom_highlight, &option.highlight_hex)
                    && hex_strings_equal(custom_background, &option.background_hex)
                    && hex_strings_equal(custom_surface, &option.surface_hex)
                    && hex_strings_equal(custom_text, &option.text_hex)
            })
            .unwrap_or(false);
        let name_matches_selected = if custom_theme_name.is_empty() {
            true
        } else if let Some(option) = selected_option {
            if parse_saved_theme_key(&option.key).is_some() {
                custom_theme_name.eq_ignore_ascii_case(&option.label)
            } else if option.key == CUSTOM_THEME_KEY {
                custom_theme_name.eq_ignore_ascii_case(&self.theme.name)
            } else {
                false
            }
        } else {
            false
        };
        let use_custom_colors = if has_any_color_input {
            selected_option
                .map(|_| !colors_match_selected || !name_matches_selected)
                .unwrap_or(true)
        } else if custom_theme_name.is_empty() {
            false
        } else {
            !name_matches_selected || selected_option.is_none()
        };

        let mut changed = false;
        if title != self.title {
            self.title = title;
            changed = true;
        }
        if columns != self.column_count {
            self.column_count = columns;
            self.rebuild_display();
            changed = true;
        }
        if use_custom_colors {
            let primary = require_color_field(custom_primary, "Primary")?;
            let accent = require_color_field(custom_accent, "Accent")?;
            let highlight = require_color_field(custom_highlight, "Highlight")?;
            let background = require_color_field(custom_background, "Background")?;
            let surface = require_color_field(custom_surface, "Surface")?;
            let text_color = require_color_field(custom_text, "Text")?;
            let theme_name = if custom_theme_name.is_empty() {
                "Custom Theme".to_string()
            } else {
                custom_theme_name.to_string()
            };
            let theme = Theme::from_hexes(
                theme_name.clone(),
                &primary,
                &accent,
                &highlight,
                &background,
                &surface,
                &text_color,
            );
            let mut new_theme_key = CUSTOM_THEME_KEY.to_string();
            if !custom_theme_name.is_empty() {
                let saved = SavedTheme {
                    name: theme_name.clone(),
                    primary,
                    accent,
                    highlight: Some(highlight.clone()),
                    background,
                    surface,
                    text: text_color,
                };
                let index = self.upsert_saved_theme(saved);
                new_theme_key = saved_theme_key(index);
            }
            self.theme = theme.clone();
            self.theme_key = new_theme_key;
            let _ = self.theme.save(&self.paths.theme_file);
            changed = true;
        } else {
            if let Some(index) = parse_saved_theme_key(&theme_key) {
                if parse_saved_theme_key(&self.theme_key) != Some(index) {
                    if let Some(theme) = self.theme_from_saved_index(index) {
                        self.theme = theme.clone();
                        self.theme_key = saved_theme_key(index);
                        let _ = self.theme.save(&self.paths.theme_file);
                        changed = true;
                    } else {
                        return Err("Saved theme not found".into());
                    }
                }
            } else if theme_key == CUSTOM_THEME_KEY {
                if self.theme_key != CUSTOM_THEME_KEY {
                    return Err("Enter custom colors to create a custom theme".into());
                }
            } else if theme_key != self.theme_key {
                let theme = Theme::from_name(&theme_key)
                    .ok_or_else(|| "Unknown theme selected".to_string())?;
                self.theme = theme.clone();
                self.theme_key = theme_key.clone();
                let _ = self.theme.save(&self.paths.theme_file);
                changed = true;
            }
        }

        let _ = self.save_menu();
        if changed {
            Ok("Settings updated".into())
        } else {
            Ok("No settings changed".into())
        }
    }
}

#[derive(Clone)]
struct PendingCommand {
    command: String,
    pause: bool,
}

#[derive(Clone)]
struct MenuItem {
    label: String,
    cmd: String,
    info: String,
    pause: bool,
}

impl MenuItem {
    fn from_config(category: &str, cfg: &MenuItemConfig) -> Self {
        MenuItem {
            label: cfg.label.clone(),
            cmd: cfg.cmd.clone(),
            info: cfg
                .info
                .clone()
                .unwrap_or_else(|| format!("Item in {category}")),
            pause: cfg.pause.unwrap_or(false),
        }
    }
}

#[derive(Clone)]
struct CategoryState {
    name: String,
    expanded: bool,
    column: u16,
    colors: Option<ColorConfig>,
    items: Vec<MenuItem>,
}

impl CategoryState {
    fn from_config(name: &str, cfg: &CategoryConfig) -> Self {
        let column = cfg.column.unwrap_or(1).clamp(1, MAX_COLUMNS);
        let items = cfg
            .items
            .iter()
            .map(|item| MenuItem::from_config(name, item))
            .collect();
        CategoryState {
            name: name.to_string(),
            expanded: cfg.expanded,
            column,
            colors: cfg.colors.clone(),
            items,
        }
    }

    fn to_config(&self) -> CategoryConfig {
        CategoryConfig {
            expanded: self.expanded,
            column: Some(self.column),
            items: self
                .items
                .iter()
                .map(|item| MenuItemConfig {
                    label: item.label.clone(),
                    cmd: item.cmd.clone(),
                    info: Some(item.info.clone()),
                    category: Some(self.name.clone()),
                    pause: Some(item.pause),
                })
                .collect(),
            colors: self.colors.clone(),
        }
    }
}

enum DisplayEntry {
    Category {
        category_index: usize,
    },
    Item {
        category_index: usize,
        item_index: usize,
    },
}

#[derive(Clone)]
struct InfoPopup {
    label: String,
    command: String,
    category: String,
    description: String,
}

struct ItemFormState {
    target: Option<(usize, usize)>,
    label: String,
    command: String,
    info: String,
    category: String,
    fallback_category: String,
    pause: bool,
    available_categories: Vec<String>,
    selected_field: ItemField,
    error: Option<String>,
    mode_label: &'static str,
}

#[derive(Clone)]
struct ItemFormInput {
    target: Option<(usize, usize)>,
    label: String,
    command: String,
    info: String,
    category: String,
    fallback_category: String,
    pause: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemField {
    Label,
    Command,
    Description,
    Category,
    Pause,
}

enum PopupState {
    Info(InfoPopup),
    Message(String),
    ItemForm(ItemFormState),
    CategoryForm(CategoryFormState),
    SettingsForm(SettingsFormState),
}

enum DeferredAction {
    NewItem,
    EditItem {
        category_index: usize,
        item_index: usize,
    },
    EditCategory {
        category_index: usize,
    },
    Settings(SettingsField),
}

enum PopupResult {
    None,
    Close(Option<String>),
    ItemSubmit(ItemFormInput),
    CategorySubmit(CategorySubmitPayload),
    CategoryDeletePreset(usize),
    SettingsSubmit(SettingsFormInput),
    SettingsDeleteSavedTheme(usize),
}

enum PopupClickAction {
    Category(CategoryFormClick),
    Settings(SettingsFormClick),
}

enum CategoryFormClick {
    SelectField(CategoryField),
    SelectPalette(usize),
    Shortcut(CategoryShortcutAction),
}

enum SettingsFormClick {
    SelectField(SettingsField),
    SelectTheme(usize),
    DeleteSavedTheme(usize),
    Shortcut(SettingsShortcutAction),
}

#[derive(Clone, Copy)]
struct SettingsShortcutSegment {
    start: u16,
    end: u16,
    action: SettingsShortcutAction,
}

#[derive(Clone, Copy)]
struct CategoryShortcutSegment {
    start: u16,
    end: u16,
    action: CategoryShortcutAction,
}

#[derive(Clone, Copy)]
enum SettingsShortcutAction {
    NextField,
    Submit,
    Cancel,
    PreviousTheme,
    NextTheme,
    DeleteTheme,
}

#[derive(Clone, Copy)]
enum CategoryShortcutAction {
    NextField,
    PreviousField,
    Submit,
    Cancel,
    PreviousPalette,
    NextPalette,
    DeletePreset,
}

struct CategoryFormState {
    category_index: usize,
    name: String,
    column_value: String,
    selected_field: CategoryField,
    error: Option<String>,
    color_presets: Vec<ColorPreset>,
    palette_index: usize,
    custom_preset_name: String,
    custom_preset_background: String,
    custom_preset_text: String,
}

#[derive(Default)]
struct CategoryFormLayout {
    line_count: usize,
    name_line: Option<usize>,
    column_line: Option<usize>,
    custom_heading_line: Option<usize>,
    custom_name_line: Option<usize>,
    custom_background_line: Option<usize>,
    custom_text_line: Option<usize>,
    shortcut_segments: Vec<CategoryShortcutSegment>,
    shortcut_total_width: u16,
    shortcut_line: Option<Line<'static>>,
    presets_heading_line: Option<usize>,
    presets_start_line: Option<usize>,
    presets_count: usize,
}

#[derive(Clone)]
struct CategoryFormInput {
    category_index: usize,
    name: String,
    column_value: String,
    background: String,
    text_color: String,
}

struct CategorySubmitPayload {
    form: CategoryFormInput,
    new_preset: Option<CustomPresetInput>,
}

#[derive(Clone)]
struct CustomPresetInput {
    name: String,
    background: String,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CategoryField {
    Name,
    Column,
    CustomPresetName,
    CustomPresetBackground,
    CustomPresetText,
    Palette,
}

enum FormKeyResult {
    Continue,
    Cancel,
    Submit(CategorySubmitPayload),
    DeletePreset(usize),
}

enum ItemFormKeyResult {
    Continue,
    Cancel,
    Submit(ItemFormInput),
}

impl CategoryFormState {
    fn new(index: usize, category: &CategoryState, presets: Vec<ColorPreset>) -> Self {
        let background = category
            .colors
            .as_ref()
            .and_then(|c| c.background.clone())
            .map(|value| normalize_hex(&value))
            .unwrap_or_default();
        let text = category
            .colors
            .as_ref()
            .and_then(|c| c.text.clone())
            .map(|value| normalize_hex(&value))
            .unwrap_or_default();
        let color_presets = if presets.is_empty() {
            vec![ColorPreset::new("Default", "#034e68", "#caf0f8")]
        } else {
            presets
        };
        let palette_index = if !background.is_empty() && !text.is_empty() {
            color_presets
                .iter()
                .position(|preset| preset.matches(&background, &text))
                .unwrap_or(0)
        } else {
            0
        };        let mut custom_name = String::new();
        if let Some(preset) = color_presets.get(palette_index) {
            custom_name = preset.name.clone();
        } else if !category.name.is_empty() {
            custom_name = format!("{} Colors", category.name);
        }

        Self {
            category_index: index,
            name: category.name.clone(),
            column_value: category.column.to_string(),
            selected_field: CategoryField::Name,
            error: None,
            color_presets,
            palette_index,
            custom_preset_name: custom_name,
            custom_preset_background: background,
            custom_preset_text: text,
        }
    }

    fn render_lines(&self, app: &AppState) -> (Vec<FormLine>, CategoryFormLayout) {
        let mut layout = CategoryFormLayout::default();
        let mut lines: Vec<FormLine> = Vec::new();
        lines.push(plain_line(Line::from("Update the category fields below.")));

        layout.name_line = Some(lines.len());
        lines.push(make_field_line(
            "Name",
            &self.name,
            self.selected_field == CategoryField::Name,
            app,
        ));

        layout.column_line = Some(lines.len());
        lines.push(make_field_line(
            "Column",
            &self.column_value,
            self.selected_field == CategoryField::Column,
            app,
        ));

        if !self.color_presets.is_empty() {
            lines.push(plain_line(Line::from("")));
            layout.presets_heading_line = Some(lines.len());
            lines.push(plain_line(Line::from(vec![Span::styled(
                "Color Theme (Tab to focus, ←/→ select)",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )])));
            layout.presets_start_line = Some(lines.len());
            for (idx, preset) in self.color_presets.iter().enumerate() {
                let is_selected = self.palette_index == idx;
                let highlight_palette = is_selected && self.selected_field == CategoryField::Palette;
                let mut label_style = Style::default().fg(app.theme.text);
                if is_selected {
                    label_style = label_style.add_modifier(Modifier::BOLD);
                }
                let preview_bg = color_from_hex(&preset.background);
                let preview_text = color_from_hex(&preset.text).unwrap_or(app.theme.text);
                let mut spans = vec![Span::styled(
                    format!("{:>2}. {}", idx + 1, preset.name),
                    label_style,
                )];
                if let Some(bg) = preview_bg {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        "     ",
                        Style::default().bg(bg).fg(preview_text),
                    ));
                }
                spans.push(Span::raw("  "));
                let mut background_hex_style = Style::default().fg(app.theme.text);
                let mut divider_style = Style::default().fg(app.theme.text);
                let mut text_hex_style = Style::default().fg(app.theme.text);
                if is_selected {
                    background_hex_style = background_hex_style.add_modifier(Modifier::BOLD);
                    divider_style = divider_style.add_modifier(Modifier::BOLD);
                    text_hex_style = text_hex_style.add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(preset.background.clone(), background_hex_style));
                spans.push(Span::styled(" / ", divider_style));
                spans.push(Span::styled(preset.text.clone(), text_hex_style));
                let line = if highlight_palette {
                    FormLine::highlighted(Line::from(spans))
                } else {
                    FormLine::plain(Line::from(spans))
                };
                lines.push(line);
            }
            layout.presets_count = self.color_presets.len();
        }

        lines.push(plain_line(Line::from("")));
        layout.custom_heading_line = Some(lines.len());
        lines.push(plain_line(Line::from(vec![Span::styled(
            "Custom Theme (#RRGGBB)",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        )])));
        layout.custom_name_line = Some(lines.len());
        lines.push(make_field_line(
            "Name",
            &self.custom_preset_name,
            self.selected_field == CategoryField::CustomPresetName,
            app,
        ));
        layout.custom_background_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Background",
            &self.custom_preset_background,
            self.selected_field == CategoryField::CustomPresetBackground,
            color_from_hex(&self.custom_preset_background),
            app,
        ));
        layout.custom_text_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Text",
            &self.custom_preset_text,
            self.selected_field == CategoryField::CustomPresetText,
            color_from_hex(&self.custom_preset_text),
            app,
        ));

        lines.push(plain_line(Line::from("")));
        let (shortcut_line, shortcut_segments, shortcut_width) =
            build_category_shortcut_line(app, self.has_deletable_preset());
        layout.shortcut_line = Some(shortcut_line);
        layout.shortcut_segments = shortcut_segments;
        layout.shortcut_total_width = shortcut_width;
        if let Some(error) = &self.error {
            lines.push(plain_line(Line::from(vec![Span::styled(
                error.clone(),
                Style::default().fg(Color::Red),
            )])));
        }
        layout.line_count = lines.len();
        (lines, layout)
    }
    fn handle_key(&mut self, key: KeyEvent) -> FormKeyResult {
        self.error = None;
        match key.code {
            KeyCode::Esc => FormKeyResult::Cancel,
            KeyCode::Enter => {
                if self.selected_field == CategoryField::Palette {
                    if self.has_deletable_preset() {
                        if let Some(index) = self.current_custom_preset_index() {
                            return FormKeyResult::DeletePreset(index);
                        }
                    }
                    match self.build_submission() {
                        Ok(input) => FormKeyResult::Submit(input),
                        Err(err) => {
                            self.error = Some(err);
                            FormKeyResult::Continue
                        }
                    }
                } else {
                    match self.build_submission() {
                        Ok(input) => FormKeyResult::Submit(input),
                        Err(err) => {
                            self.error = Some(err);
                            FormKeyResult::Continue
                        }
                    }
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                self.next_field();
                FormKeyResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.previous_field();
                FormKeyResult::Continue
            }
            KeyCode::Left if self.selected_field == CategoryField::Palette => {
                self.previous_palette();
                FormKeyResult::Continue
            }
            KeyCode::Right if self.selected_field == CategoryField::Palette => {
                self.next_palette();
                FormKeyResult::Continue
            }
            KeyCode::Backspace => {
                if let Some(value) = self.active_value_mut() {
                    value.pop();
                }
                FormKeyResult::Continue
            }
            KeyCode::Delete => {
                if let Some(value) = self.active_value_mut() {
                    value.clear();
                    FormKeyResult::Continue
                } else if self.selected_field == CategoryField::Palette
                    && self.has_deletable_preset()
                {
                    if let Some(index) = self.current_custom_preset_index() {
                        FormKeyResult::DeletePreset(index)
                    } else {
                        FormKeyResult::Continue
                    }
                } else {
                    FormKeyResult::Continue
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D')
                if self.has_deletable_preset()
                    && self.selected_field == CategoryField::Palette =>
            {
                if let Some(index) = self.current_custom_preset_index() {
                    FormKeyResult::DeletePreset(index)
                } else {
                    FormKeyResult::Continue
                }
            }
            KeyCode::Char(c) => {
                if self.selected_field != CategoryField::Palette
                    && !key.modifiers.contains(KeyModifiers::CONTROL) {
                    if let Some(value) = self.active_value_mut() {
                        value.push(c);
                    }
                }
                FormKeyResult::Continue
            }
            _ => FormKeyResult::Continue,
        }
    }

    fn build_submission(&self) -> Result<CategorySubmitPayload, String> {
        let background_result = parse_color_field(&self.custom_preset_background)?;
        let text_result = parse_color_field(&self.custom_preset_text)?;

        let background_value = background_result.clone().unwrap_or_default();
        let text_value = text_result.clone().unwrap_or_default();

        let mut new_preset: Option<CustomPresetInput> = None;
        if let (Some(bg), Some(txt)) = (background_result, text_result) {
            let exists = self
                .color_presets
                .iter()
                .any(|preset| hex_strings_equal(&preset.background, &bg) && hex_strings_equal(&preset.text, &txt));
            if !exists {
                let name = if self.custom_preset_name.trim().is_empty() {
                    "Custom Theme".to_string()
                } else {
                    self.custom_preset_name.trim().to_string()
                };
                new_preset = Some(CustomPresetInput {
                    name,
                    background: bg.clone(),
                    text: txt.clone(),
                });
            }
        }

        Ok(CategorySubmitPayload {
            form: CategoryFormInput {
                category_index: self.category_index,
                name: self.name.clone(),
                column_value: self.column_value.clone(),
                background: background_value,
                text_color: text_value,
            },
            new_preset,
        })
    }

    fn next_field(&mut self) {
        let has_palette = !self.color_presets.is_empty();
        self.selected_field = match self.selected_field {
            CategoryField::Name => CategoryField::Column,
            CategoryField::Column => {
                if has_palette {
                    CategoryField::Palette
                } else {
                    CategoryField::CustomPresetName
                }
            }
            CategoryField::Palette => CategoryField::CustomPresetName,
            CategoryField::CustomPresetName => CategoryField::CustomPresetBackground,
            CategoryField::CustomPresetBackground => CategoryField::CustomPresetText,
            CategoryField::CustomPresetText => CategoryField::Name,
        };
    }
    fn previous_field(&mut self) {
        let has_palette = !self.color_presets.is_empty();
        self.selected_field = match self.selected_field {
            CategoryField::Name => CategoryField::CustomPresetText,
            CategoryField::Column => CategoryField::Name,
            CategoryField::Palette => CategoryField::Column,
            CategoryField::CustomPresetName => {
                if has_palette {
                    CategoryField::Palette
                } else {
                    CategoryField::Column
                }
            }
            CategoryField::CustomPresetBackground => CategoryField::CustomPresetName,
            CategoryField::CustomPresetText => CategoryField::CustomPresetBackground,
        };
    }
    fn active_value_mut(&mut self) -> Option<&mut String> {
        match self.selected_field {
            CategoryField::Name => Some(&mut self.name),
            CategoryField::Column => Some(&mut self.column_value),
            CategoryField::CustomPresetName => Some(&mut self.custom_preset_name),
            CategoryField::CustomPresetBackground => Some(&mut self.custom_preset_background),
            CategoryField::CustomPresetText => Some(&mut self.custom_preset_text),
            CategoryField::Palette => None,
        }
    }

    fn has_deletable_preset(&self) -> bool {
        self.current_custom_preset_index().is_some()
    }

    fn current_custom_preset_index(&self) -> Option<usize> {
        self.color_presets
            .get(self.palette_index)
            .and_then(|preset| preset.custom_index)
    }
    fn refresh_presets(&mut self, presets: Vec<ColorPreset>) {
        self.color_presets = presets;
        if self.color_presets.is_empty() {
            self.palette_index = 0;
        } else {
            if self.palette_index >= self.color_presets.len() {
                self.palette_index = self.color_presets.len() - 1;
            }
            self.apply_selected_palette();
        }
    }

    fn focus_palette_index(&mut self, index: usize) {
        if self.color_presets.is_empty() {
            return;
        }
        self.palette_index = index.min(self.color_presets.len() - 1);
        self.selected_field = CategoryField::Palette;
        self.apply_selected_palette();
    }

    fn next_palette(&mut self) {
        if self.color_presets.is_empty() {
            return;
        }
        self.palette_index = (self.palette_index + 1) % self.color_presets.len();
        self.apply_selected_palette();
    }

    fn previous_palette(&mut self) {
        if self.color_presets.is_empty() {
            return;
        }
        if self.palette_index == 0 {
            self.palette_index = self.color_presets.len() - 1;
        } else {
            self.palette_index -= 1;
        }
        self.apply_selected_palette();
    }

    fn apply_selected_palette(&mut self) {
        if let Some(preset) = self.color_presets.get(self.palette_index) {
            self.custom_preset_background = preset.background.clone();
            self.custom_preset_text = preset.text.clone();
            self.custom_preset_name = preset.name.clone();
        }
    }
}

impl ItemFormState {
    fn new(
        target: Option<(usize, usize)>,
        label: String,
        command: String,
        info: String,
        category: String,
        fallback_category: String,
        pause: bool,
        available_categories: Vec<String>,
    ) -> Self {
        Self {
            target,
            label,
            command,
            info,
            category,
            fallback_category,
            pause,
            available_categories,
            selected_field: ItemField::Label,
            error: None,
            mode_label: if target.is_some() {
                "Edit Menu Item"
            } else {
                "New Menu Item"
            },
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ItemFormKeyResult {
        self.error = None;
        match key.code {
            KeyCode::Esc => ItemFormKeyResult::Cancel,
            KeyCode::Enter => ItemFormKeyResult::Submit(self.to_input()),
            KeyCode::Tab | KeyCode::Down => {
                self.next_field();
                ItemFormKeyResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.previous_field();
                ItemFormKeyResult::Continue
            }
            KeyCode::Char(' ') if self.selected_field == ItemField::Pause => {
                self.pause = !self.pause;
                ItemFormKeyResult::Continue
            }
            KeyCode::Backspace => {
                if let Some(value) = self.active_value_mut() {
                    value.pop();
                }
                ItemFormKeyResult::Continue
            }
            KeyCode::Delete => {
                if let Some(value) = self.active_value_mut() {
                    value.clear();
                }
                ItemFormKeyResult::Continue
            }
            KeyCode::Char(c) => {
                if self.selected_field != ItemField::Pause
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    if let Some(value) = self.active_value_mut() {
                        value.push(c);
                    }
                }
                ItemFormKeyResult::Continue
            }
            _ => ItemFormKeyResult::Continue,
        }
    }

    fn to_input(&self) -> ItemFormInput {
        ItemFormInput {
            target: self.target,
            label: self.label.clone(),
            command: self.command.clone(),
            info: self.info.clone(),
            category: self.category.clone(),
            fallback_category: self.fallback_category.clone(),
            pause: self.pause,
        }
    }

    fn next_field(&mut self) {
        self.selected_field = match self.selected_field {
            ItemField::Label => ItemField::Command,
            ItemField::Command => ItemField::Description,
            ItemField::Description => ItemField::Category,
            ItemField::Category => ItemField::Pause,
            ItemField::Pause => ItemField::Label,
        };
    }

    fn previous_field(&mut self) {
        self.selected_field = match self.selected_field {
            ItemField::Label => ItemField::Pause,
            ItemField::Command => ItemField::Label,
            ItemField::Description => ItemField::Command,
            ItemField::Category => ItemField::Description,
            ItemField::Pause => ItemField::Category,
        };
    }

    fn active_value_mut(&mut self) -> Option<&mut String> {
        match self.selected_field {
            ItemField::Label => Some(&mut self.label),
            ItemField::Command => Some(&mut self.command),
            ItemField::Description => Some(&mut self.info),
            ItemField::Category => Some(&mut self.category),
            ItemField::Pause => None,
        }
    }
}

struct SettingsFormState {
    title: String,
    columns_value: String,
    theme_options: Vec<ThemeOption>,
    theme_index: usize,
    selected_field: SettingsField,
    error: Option<String>,
    custom_primary: String,
    custom_accent: String,
    custom_background: String,
    custom_surface: String,
    custom_text: String,
    custom_highlight: String,
    custom_theme_name: String,
}

#[derive(Default)]
struct SettingsFormLayout {
    line_count: usize,
    title_line: Option<usize>,
    columns_line: Option<usize>,
    theme_heading_line: Option<usize>,
    theme_list_start: Option<usize>,
    theme_count: usize,
    custom_heading_line: Option<usize>,
    delete_saved_theme_line: Option<usize>,
    delete_saved_theme_index: Option<usize>,
    shortcut_line: Option<Line<'static>>,
    shortcut_segments: Vec<SettingsShortcutSegment>,
    shortcut_total_width: u16,
    custom_name_line: Option<usize>,
    custom_primary_line: Option<usize>,
    custom_accent_line: Option<usize>,
    custom_background_line: Option<usize>,
    custom_surface_line: Option<usize>,
    custom_text_line: Option<usize>,
    custom_highlight_line: Option<usize>,
}

#[derive(Clone)]
struct SettingsFormInput {
    title: String,
    columns: String,
    theme_key: String,
    custom_primary: String,
    custom_accent: String,
    custom_background: String,
    custom_surface: String,
    custom_text: String,
    custom_highlight: String,
    custom_theme_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Title,
    Columns,
    Theme,
    CustomName,
    CustomPrimary,
    CustomAccent,
    CustomBackground,
    CustomSurface,
    CustomText,
    CustomHighlight,
}

enum SettingsFormKeyResult {
    Continue,
    Cancel,
    Submit(SettingsFormInput),
    DeleteSavedTheme(usize),
}

impl SettingsFormState {
    fn new(
        title: String,
        columns: u16,
        theme_key: String,
        options: Vec<ThemeOption>,
        initial_field: SettingsField,
        current_theme: &Theme,
        is_custom_theme: bool,
    ) -> Self {
        let columns_value = columns.to_string();
        let theme_index = options
            .iter()
            .position(|opt| opt.key == theme_key)
            .unwrap_or(0);
        Self {
            title,
            columns_value,
            theme_options: options,
            theme_index,
            selected_field: initial_field,
            error: None,
            custom_primary: String::new(),
            custom_accent: String::new(),
            custom_background: String::new(),
            custom_surface: String::new(),
            custom_text: String::new(),
            custom_highlight: String::new(),
            custom_theme_name: if is_custom_theme {
                current_theme.name.clone()
            } else {
                String::new()
            },
        }
        .with_selected_theme_colors()
    }

    fn current_deletable_theme_index(&self) -> Option<usize> {
        self.theme_options
            .get(self.theme_index)
            .and_then(|opt| parse_saved_theme_key(&opt.key))
    }

    fn render_lines(&self, app: &AppState) -> (Vec<FormLine>, SettingsFormLayout) {
        let mut layout = SettingsFormLayout::default();
        let mut lines: Vec<FormLine> = Vec::new();
        let deletable_index = self.current_deletable_theme_index();
        if let Some(saved_idx) = deletable_index {
            layout.delete_saved_theme_index = Some(saved_idx);
        }
        lines.push(plain_line(Line::from("Adjust application settings below.")));

        layout.title_line = Some(lines.len());
        lines.push(make_field_line(
            "Title",
            &self.title,
            self.selected_field == SettingsField::Title,
            app,
        ));

        layout.columns_line = Some(lines.len());
        lines.push(make_field_line(
            "Columns (1-6)",
            &self.columns_value,
            self.selected_field == SettingsField::Columns,
            app,
        ));

        lines.push(plain_line(Line::from("")));
        let (shortcut_line, shortcut_segments, shortcut_width) =
            build_settings_shortcut_line(app, deletable_index.is_some());
        layout.shortcut_line = Some(shortcut_line);
        layout.shortcut_segments = shortcut_segments;
        layout.shortcut_total_width = shortcut_width;
        if let Some(error) = &self.error {
            lines.push(plain_line(Line::from(vec![Span::styled(
                error.clone(),
                Style::default().fg(Color::Red),
            )])));
        }
        if !self.theme_options.is_empty() {
            lines.push(plain_line(Line::from("")));
            layout.theme_heading_line = Some(lines.len());
            lines.push(plain_line(Line::from(vec![Span::styled(
                "Theme Presets (Tab to focus, ←/→ select)",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )])));
            layout.theme_list_start = Some(lines.len());
            for (idx, option) in self.theme_options.iter().enumerate() {
                let is_active = self.theme_index == idx;
                let mut label_style = Style::default().fg(app.theme.text);
                if is_active {
                    label_style = label_style.add_modifier(Modifier::BOLD);
                }
                let mut spans = vec![Span::styled(
                    format!("{:>2}. {}", idx + 1, option.label),
                    label_style,
                )];
                if let Some(surface) = color_from_hex(&option.surface_hex) {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        "     ",
                        Style::default().bg(surface).fg(app.theme.text),
                    ));
                }
                if let Some(accent) = color_from_hex(&option.accent_hex) {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        "     ",
                        Style::default().bg(accent).fg(app.theme.background),
                    ));
                }
                if let Some(highlight) = color_from_hex(&option.highlight_hex) {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        "     ",
                        Style::default().bg(highlight).fg(app.theme.background),
                    ));
                }
                for (label, hex) in [
                    ("Primary", &option.primary_hex),
                    ("Accent", &option.accent_hex),
                    ("Highlight", &option.highlight_hex),
                    ("Background", &option.background_hex),
                    ("Surface", &option.surface_hex),
                    ("Text", &option.text_hex),
                ] {
                    spans.push(Span::raw("  "));
                    let mut color_style = Style::default().fg(app.theme.text);
                    if is_active {
                        color_style = color_style.add_modifier(Modifier::BOLD);
                    }
                    spans.push(Span::styled(
                        format!("{} {}", label, hex.to_uppercase()),
                        color_style,
                    ));
                }
                let line = Line::from(spans);
                if self.selected_field == SettingsField::Theme && is_active {
                    lines.push(FormLine::highlighted(line));
                } else {
                    lines.push(FormLine::plain(line));
                }
            }
            layout.theme_count = self.theme_options.len();
        }
        lines.push(plain_line(Line::from("")));
        layout.custom_heading_line = Some(lines.len());
        lines.push(plain_line(Line::from(vec![Span::styled(
            "Custom Theme Colors (#RRGGBB, leave blank to keep preset)",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        )])));
        layout.custom_name_line = Some(lines.len());
        lines.push(make_field_line(
            "Custom Theme Name",
            &self.custom_theme_name,
            self.selected_field == SettingsField::CustomName,
            app,
        ));
        layout.custom_primary_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Primary",
            &self.custom_primary,
            self.selected_field == SettingsField::CustomPrimary,
            color_from_hex(&self.custom_primary),
            app,
        ));
        layout.custom_accent_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Accent",
            &self.custom_accent,
            self.selected_field == SettingsField::CustomAccent,
            color_from_hex(&self.custom_accent),
            app,
        ));
        layout.custom_highlight_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Highlight",
            &self.custom_highlight,
            self.selected_field == SettingsField::CustomHighlight,
            color_from_hex(&self.custom_highlight),
            app,
        ));
        layout.custom_background_line = Some(lines.len());
        lines.push(make_field_line(
            "Background",
            &self.custom_background,
            self.selected_field == SettingsField::CustomBackground,
            app,
        ));
        layout.custom_surface_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Surface",
            &self.custom_surface,
            self.selected_field == SettingsField::CustomSurface,
            color_from_hex(&self.custom_surface),
            app,
        ));
        layout.custom_text_line = Some(lines.len());
        lines.push(make_color_field_line(
            "Text",
            &self.custom_text,
            self.selected_field == SettingsField::CustomText,
            color_from_hex(&self.custom_text),
            app,
        ));
        layout.line_count = lines.len();
        (lines, layout)
    }

    fn handle_key(&mut self, key: KeyEvent) -> SettingsFormKeyResult {
        self.error = None;
        match key.code {
            KeyCode::Esc => SettingsFormKeyResult::Cancel,
            KeyCode::Enter => SettingsFormKeyResult::Submit(self.to_input()),
            KeyCode::Tab | KeyCode::Down => {
                self.next_field();
                SettingsFormKeyResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.previous_field();
                SettingsFormKeyResult::Continue
            }
            KeyCode::Left if self.selected_field == SettingsField::Theme => {
                self.previous_theme();
                SettingsFormKeyResult::Continue
            }
            KeyCode::Right if self.selected_field == SettingsField::Theme => {
                self.next_theme();
                SettingsFormKeyResult::Continue
            }
            KeyCode::Char('d') | KeyCode::Char('D')
                if self.selected_field == SettingsField::Theme =>
            {
                if let Some(index) = self.current_deletable_theme_index() {
                    SettingsFormKeyResult::DeleteSavedTheme(index)
                } else {
                    SettingsFormKeyResult::Continue
                }
            }
            KeyCode::Backspace => {
                if let Some(value) = self.active_value_mut() {
                    value.pop();
                }
                SettingsFormKeyResult::Continue
            }
            KeyCode::Delete => {
                if let Some(value) = self.active_value_mut() {
                    value.clear();
                }
                SettingsFormKeyResult::Continue
            }
            KeyCode::Char(c) => {
                if self.selected_field != SettingsField::Theme
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    if let Some(value) = self.active_value_mut() {
                        value.push(c);
                    }
                }
                SettingsFormKeyResult::Continue
            }
            _ => SettingsFormKeyResult::Continue,
        }
    }

    fn to_input(&self) -> SettingsFormInput {
        SettingsFormInput {
            title: self.title.clone(),
            columns: self.columns_value.clone(),
            theme_key: self
                .theme_options
                .get(self.theme_index)
                .map(|opt| opt.key.clone())
                .unwrap_or_default(),
            custom_primary: self.custom_primary.clone(),
            custom_accent: self.custom_accent.clone(),
            custom_background: self.custom_background.clone(),
            custom_surface: self.custom_surface.clone(),
            custom_text: self.custom_text.clone(),
            custom_highlight: self.custom_highlight.clone(),
            custom_theme_name: self.custom_theme_name.clone(),
        }
    }

    fn next_field(&mut self) {
        self.selected_field = match self.selected_field {
            SettingsField::Title => SettingsField::Columns,
            SettingsField::Columns => SettingsField::Theme,
            SettingsField::Theme => SettingsField::CustomName,
            SettingsField::CustomName => SettingsField::CustomPrimary,
            SettingsField::CustomPrimary => SettingsField::CustomAccent,
            SettingsField::CustomAccent => SettingsField::CustomHighlight,
            SettingsField::CustomHighlight => SettingsField::CustomBackground,
            SettingsField::CustomBackground => SettingsField::CustomSurface,
            SettingsField::CustomSurface => SettingsField::CustomText,
            SettingsField::CustomText => SettingsField::Title,
        };
    }

    fn previous_field(&mut self) {
        self.selected_field = match self.selected_field {
            SettingsField::Title => SettingsField::CustomText,
            SettingsField::Columns => SettingsField::Title,
            SettingsField::Theme => SettingsField::Columns,
            SettingsField::CustomName => SettingsField::Theme,
            SettingsField::CustomPrimary => SettingsField::CustomName,
            SettingsField::CustomAccent => SettingsField::CustomPrimary,
            SettingsField::CustomHighlight => SettingsField::CustomAccent,
            SettingsField::CustomBackground => SettingsField::CustomHighlight,
            SettingsField::CustomSurface => SettingsField::CustomBackground,
            SettingsField::CustomText => SettingsField::CustomSurface,
        };
    }

    fn active_value_mut(&mut self) -> Option<&mut String> {
        match self.selected_field {
            SettingsField::Title => Some(&mut self.title),
            SettingsField::Columns => Some(&mut self.columns_value),
            SettingsField::Theme => None,
            SettingsField::CustomName => Some(&mut self.custom_theme_name),
            SettingsField::CustomPrimary => Some(&mut self.custom_primary),
            SettingsField::CustomAccent => Some(&mut self.custom_accent),
            SettingsField::CustomBackground => Some(&mut self.custom_background),
            SettingsField::CustomSurface => Some(&mut self.custom_surface),
            SettingsField::CustomText => Some(&mut self.custom_text),
            SettingsField::CustomHighlight => Some(&mut self.custom_highlight),
        }
    }

    fn with_selected_theme_colors(mut self) -> Self {
        self.populate_custom_fields_from_selection();
        self
    }

    fn populate_custom_fields_from_selection(&mut self) {
        if self.theme_options.is_empty() {
            return;
        }
        if self.theme_index >= self.theme_options.len() {
            self.theme_index = 0;
        }
        if let Some(option) = self.theme_options.get(self.theme_index) {
            self.custom_primary = option.primary_hex.clone();
            self.custom_accent = option.accent_hex.clone();
            self.custom_highlight = option.highlight_hex.clone();
            self.custom_background = option.background_hex.clone();
            self.custom_surface = option.surface_hex.clone();
            self.custom_text = option.text_hex.clone();
            if parse_saved_theme_key(&option.key).is_some() {
                self.custom_theme_name = option.label.clone();
            } else if option.key != CUSTOM_THEME_KEY {
                self.custom_theme_name.clear();
            }
        }
    }

    fn next_theme(&mut self) {
        if self.theme_options.is_empty() {
            return;
        }
        self.theme_index = (self.theme_index + 1) % self.theme_options.len();
        self.populate_custom_fields_from_selection();
    }

    fn previous_theme(&mut self) {
        if self.theme_options.is_empty() {
            return;
        }
        if self.theme_index == 0 {
            self.theme_index = self.theme_options.len() - 1;
        } else {
            self.theme_index -= 1;
        }
        self.populate_custom_fields_from_selection();
    }
}

fn build_settings_shortcut_line(
    app: &AppState,
    include_delete: bool,
) -> (Line<'static>, Vec<SettingsShortcutSegment>, u16) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut segments: Vec<SettingsShortcutSegment> = Vec::new();
    let mut cursor: u16 = 0;
    let key_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(app.theme.surface);
    let entries: Vec<(&str, &str, SettingsShortcutAction)> = vec![
        ("Tab", " Move", SettingsShortcutAction::NextField),
        ("↵", " Save", SettingsShortcutAction::Submit),
        ("Esc", " Cancel/Exit", SettingsShortcutAction::Cancel),
    ];

    for (idx, (key, label, action)) in entries.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" | ", label_style));
            cursor = cursor.saturating_add(3);
        }
        let entry_start = cursor;
        spans.push(Span::styled(*key, key_style));
        cursor = cursor.saturating_add(key.chars().count() as u16);
        if !label.is_empty() {
            spans.push(Span::styled(*label, label_style));
            cursor = cursor.saturating_add(label.chars().count() as u16);
        }
        segments.push(SettingsShortcutSegment {
            start: entry_start,
            end: cursor,
            action: *action,
        });
    }

    if !entries.is_empty() {
        spans.push(Span::styled(" | ", label_style));
        cursor = cursor.saturating_add(3);
    }

    let left_start = cursor;
    spans.push(Span::styled("←", key_style));
    cursor = cursor.saturating_add("←".chars().count() as u16);
    segments.push(SettingsShortcutSegment {
        start: left_start,
        end: cursor,
        action: SettingsShortcutAction::PreviousTheme,
    });
    spans.push(Span::styled("/", label_style));
    cursor = cursor.saturating_add(1);
    let right_start = cursor;
    spans.push(Span::styled("→", key_style));
    cursor = cursor.saturating_add("→".chars().count() as u16);
    spans.push(Span::styled(" Select", label_style));
    cursor = cursor.saturating_add(" Select".len() as u16);
    segments.push(SettingsShortcutSegment {
        start: right_start,
        end: cursor,
        action: SettingsShortcutAction::NextTheme,
    });

    if include_delete {
        spans.push(Span::styled(" | ", label_style));
        cursor = cursor.saturating_add(3);
        let entry_start = cursor;
        spans.push(Span::styled("d", key_style));
        cursor = cursor.saturating_add(1);
        spans.push(Span::styled(" Delete theme", label_style));
        cursor = cursor.saturating_add(" Delete theme".len() as u16);
        segments.push(SettingsShortcutSegment {
            start: entry_start,
            end: cursor,
            action: SettingsShortcutAction::DeleteTheme,
        });
    }

    (Line::from(spans), segments, cursor)
}

#[derive(Clone)]
struct Theme {
    name: String,
    primary: Color,
    accent: Color,
    #[allow(dead_code)]
    highlight: Color,
    background: Color,
    surface: Color,
    text: Color,
    primary_hex: String,
    accent_hex: String,
    highlight_hex: String,
    background_hex: String,
    surface_hex: String,
    text_hex: String,
}

impl Theme {
    fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = fs::read_to_string(path)?;
            match serde_json::from_str::<ThemeFile>(&data) {
                Ok(file) => {
                    if let Some(skin) = file.skin {
                        if let Some(theme) = Theme::from_name(&skin) {
                            return Ok(theme);
                        }
                    }
                    if let Some(colors) = file.colors {
                        if let Some(theme) = Theme::from_colors("Custom", colors) {
                            return Ok(theme);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        let theme = Theme::from_name("nord").unwrap();
        theme.save(path)?;
        Ok(theme)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let file = ThemeFile {
            skin: Some(self.name.clone()),
            colors: Some(ThemeColorOverrides {
                primary: Some(self.primary_hex.clone()),
                accent: Some(self.accent_hex.clone()),
                highlight: Some(self.highlight_hex.clone()),
                background: Some(self.background_hex.clone()),
                surface: Some(self.surface_hex.clone()),
                text: Some(self.text_hex.clone()),
            }),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(&file)?)?;
        Ok(())
    }

    fn from_name(name: &str) -> Option<Self> {
        THEME_PRESETS
            .iter()
            .find(|preset| preset.0 == name)
            .map(|(_, def)| Theme::from_definition(name.to_string(), def))
    }

    fn from_definition(name: String, def: &ThemeDefinition) -> Theme {
        Theme::from_hexes(
            name,
            def.primary,
            def.accent,
            def.highlight,
            def.background,
            def.surface,
            def.text,
        )
    }

    fn from_colors(name: &str, overrides: ThemeColorOverrides) -> Option<Self> {
        Some(Theme::from_hexes(
            name.to_string(),
            overrides.primary.as_deref().unwrap_or("#5E81AC"),
            overrides.accent.as_deref().unwrap_or("#D08770"),
            overrides
                .highlight
                .as_deref()
                .or(overrides.accent.as_deref())
                .unwrap_or("#76B3C5"),
            overrides.background.as_deref().unwrap_or("#3B4252"),
            overrides.surface.as_deref().unwrap_or("#4C566A"),
            overrides.text.as_deref().unwrap_or("#ECEFF4"),
        ))
    }

    fn from_hexes(
        name: String,
        primary: &str,
        accent: &str,
        highlight: &str,
        background: &str,
        surface: &str,
        text: &str,
    ) -> Theme {
        Theme {
            name,
            primary: color_from_hex(primary).unwrap_or(Color::Blue),
            accent: color_from_hex(accent).unwrap_or(Color::Cyan),
            highlight: color_from_hex(highlight).unwrap_or(Color::Cyan),
            background: color_from_hex(background).unwrap_or(Color::Black),
            surface: color_from_hex(surface).unwrap_or(Color::DarkGray),
            text: color_from_hex(text).unwrap_or(Color::White),
            primary_hex: normalize_hex(primary),
            accent_hex: normalize_hex(accent),
            highlight_hex: normalize_hex(highlight),
            background_hex: normalize_hex(background),
            surface_hex: normalize_hex(surface),
            text_hex: normalize_hex(text),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ThemeFile {
    skin: Option<String>,
    colors: Option<ThemeColorOverrides>,
}

#[derive(Serialize, Deserialize)]
struct ThemeColorOverrides {
    primary: Option<String>,
    accent: Option<String>,
    #[serde(default)]
    highlight: Option<String>,
    background: Option<String>,
    surface: Option<String>,
    text: Option<String>,
}

struct ThemeDefinition {
    name: &'static str,
    primary: &'static str,
    accent: &'static str,
    highlight: &'static str,
    background: &'static str,
    surface: &'static str,
    text: &'static str,
}

impl ThemeOption {
    fn from_definition(key: &str, def: &ThemeDefinition) -> Self {
        Self {
            key: key.to_string(),
            label: def.name.to_string(),
            primary_hex: def.primary.to_string(),
            accent_hex: def.accent.to_string(),
            highlight_hex: def.highlight.to_string(),
            background_hex: def.background.to_string(),
            surface_hex: def.surface.to_string(),
            text_hex: def.text.to_string(),
        }
    }
}

const THEME_PRESETS: &[(&str, ThemeDefinition)] = &[
    (
        "classic",
        ThemeDefinition {
            name: "Midnight Classic",
            primary: "#6FC6D4",
            accent: "#0F1A2B",
            highlight: "#9FE6EC",
            background: "#314A63",
            surface: "#416079",
            text: "#F2F8FF",
        },
    ),
    (
        "nord",
        ThemeDefinition {
            name: "Nord",
            primary: "#5E81AC",
            accent: "#D08770",
            highlight: "#76B3C5",
            background: "#3B4252",
            surface: "#4C566A",
            text: "#ECEFF4",
        },
    ),
    (
        "gruvbox",
        ThemeDefinition {
            name: "Midnight Mist",
            primary: "#66C3CF",
            accent: "#0E1828",
            highlight: "#96DFE8",
            background: "#2C4156",
            surface: "#3B5A72",
            text: "#F4FBFF",
        },
    ),
    (
        "dracula",
        ThemeDefinition {
            name: "Midnight Dusk",
            primary: "#6BC6D7",
            accent: "#142033",
            highlight: "#A1E6EC",
            background: "#2E475F",
            surface: "#3E5D78",
            text: "#F5FBFF",
        },
    ),
    (
        "monokai",
        ThemeDefinition {
            name: "Midnight Deep",
            primary: "#5FC0CD",
            accent: "#0D1725",
            highlight: "#92DDE7",
            background: "#243A50",
            surface: "#344F68",
            text: "#F6FCFF",
        },
    ),
];

fn is_preset_theme_key(key: &str) -> bool {
    THEME_PRESETS
        .iter()
        .any(|(preset_key, _)| preset_key == &key)
}

fn saved_theme_key(index: usize) -> String {
    format!("{SAVED_THEME_PREFIX}{index}")
}

fn parse_saved_theme_key(key: &str) -> Option<usize> {
    key.strip_prefix(SAVED_THEME_PREFIX)?.parse::<usize>().ok()
}

#[derive(Clone, Copy)]
struct FooterShortcut {
    key: &'static str,
    label: &'static str,
    action: FooterAction,
}

#[derive(Clone, Copy)]
enum FooterAction {
    Quit,
    Edit,
    Execute,
    NewItem,
    Delete,
    Settings,
    ScanBin,
}

struct FooterSegment {
    start: u16,
    end: u16,
    action: FooterAction,
}

struct FooterLineData {
    line: Line<'static>,
    segments: Vec<FooterSegment>,
    total_width: u16,
}

const FOOTER_SHORTCUTS: &[FooterShortcut] = &[
    FooterShortcut {
        key: "q",
        label: " Exit",
        action: FooterAction::Quit,
    },
    FooterShortcut {
        key: "e",
        label: " Edit",
        action: FooterAction::Edit,
    },
    FooterShortcut {
        key: "↵",
        label: " Execute",
        action: FooterAction::Execute,
    },
    FooterShortcut {
        key: "n",
        label: " New Item",
        action: FooterAction::NewItem,
    },
    FooterShortcut {
        key: "d",
        label: " Delete",
        action: FooterAction::Delete,
    },
    FooterShortcut {
        key: "s",
        label: " Settings",
        action: FooterAction::Settings,
    },
    FooterShortcut {
        key: "^b",
        label: " Scan ./import",
        action: FooterAction::ScanBin,
    },
];

fn color_from_hex(value: &str) -> Option<Color> {
    let normalized = normalize_hex(value);
    let bytes = normalized.as_bytes();
    let r = u8::from_str_radix(std::str::from_utf8(&bytes[1..3]).ok()?, 16).ok()?;
    let g = u8::from_str_radix(std::str::from_utf8(&bytes[3..5]).ok()?, 16).ok()?;
    let b = u8::from_str_radix(std::str::from_utf8(&bytes[5..7]).ok()?, 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn normalize_hex(value: &str) -> String {
    let mut cleaned = value.trim().to_string();
    if !cleaned.starts_with('#') {
        cleaned.insert(0, '#');
    }
    if cleaned.len() != 7 {
        return "#ffffff".into();
    }
    cleaned
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String> {
    println!("{prompt} [{default}]: ");
    print!("> ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_optional(prompt: &str) -> Result<String> {
    println!("{prompt}: ");
    print!("> ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_bool(prompt: &str, default: bool) -> Result<bool> {
    let default_hint = if default { "Y/n" } else { "y/N" };
    println!("{prompt} ({default_hint})");
    print!("> ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    let trimmed = buf.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        Ok(default)
    } else if trimmed == "y" || trimmed == "yes" {
        Ok(true)
    } else if trimmed == "n" || trimmed == "no" {
        Ok(false)
    } else {
        Ok(default)
    }
}

fn sanitize_hex_color_input(input: &str) -> Option<String> {
    let mut value = input.trim().to_string();
    if !value.starts_with('#') {
        value.insert(0, '#');
    }
    if value.len() != 7 {
        return None;
    }
    if u32::from_str_radix(&value[1..], 16).is_ok() {
        Some(value)
    } else {
        None
    }
}

fn hex_strings_equal(a: &str, b: &str) -> bool {
    match (sanitize_hex_color_input(a), sanitize_hex_color_input(b)) {
        (Some(mut left), Some(mut right)) => {
            left.make_ascii_lowercase();
            right.make_ascii_lowercase();
            left == right
        }
        _ => false,
    }
}

fn parse_color_field(value: &str) -> Result<Option<String>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    sanitize_hex_color_input(trimmed)
        .map(Some)
        .ok_or_else(|| "Colors must use #RRGGBB format".to_string())
}

fn require_color_field(value: &str, label: &str) -> Result<String, String> {
    parse_color_field(value)?
        .ok_or_else(|| format!("{label} color is required when creating a custom theme"))
}

fn filename_to_label(name: &str) -> String {
    name.replace(['_', '-'], " ")
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_executable_file(entry: &fs::DirEntry) -> bool {
    #[cfg(unix)]
    {
        entry
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("exe"))
            .unwrap_or(true)
    }
}
