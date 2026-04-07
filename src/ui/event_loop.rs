use crate::{bourne, diff};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::{prelude::*, Terminal};
use std::collections::HashMap;
use std::io;

use super::rebase::prepare_rebase_changes;
use super::render::{align_lines, aligned_line_count, build_unified_lines, ui, unified_line_count};
use super::types::*;

fn commit_rebase_changes(app: &mut App) {
    let mut any_applied = false;
    let mut errors = Vec::new();

    for (file, changes) in &app.rebase_changes {
        let mut operations = Vec::new();

        for change in changes {
            if change.state != ChangeState::Accepted {
                continue;
            }

            if change.is_base {
                if let Some(paired_content) = &change.paired_content {
                    // Replace: swap old content with incoming content
                    let clean = paired_content.strip_prefix('+').unwrap_or(paired_content);
                    operations.push(diff::ChangeOp::Replace(change.line_num, clean.to_string()));
                } else {
                    // Delete: remove the line entirely
                    operations.push(diff::ChangeOp::Delete(change.line_num));
                }
            } else {
                // Insert: use computed base position
                let clean = change.content.strip_prefix('+').unwrap_or(&change.content);
                let base_pos = change.base_insert_pos.unwrap_or(change.line_num);
                operations.push(diff::ChangeOp::Insert {
                    base_pos,
                    order: change.line_num,
                    content: clean.to_string(),
                });
            }
        }

        if !operations.is_empty() {
            any_applied = true;
            if let Err(e) = diff::apply_changes(file, &operations) {
                errors.push(format!("{}: {}", file, e));
            }
        }
    }

    // Surface feedback through the UI
    if !errors.is_empty() {
        app.status_message = Some(format!("Error: {}", errors.join("; ")));
    } else if any_applied {
        app.status_message = Some("Changes applied successfully!".to_string());
    } else {
        app.status_message = Some("No accepted changes to apply.".to_string());
    }

    // Return to diff mode
    app.app_mode = AppMode::Diff;
}

fn set_change_state(app: &mut App, state: ChangeState) {
    if let Some(file) = app.file_names.get(app.current_file_idx) {
        if let Some(changes) = app.rebase_changes.get_mut(file) {
            if app.current_change_idx < changes.len() {
                changes[app.current_change_idx].state = state;
                if app.current_change_idx < changes.len() - 1 {
                    app.current_change_idx += 1;
                }
            }
        }
    }
}

fn navigate_rebase_file(app: &mut App, forward: bool) {
    let len = app.file_names.len();
    if len == 0 {
        return;
    }
    for offset in 1..len {
        let idx = if forward {
            (app.current_file_idx + offset) % len
        } else {
            (app.current_file_idx + len - offset) % len
        };
        if let Some(changes) = app.rebase_changes.get(&app.file_names[idx]) {
            if !changes.is_empty() {
                app.current_file_idx = idx;
                app.current_change_idx = 0;
                return;
            }
        }
    }
}

/// Re-fetch the diff and update app state if changes are detected.
fn refresh_if_changed(app: &mut App) {
    let new_data = match diff::refresh_diff(&app.diff_source) {
        Ok(data) => data,
        Err(_) => return, // Silently skip on error (e.g., git not available)
    };

    let (mut new_file_changes, new_left_label, new_right_label) = new_data;

    // Apply include/exclude filters
    app.file_filter.apply(&mut new_file_changes);

    // Only update if the diff actually changed
    if new_file_changes == app.file_changes {
        return;
    }

    // Remember current file name to restore position
    let current_file_name = app.file_names.get(app.current_file_idx).cloned();

    // Build new sorted file list
    let mut new_file_names: Vec<String> = new_file_changes.keys().cloned().collect();
    new_file_names.sort();

    // Restore file index by name, or clamp
    let new_idx = current_file_name
        .as_ref()
        .and_then(|name| new_file_names.iter().position(|n| n == name))
        .unwrap_or(0)
        .min(new_file_names.len().saturating_sub(1));

    // Keep scroll positions for files that still exist, add new ones
    let mut new_scroll_positions = HashMap::new();
    for name in &new_file_names {
        let pos = app.scroll_positions.get(name).copied().unwrap_or(0);
        new_scroll_positions.insert(name.clone(), pos);
    }

    app.file_changes = new_file_changes;
    app.left_label = new_left_label;
    app.right_label = new_right_label;
    app.file_names = new_file_names;
    app.scroll_positions = new_scroll_positions;
    app.current_file_idx = new_idx;

    // Clear rebase state since the diff changed underneath
    if matches!(app.app_mode, AppMode::Rebase) {
        app.app_mode = AppMode::Diff;
        app.rebase_changes.clear();
        app.current_change_idx = 0;
    }
}

/// Get total line count for the current file's diff content.
fn diff_line_count(app: &App) -> usize {
    let file = match app.file_names.get(app.current_file_idx) {
        Some(f) => f,
        None => return 0,
    };
    let (base, head) = match app.file_changes.get(file) {
        Some(c) => c,
        None => return 0,
    };
    match app.view_mode {
        ViewMode::SideBySide => aligned_line_count(base, head),
        ViewMode::Unified => unified_line_count(base, head),
    }
}

/// Adjust scroll so the cursor line stays visible within the viewport.
fn scroll_to_cursor(app: &mut App, visible_height: usize) {
    let file = match app.file_names.get(app.current_file_idx) {
        Some(f) => f.clone(),
        None => return,
    };
    let scroll = *app.scroll_positions.get(&file).unwrap_or(&0);
    if app.cursor_line < scroll {
        app.scroll_positions.insert(file, app.cursor_line);
    } else if app.cursor_line >= scroll + visible_height {
        app.scroll_positions
            .insert(file, app.cursor_line - visible_height + 1);
    }
}

/// Get the line at the cursor position for commenting.
/// Returns (filename, line_number, cleaned_line_content).
fn get_line_at_cursor(app: &App) -> Option<(String, usize, String)> {
    let file = app.file_names.get(app.current_file_idx)?.clone();
    let (base_lines, head_lines) = app.file_changes.get(&file)?;

    let lines = match app.view_mode {
        ViewMode::SideBySide => {
            let (_, aligned_head) = align_lines(base_lines, head_lines);
            aligned_head
        }
        ViewMode::Unified => build_unified_lines(base_lines, head_lines),
    };

    // Find the first non-gap line at or after the cursor position
    for line in lines.iter().skip(app.cursor_line) {
        let (line_num, content) = line;
        if *line_num > 0 {
            let cleaned = content
                .strip_prefix('+')
                .or_else(|| content.strip_prefix('-'))
                .or_else(|| content.strip_prefix(' '))
                .unwrap_or(content)
                .to_string();
            return Some((file, *line_num, cleaned));
        }
    }

    None
}

/// Returns `Ok(true)` when the app exits after a successful rebase
/// (so the caller can print a message), `Ok(false)` for normal exit.
pub fn run_ui<B: Backend>(terminal: &mut Terminal<B>, mut app: App) -> io::Result<bool>
where
    std::io::Error: From<B::Error>,
{
    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        // Poll with a timeout so we can detect file changes even without
        // user input.  When the timeout expires, re-run git diff and refresh
        // the view if anything changed.
        if !event::poll(std::time::Duration::from_secs(1))? {
            refresh_if_changed(&mut app);
            continue;
        }

        // Drain all queued events before redrawing.  This batches rapid
        // scroll inputs so the UI stays snappy.
        let first = event::read()?;
        let mut events = vec![first];
        while event::poll(std::time::Duration::ZERO)? {
            events.push(event::read()?);
        }

        for ev in events {
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Clear transient status message on any keypress
                    app.status_message = None;

                    // Handle help modal if shown
                    if app.show_help_modal {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                                app.show_help_modal = false;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle rebase modal if shown
                    if app.show_rebase_modal {
                        match key.code {
                            KeyCode::Char('r') => match diff::get_upstream_branch() {
                                Ok(Some(upstream)) => match diff::perform_rebase(&upstream) {
                                    Ok(true) => {
                                        app.show_rebase_modal = false;
                                        return Ok(true);
                                    }
                                    Ok(false) => {
                                        app.rebase_notification = Some(
                                            "Rebase failed due to conflicts and was rolled back."
                                                .to_string(),
                                        );
                                    }
                                    Err(e) => {
                                        app.show_rebase_modal = false;
                                        app.status_message = Some(format!("Error: {}", e));
                                    }
                                },
                                Ok(None) => {
                                    app.show_rebase_modal = false;
                                    app.status_message =
                                        Some("No upstream branch configured.".to_string());
                                }
                                Err(e) => {
                                    app.show_rebase_modal = false;
                                    app.status_message = Some(format!("Error: {}", e));
                                }
                            },
                            KeyCode::Char('i') | KeyCode::Esc => {
                                app.show_rebase_modal = false;
                            }
                            _ => {}
                        }
                        continue; // Skip other key processing when modal is shown
                    }

                    // Handle comment input if active
                    if let Some(ref mut ci) = app.comment_input {
                        match key.code {
                            KeyCode::Esc => {
                                app.comment_input = None;
                            }
                            KeyCode::Enter => {
                                if !ci.text.trim().is_empty() {
                                    let msg = format!(
                                        "[giff] {}:{}: \"{}\" — {}",
                                        ci.file, ci.line_num, ci.line_content, ci.text
                                    );
                                    match diff::git_repo_root() {
                                        Ok(dir) => match bourne::send_comment(&dir, &msg) {
                                            Ok(()) => {
                                                app.status_message =
                                                    Some("Comment sent to Claude Code".to_string());
                                            }
                                            Err(e) => {
                                                app.status_message =
                                                    Some(format!("Error: {}", e));
                                            }
                                        },
                                        Err(e) => {
                                            app.status_message = Some(format!("Error: {}", e));
                                        }
                                    }
                                }
                                app.comment_input = None;
                            }
                            KeyCode::Backspace => {
                                if ci.cursor_pos > 0 {
                                    ci.text.remove(ci.cursor_pos - 1);
                                    ci.cursor_pos -= 1;
                                }
                            }
                            KeyCode::Delete => {
                                if ci.cursor_pos < ci.text.len() {
                                    ci.text.remove(ci.cursor_pos);
                                }
                            }
                            KeyCode::Left => {
                                ci.cursor_pos = ci.cursor_pos.saturating_sub(1);
                            }
                            KeyCode::Right => {
                                if ci.cursor_pos < ci.text.len() {
                                    ci.cursor_pos += 1;
                                }
                            }
                            KeyCode::Home => {
                                ci.cursor_pos = 0;
                            }
                            KeyCode::End => {
                                ci.cursor_pos = ci.text.len();
                            }
                            KeyCode::Char(ch) => {
                                ci.text.insert(ci.cursor_pos, ch);
                                ci.cursor_pos += 1;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            match app.app_mode {
                                AppMode::Diff => return Ok(false),
                                AppMode::Rebase => {
                                    // Return to diff mode without applying changes
                                    app.app_mode = AppMode::Diff;
                                }
                            }
                        }
                        KeyCode::Char('r') => {
                            if let AppMode::Diff = app.app_mode {
                                app.app_mode = AppMode::Rebase;
                                prepare_rebase_changes(&mut app);
                            }
                        }
                        KeyCode::Char('a') => {
                            if let AppMode::Rebase = app.app_mode {
                                set_change_state(&mut app, ChangeState::Accepted);
                            }
                        }
                        KeyCode::Char('x') => {
                            if let AppMode::Rebase = app.app_mode {
                                set_change_state(&mut app, ChangeState::Rejected);
                            }
                        }
                        KeyCode::Char('c') => match app.app_mode {
                            AppMode::Rebase => {
                                commit_rebase_changes(&mut app);
                            }
                            AppMode::Diff => {
                                if let Some((file, line_num, line_content)) =
                                    get_line_at_cursor(&app)
                                {
                                    app.comment_input = Some(CommentInput {
                                        file,
                                        line_num,
                                        line_content,
                                        text: String::new(),
                                        cursor_pos: 0,
                                    });
                                }
                            }
                        },
                        KeyCode::Char('j') | KeyCode::Down => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    if app.current_file_idx + 1 < app.file_names.len() {
                                        app.current_file_idx += 1;
                                        app.cursor_line = 0;
                                    }
                                }
                                Pane::DiffContent => {
                                    let total = diff_line_count(&app);
                                    if total > 0 && app.cursor_line + 1 < total {
                                        app.cursor_line += 1;
                                        let page =
                                            terminal.size()?.height.saturating_sub(6) as usize;
                                        scroll_to_cursor(&mut app, page);
                                    }
                                }
                            },
                            AppMode::Rebase => {
                                if let Some(file) = app.file_names.get(app.current_file_idx) {
                                    if let Some(changes) = app.rebase_changes.get(file) {
                                        if !changes.is_empty()
                                            && app.current_change_idx < changes.len() - 1
                                        {
                                            app.current_change_idx += 1;
                                        }
                                    }
                                }
                            }
                        },
                        KeyCode::Char('k') | KeyCode::Up => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    if app.current_file_idx > 0 {
                                        app.current_file_idx -= 1;
                                        app.cursor_line = 0;
                                    }
                                }
                                Pane::DiffContent => {
                                    if app.cursor_line > 0 {
                                        app.cursor_line -= 1;
                                        let page =
                                            terminal.size()?.height.saturating_sub(6) as usize;
                                        scroll_to_cursor(&mut app, page);
                                    }
                                }
                            },
                            AppMode::Rebase => {
                                if app.current_change_idx > 0 {
                                    app.current_change_idx -= 1;
                                }
                            }
                        },
                        KeyCode::PageDown => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    let page = terminal.size()?.height.saturating_sub(6) as usize;
                                    app.current_file_idx = (app.current_file_idx + page)
                                        .min(app.file_names.len().saturating_sub(1));
                                }
                                Pane::DiffContent => {
                                    let page =
                                        terminal.size()?.height.saturating_sub(6) as usize;
                                    let total = diff_line_count(&app);
                                    app.cursor_line = app
                                        .cursor_line
                                        .saturating_add(page)
                                        .min(total.saturating_sub(1));
                                    scroll_to_cursor(&mut app, page);
                                }
                            },
                            AppMode::Rebase => {
                                if let Some(file) = app.file_names.get(app.current_file_idx) {
                                    if let Some(changes) = app.rebase_changes.get(file) {
                                        if !changes.is_empty() {
                                            let page =
                                                terminal.size()?.height.saturating_sub(6) as usize;
                                            app.current_change_idx = (app.current_change_idx
                                                + page)
                                                .min(changes.len() - 1);
                                        }
                                    }
                                }
                            }
                        },
                        KeyCode::PageUp => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    let page = terminal.size()?.height.saturating_sub(6) as usize;
                                    app.current_file_idx =
                                        app.current_file_idx.saturating_sub(page);
                                }
                                Pane::DiffContent => {
                                    let page =
                                        terminal.size()?.height.saturating_sub(6) as usize;
                                    app.cursor_line = app.cursor_line.saturating_sub(page);
                                    scroll_to_cursor(&mut app, page);
                                }
                            },
                            AppMode::Rebase => {
                                let page = terminal.size()?.height.saturating_sub(6) as usize;
                                app.current_change_idx =
                                    app.current_change_idx.saturating_sub(page);
                            }
                        },
                        KeyCode::Home => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    app.current_file_idx = 0;
                                }
                                Pane::DiffContent => {
                                    app.cursor_line = 0;
                                    if let Some(file) = app.file_names.get(app.current_file_idx) {
                                        app.scroll_positions.insert(file.clone(), 0);
                                    }
                                }
                            },
                            AppMode::Rebase => {
                                app.current_change_idx = 0;
                            }
                        },
                        KeyCode::End => match app.app_mode {
                            AppMode::Diff => match app.focused_pane {
                                Pane::FileList => {
                                    app.current_file_idx = app.file_names.len().saturating_sub(1);
                                }
                                Pane::DiffContent => {
                                    let total = diff_line_count(&app);
                                    app.cursor_line = total.saturating_sub(1);
                                    if let Some(file) = app.file_names.get(app.current_file_idx) {
                                        app.scroll_positions.insert(file.clone(), usize::MAX);
                                    }
                                }
                            },
                            AppMode::Rebase => {
                                if let Some(file) = app.file_names.get(app.current_file_idx) {
                                    if let Some(changes) = app.rebase_changes.get(file) {
                                        if !changes.is_empty() {
                                            app.current_change_idx = changes.len() - 1;
                                        }
                                    }
                                }
                            }
                        },
                        KeyCode::Tab => {
                            // Toggle between file list and diff content (only in diff mode)
                            if let AppMode::Diff = app.app_mode {
                                app.focused_pane = match app.focused_pane {
                                    Pane::FileList => Pane::DiffContent,
                                    Pane::DiffContent => Pane::FileList,
                                }
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            if let AppMode::Diff = app.app_mode {
                                app.focused_pane = Pane::FileList;
                            }
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            if let AppMode::Diff = app.app_mode {
                                app.focused_pane = Pane::DiffContent;
                            }
                        }
                        KeyCode::Char('t') => {
                            // Cycle through available themes
                            if !app.theme_cycle.is_empty() {
                                app.theme_cycle_idx =
                                    (app.theme_cycle_idx + 1) % app.theme_cycle.len();
                                app.theme = app.theme_cycle[app.theme_cycle_idx].clone();
                            }
                        }
                        KeyCode::Char('u') => {
                            // Toggle between unified and side-by-side view (only in diff mode)
                            if let AppMode::Diff = app.app_mode {
                                app.view_mode = match app.view_mode {
                                    ViewMode::SideBySide => ViewMode::Unified,
                                    ViewMode::Unified => ViewMode::SideBySide,
                                }
                            }
                        }
                        KeyCode::Char('n') => {
                            if let AppMode::Rebase = app.app_mode {
                                navigate_rebase_file(&mut app, true);
                            }
                        }
                        KeyCode::Char('p') => {
                            if let AppMode::Rebase = app.app_mode {
                                navigate_rebase_file(&mut app, false);
                            }
                        }
                        KeyCode::Char('?') => {
                            app.show_help_modal = true;
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => {
                    if app.show_help_modal || app.show_rebase_modal {
                        continue;
                    }
                    let size = terminal.size()?;
                    let scroll_amount: usize = 3;
                    match mouse.kind {
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                            if mouse.row == 0 || mouse.row >= size.height.saturating_sub(1) {
                                continue;
                            }
                            let is_down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                            match app.app_mode {
                                AppMode::Diff => {
                                    let file_list_width = size.width / 5;
                                    if mouse.column < file_list_width {
                                        if !app.file_names.is_empty() {
                                            if is_down {
                                                app.current_file_idx = (app.current_file_idx
                                                    + scroll_amount)
                                                    .min(app.file_names.len() - 1);
                                            } else {
                                                app.current_file_idx = app
                                                    .current_file_idx
                                                    .saturating_sub(scroll_amount);
                                            }
                                        }
                                    } else if let Some(file) =
                                        app.file_names.get(app.current_file_idx)
                                    {
                                        let scroll = *app.scroll_positions.get(file).unwrap_or(&0);
                                        let new_scroll = if is_down {
                                            scroll.saturating_add(scroll_amount)
                                        } else {
                                            scroll.saturating_sub(scroll_amount)
                                        };
                                        app.scroll_positions.insert(file.clone(), new_scroll);
                                    }
                                }
                                AppMode::Rebase => {
                                    if let Some(file) = app.file_names.get(app.current_file_idx) {
                                        if let Some(changes) = app.rebase_changes.get(file) {
                                            if !changes.is_empty() {
                                                if is_down {
                                                    app.current_change_idx =
                                                        (app.current_change_idx + scroll_amount)
                                                            .min(changes.len() - 1);
                                                } else {
                                                    app.current_change_idx = app
                                                        .current_change_idx
                                                        .saturating_sub(scroll_amount);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        } // end event batch
    }
}

#[cfg(test)]
mod tests {
    use super::super::theme::Theme;
    use super::*;
    use crate::diff::{DiffSource, FileChanges};

    fn make_app(file_names: Vec<&str>, changes_for: Vec<&str>) -> App {
        let file_names: Vec<String> = file_names.into_iter().map(|s| s.to_string()).collect();
        let mut rebase_changes = HashMap::new();
        for name in &file_names {
            let changes = if changes_for.contains(&name.as_str()) {
                vec![Change {
                    line_num: 1,
                    content: "-old".to_string(),
                    paired_content: None,
                    state: ChangeState::Unselected,
                    is_base: true,
                    context: vec![],
                    base_insert_pos: None,
                }]
            } else {
                vec![]
            };
            rebase_changes.insert(name.clone(), changes);
        }
        let file_changes: FileChanges = HashMap::new();
        App {
            file_changes,
            left_label: String::new(),
            right_label: String::new(),
            diff_source: DiffSource::Uncommitted,
            current_file_idx: 0,
            file_names,
            scroll_positions: HashMap::new(),
            focused_pane: Pane::FileList,
            view_mode: ViewMode::SideBySide,
            app_mode: AppMode::Rebase,
            rebase_changes,
            current_change_idx: 0,
            rebase_notification: None,
            show_rebase_modal: false,
            status_message: None,
            show_help_modal: false,
            theme: Theme::dark(),
            theme_cycle: vec![Theme::dark(), Theme::light()],
            theme_cycle_idx: 0,
            file_filter: crate::diff::FileFilter::new(&[], &[]).unwrap(),
            comment_input: None,
            cursor_line: 0,
        }
    }

    #[test]
    fn navigate_forward_finds_next_file_with_changes() {
        let mut app = make_app(vec!["a.rs", "b.rs", "c.rs"], vec!["a.rs", "c.rs"]);
        app.current_file_idx = 0;
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 2); // skips b.rs (empty)
    }

    #[test]
    fn navigate_forward_wraps_around() {
        let mut app = make_app(vec!["a.rs", "b.rs", "c.rs"], vec!["a.rs", "c.rs"]);
        app.current_file_idx = 2;
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 0); // wraps to a.rs
    }

    #[test]
    fn navigate_backward_finds_previous_file_with_changes() {
        let mut app = make_app(vec!["a.rs", "b.rs", "c.rs"], vec!["a.rs", "c.rs"]);
        app.current_file_idx = 2;
        navigate_rebase_file(&mut app, false);
        assert_eq!(app.current_file_idx, 0); // skips b.rs
    }

    #[test]
    fn navigate_backward_wraps_around() {
        let mut app = make_app(vec!["a.rs", "b.rs", "c.rs"], vec!["a.rs", "c.rs"]);
        app.current_file_idx = 0;
        navigate_rebase_file(&mut app, false);
        assert_eq!(app.current_file_idx, 2); // wraps to c.rs
    }

    #[test]
    fn navigate_no_files_with_changes_stays_put() {
        let mut app = make_app(vec!["a.rs", "b.rs"], vec![]);
        app.current_file_idx = 0;
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 0); // unchanged
    }

    #[test]
    fn navigate_single_file_with_changes_stays_put() {
        let mut app = make_app(vec!["a.rs", "b.rs"], vec!["a.rs"]);
        app.current_file_idx = 0;
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 0); // only file with changes
    }

    #[test]
    fn navigate_empty_file_list() {
        let mut app = make_app(vec![], vec![]);
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 0);
    }

    #[test]
    fn navigate_resets_change_idx() {
        let mut app = make_app(vec!["a.rs", "b.rs"], vec!["a.rs", "b.rs"]);
        app.current_file_idx = 0;
        app.current_change_idx = 5;
        navigate_rebase_file(&mut app, true);
        assert_eq!(app.current_file_idx, 1);
        assert_eq!(app.current_change_idx, 0);
    }
}
