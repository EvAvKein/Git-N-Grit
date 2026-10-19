//! Rendering: two panes, prompt, picker, confirm, labels, relative times.

use std::collections::HashMap;
use std::path::{Component, Path};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use uuid::Uuid;

use crate::controller::{Controller, RepoSelection, Row, UiState};
use crate::model::storage::repo::Event;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The colours in use. Named ANSI colours only, so the terminal's own theme
/// decides what they look like. Bold and dim are modifiers, not colours, and
/// survive `NO_COLOR`.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    color: bool,
}

impl Palette {
    /// Colour unless `NO_COLOR` is set and non-empty, per no-color.org.
    pub fn detect() -> Palette {
        Palette {
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        }
    }

    pub fn plain() -> Palette {
        Palette { color: false }
    }

    #[cfg(test)]
    fn colored() -> Palette {
        Palette { color: true }
    }

    fn tint(self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    /// The selection marker.
    fn accent(self) -> Style {
        self.tint(Color::Cyan)
    }

    /// XP earned.
    fn gain(self) -> Style {
        self.tint(Color::Green)
    }

    /// What is not working.
    fn alert(self) -> Style {
        self.tint(Color::Red)
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// `> ` on the selected line and blanks on the rest, so lines stay aligned.
fn marker(selected: bool, palette: Palette) -> Span<'static> {
    if selected {
        Span::styled("> ", palette.accent())
    } else {
        Span::raw("  ")
    }
}

/// Draw one frame. `utc_offset_secs` is
/// [`crate::model::git::local_utc_offset`], read once at startup and handed
/// down rather than called here.
pub fn draw(
    frame: &mut Frame,
    controller: &mut Controller,
    utc_offset_secs: i32,
    palette: Palette,
) {
    let area = frame.area();
    let [main, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
    let left_width = ((main.width as u32 * 45 / 100) as u16).clamp(28, 40);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Min(0)]).areas(main);

    render_left(frame, left, controller, palette);
    render_right(frame, right, controller, utc_offset_secs, palette);
    render_hint(frame, hint);

    match controller.mode() {
        UiState::List => {}
        UiState::AddPrompt {
            input,
            error,
            checking,
        } => render_prompt(frame, area, input, error.as_deref(), *checking, palette),
        UiState::Picker {
            lines,
            selected_line,
            ..
        } => render_picker(frame, area, controller, lines, *selected_line, palette),
        UiState::ConfirmRemove { target, confirmed } => {
            render_confirm(frame, area, controller, target, *confirmed, palette)
        }
    }
}

// ---- left pane -------------------------------------------------------

fn render_left(frame: &mut Frame, area: Rect, controller: &Controller, palette: Palette) {
    let block = Block::bordered().title(" Git-N-Grit ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let labels = repo_labels(controller);
    let total = controller.row_count();
    let visible = inner.height as usize;
    let top = left_scroll_top(controller.selected(), total, visible);

    let mut lines = Vec::with_capacity(visible);
    for i in top..total.min(top + visible) {
        let selected = i == controller.selected();
        let (label, xp, inactive) = match controller.row(i) {
            Some(Row::Add) => ("+ Add repo…".to_string(), None, false),
            Some(Row::Repo(id)) => {
                let label = labels.get(&id).cloned().unwrap_or_default();
                let xp = controller.state(id).map(|s| s.total_xp).unwrap_or_default();
                (label, Some(xp), controller.inactive_reason(id).is_some())
            }
            Some(Row::BadFile(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                (name, None, true)
            }
            None => continue,
        };
        lines.push(left_row(&label, xp, inactive, selected, palette));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One left-pane row. `xp` is `None` where the row has no total.
fn left_row(
    label: &str,
    xp: Option<u64>,
    inactive: bool,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let mut style = if inactive { dim() } else { Style::default() };
    if selected {
        style = style.patch(bold());
    }
    let mut spans = vec![
        marker(selected, palette),
        Span::styled(label.to_string(), style),
    ];
    if inactive {
        spans.push(Span::styled(" !", style.patch(palette.alert())));
    }
    if let Some(xp) = xp {
        spans.push(Span::styled(format!(" {}xp", comma(xp)), style));
    }
    Line::from(spans)
}

/// Every registered repo's display label, keyed by id — extended leftward on
/// collision with another currently registered repo.
fn repo_labels(controller: &Controller) -> HashMap<Uuid, String> {
    let ids: Vec<Uuid> = (0..controller.row_count())
        .filter_map(|i| match controller.row(i) {
            Some(Row::Repo(id)) => Some(id),
            _ => None,
        })
        .collect();
    let dirs: Vec<&Path> = ids
        .iter()
        .filter_map(|id| controller.state(*id).and_then(|s| s.label_dir()))
        .collect();
    let labelled = labels(&dirs);
    ids.into_iter().zip(labelled).collect()
}

/// The last path component, extended leftward one component at a time only
/// against another path in `paths` that would otherwise read the same. A path
/// that runs out of components before it is unique keeps its full path — the
/// spec doesn't name a fallback, and that is the least surprising one.
pub fn labels(paths: &[&Path]) -> Vec<String> {
    let components: Vec<Vec<&str>> = paths.iter().map(|p| normal_components(p)).collect();
    let mut depth = vec![1usize; paths.len()];

    loop {
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, comps) in components.iter().enumerate() {
            groups.entry(suffix(comps, depth[i])).or_default().push(i);
        }
        let mut changed = false;
        for indices in groups.values().filter(|g| g.len() > 1) {
            for &i in indices {
                if depth[i] < components[i].len() {
                    depth[i] += 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    (0..paths.len())
        .map(|i| suffix(&components[i], depth[i]))
        .collect()
}

fn normal_components(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect()
}

fn suffix(comps: &[&str], depth: usize) -> String {
    let start = comps.len().saturating_sub(depth.max(1));
    comps[start..].join("/")
}

/// The topmost row a `visible`-row window has to start at to keep `selected`
/// inside it. Recomputed from scratch every frame, since nothing here is kept
/// between draws — pinning to whichever edge `selected` has crossed is the
/// stateless approximation of "scrolls to keep the selection in view".
fn left_scroll_top(selected: usize, total: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let max_top = total - visible;
    selected.saturating_sub(visible - 1).min(max_top)
}

// ---- right pane -------------------------------------------------------

fn render_right(
    frame: &mut Frame,
    area: Rect,
    controller: &mut Controller,
    utc_offset_secs: i32,
    palette: Palette,
) {
    let block = Block::bordered();
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(row) = controller.row(controller.selected()) else {
        return;
    };
    match row {
        Row::Add => {}
        Row::BadFile(path) => {
            let message = controller
                .bad_file(&path)
                .map(|e| e.message.as_str())
                .unwrap_or_default();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            frame.render_widget(
                Paragraph::new(vec![Line::from(name), Line::from(""), Line::from(message)]),
                inner,
            );
        }
        Row::Repo(id) => render_repo_pane(frame, inner, controller, id, utc_offset_secs, palette),
    }
}

fn render_repo_pane(
    frame: &mut Frame,
    inner: Rect,
    controller: &mut Controller,
    id: Uuid,
    utc_offset_secs: i32,
    palette: Palette,
) {
    let Some(state) = controller.state(id) else {
        return;
    };
    let label = state.label_dir().map(display_path).unwrap_or_default();
    let total = state.total_xp;
    let header = Line::from(format!("{label}          total {}xp", comma(total)));

    if let Some(reason) = controller.inactive_reason(id) {
        frame.render_widget(
            Paragraph::new(vec![
                header,
                Line::from(""),
                Line::styled(reason.to_string(), palette.alert()),
            ]),
            inner,
        );
        return;
    }

    let event_rows = inner.height.saturating_sub(1) as usize / 2;
    controller.set_page(event_rows.max(1));
    let Some(state) = controller.state(id) else {
        return;
    };
    let now = now_unix();
    let mut lines = vec![header];
    for event in state
        .events
        .iter()
        .rev()
        .skip(controller.scroll())
        .take(event_rows)
    {
        lines.push(event_time_line(event, now, utc_offset_secs));
        lines.push(event_stat_line(event, palette));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn event_time_line(event: &Event, now: i64, utc_offset_secs: i32) -> Line<'static> {
    let time = relative_time(event.timestamp, now, utc_offset_secs);
    let subject = if event.rewrite {
        format!("{} (redo)", event.subject)
    } else {
        event.subject.clone()
    };
    Line::from(vec![
        Span::styled(time, dim()),
        Span::raw(format!("  {subject}")),
    ])
}

fn event_stat_line(event: &Event, palette: Palette) -> Line<'static> {
    let stats = if event.rewrite {
        "rewritten".to_string()
    } else {
        format!("+{} −{}", event.added, event.deleted)
    };
    Line::from(vec![
        Span::raw(format!("          {stats}  ")),
        Span::styled(format!("+{}xp", event.xp), palette.gain()),
    ])
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `<1h → "Nm ago"`, `<1 local day → "Nh ago"`, the previous local calendar
/// day → `"yest."`, else `"MMM D"`. Elapsed time settles the first two
/// buckets before a calendar day is ever compared, so a commit made minutes
/// ago just after local midnight reads as `"Nm ago"` rather than `"yest."`.
pub fn relative_time(event_ts: i64, now: i64, utc_offset_secs: i32) -> String {
    let elapsed = (now - event_ts).max(0);
    if elapsed < 3600 {
        return format!("{}m ago", elapsed / 60);
    }
    if elapsed < 86_400 {
        return format!("{}h ago", elapsed / 3600);
    }
    let event_day = civil_day(event_ts, utc_offset_secs);
    let today = civil_day(now, utc_offset_secs);
    if event_day + 1 == today {
        return "yest.".to_string();
    }
    let (_, month, day) = civil_from_days(event_day);
    format!("{} {day}", MONTHS[(month - 1) as usize])
}

fn civil_day(ts: i64, utc_offset_secs: i32) -> i64 {
    (ts + utc_offset_secs as i64).div_euclid(86_400)
}

/// Days-since-epoch to (year, month, day), proleptic Gregorian. Howard
/// Hinnant's `civil_from_days` — <http://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Thousands-grouped, no locale support needed for a single "xp" suffix.
fn comma(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

// ---- key hint ----------------------------------------------------------

fn render_hint(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::styled("  ↑↓/jk move   enter select   a add   d remove", dim()),
        Line::styled(
            "  r refresh   PgUp/PgDn scroll   home newest   q quit",
            dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

// ---- overlays -----------------------------------------------------------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn render_prompt(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    error: Option<&str>,
    checking: bool,
    palette: Palette,
) {
    let popup = centered(area, 60, 5);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(" Git-N-Grit ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = vec![Line::from(format!("  Path: {input}"))];
    if checking {
        lines.push(Line::styled("  checking…", dim()));
    } else if let Some(error) = error {
        lines.push(Line::styled(format!("  {error}"), palette.alert()));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_picker(
    frame: &mut Frame,
    area: Rect,
    controller: &Controller,
    lines: &[RepoSelection],
    selected_line: usize,
    palette: Palette,
) {
    let popup = centered(area, 60, (lines.len() as u16 + 2).max(3));
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(" Git-N-Grit ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rendered: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let selected = i == selected_line;
            let text = match line {
                RepoSelection::Tracked(id) => match controller.state(*id) {
                    Some(state) => {
                        let paths: Vec<String> = state
                            .worktrees
                            .iter()
                            .map(|w| display_path(&w.source_dir))
                            .collect();
                        format!("{}  {}xp", paths.join(", "), comma(state.total_xp))
                    }
                    None => String::new(),
                },
                RepoSelection::New => "new repo".to_string(),
            };
            let style = if selected { bold() } else { Style::default() };
            Line::from(vec![marker(selected, palette), Span::styled(text, style)])
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), inner);
}

fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    controller: &Controller,
    target: &Row,
    confirmed: bool,
    palette: Palette,
) {
    let popup = centered(area, 55, 7);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(" Git-N-Grit ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = match target {
        Row::Repo(id) => {
            let (label, xp) = controller
                .state(*id)
                .map(|s| {
                    (
                        s.label_dir().map(display_path).unwrap_or_default(),
                        s.total_xp,
                    )
                })
                .unwrap_or_default();
            vec![
                Line::from(format!("  Remove {label}?")),
                Line::from(format!("  Its {}xp is deleted with it.", comma(xp))),
            ]
        }
        Row::BadFile(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            vec![Line::from(format!("  Remove {name}?"))]
        }
        Row::Add => vec![],
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        marker(!confirmed, palette),
        Span::raw("Cancel"),
    ]));
    lines.push(Line::from(vec![
        marker(confirmed, palette),
        Span::styled("Remove", palette.alert()),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---- second instance ----------------------------------------------------

/// What the second-instance screen is showing.
pub enum TakeoverScreen {
    /// The two options, with the second selected where `move_here`.
    Choose { move_here: bool },
    /// The handover file is written and the lock not yet ours.
    Waiting,
}

/// `holder` is [`crate::model::storage::lock::Holder::describe`].
pub fn draw_takeover(frame: &mut Frame, holder: &str, screen: &TakeoverScreen, palette: Palette) {
    let area = frame.area();
    let [main, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let block = Block::bordered().title(" Git-N-Grit ");
    let inner = block.inner(main);
    frame.render_widget(block, main);

    let mut lines = vec![
        Line::from(""),
        Line::from("  Git-N-Grit is already open in another terminal"),
        Line::from(""),
        Line::from(format!("    {holder}")),
        Line::from(""),
    ];
    match screen {
        TakeoverScreen::Choose { move_here } => {
            let option = |selected: bool, text: &'static str| {
                let style = if selected { bold() } else { Style::default() };
                Line::from(vec![
                    Span::raw("  "),
                    marker(selected, palette),
                    Span::styled(text, style),
                ])
            };
            lines.push(option(
                !move_here,
                "Close this one, leave the other running",
            ));
            lines.push(option(
                *move_here,
                "Move it here — the other one will close",
            ));
            frame.render_widget(
                Paragraph::new(Line::styled("  ↑↓/jk move   enter select   q quit", dim())),
                hint,
            );
        }
        TakeoverScreen::Waiting => {
            lines.push(Line::styled("  Waiting for the other one to close…", dim()));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn under_an_hour_reads_in_minutes() {
        assert_eq!(relative_time(1_000, 1_599, 0), "9m ago");
        assert_eq!(relative_time(1_000, 4_599, 0), "59m ago");
    }

    #[test]
    fn an_hour_and_over_reads_in_hours_until_a_day() {
        assert_eq!(relative_time(0, 3_600, 0), "1h ago");
        assert_eq!(relative_time(0, 86_399, 0), "23h ago");
    }

    #[test]
    fn a_full_day_falls_back_to_the_calendar() {
        // 1970-01-01T00:00:00Z to 1970-01-02T00:00:01Z: elapsed just over a
        // day, and the previous calendar day at offset zero.
        assert_eq!(relative_time(0, 86_401, 0), "yest.");
    }

    #[test]
    fn two_or_more_days_back_names_the_month_and_day() {
        // 1970-01-01 seen from 1970-01-05.
        assert_eq!(relative_time(0, 4 * 86_400, 0), "Jan 1");
    }

    #[test]
    fn the_utc_offset_can_move_yesterday_across_midnight() {
        // 23:30 UTC on day 0, checked 40 minutes later at +0300: locally
        // that commit was already the *previous* local day by the time this
        // reads "yest.", one full local day plus a few minutes on.
        let day0_2330_utc = 23 * 3600 + 30 * 60;
        let now = day0_2330_utc + 86_400 + 40 * 60;
        assert_eq!(
            relative_time(day0_2330_utc as i64, now as i64, 3 * 3600),
            "yest."
        );
    }

    #[test]
    fn the_fixed_offset_holds_across_what_would_be_a_dst_boundary() {
        // Same instants, two different fixed offsets: the session's offset
        // is read once at startup and never re-resolves, so a DST-like shift
        // changes nothing here — only a different `utc_offset_secs` would.
        let a = relative_time(0, 86_401, 2 * 3600);
        let b = relative_time(0, 86_401, 3 * 3600);
        assert_eq!(a, b);
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        // 2024 is a leap year: day 60 is Feb 29, not Mar 1.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn a_collision_extends_only_the_colliding_pair() {
        let (work, old, elsewhere) = (
            PathBuf::from("/home/eve/work/api"),
            PathBuf::from("/home/eve/old/api"),
            PathBuf::from("/home/eve/dotfiles"),
        );
        let paths = [work.as_path(), old.as_path(), elsewhere.as_path()];

        assert_eq!(labels(&paths), vec!["work/api", "old/api", "dotfiles"]);
    }

    #[test]
    fn a_three_way_collision_extends_until_they_differ() {
        let (a, b, c) = (
            PathBuf::from("/x/a/api"),
            PathBuf::from("/x/b/api"),
            PathBuf::from("/x/c/api"),
        );
        let paths = [a.as_path(), b.as_path(), c.as_path()];

        assert_eq!(labels(&paths), vec!["a/api", "b/api", "c/api"]);
    }

    #[test]
    fn no_collision_stays_at_one_component() {
        let (a, b) = (PathBuf::from("/one/repo"), PathBuf::from("/two/project"));
        let paths = [a.as_path(), b.as_path()];

        assert_eq!(labels(&paths), vec!["repo", "project"]);
    }

    #[test]
    fn an_exhausted_path_keeps_its_full_length() {
        // Both paths are entirely "api" segments repeated: extending can
        // never distinguish them, so both end up at their full length.
        let (a, b) = (PathBuf::from("/api/api"), PathBuf::from("/x/api/api"));
        let paths = [a.as_path(), b.as_path()];

        assert_eq!(labels(&paths), vec!["api/api", "x/api/api"]);
    }

    #[test]
    fn totals_group_by_thousands() {
        assert_eq!(comma(0), "0");
        assert_eq!(comma(210), "210");
        assert_eq!(comma(1_240), "1,240");
        assert_eq!(comma(1_000_000), "1,000,000");
    }

    #[test]
    fn the_scroll_top_pins_to_the_edge_the_selection_crossed() {
        assert_eq!(left_scroll_top(0, 10, 5), 0);
        assert_eq!(left_scroll_top(4, 10, 5), 0);
        assert_eq!(left_scroll_top(5, 10, 5), 1);
        assert_eq!(left_scroll_top(9, 10, 5), 5);
        assert_eq!(left_scroll_top(2, 3, 5), 0); // fewer rows than fit
    }

    fn colours(line: &Line) -> Vec<Option<Color>> {
        line.spans.iter().map(|span| span.style.fg).collect()
    }

    #[test]
    fn an_inactive_row_carries_a_red_bang_before_its_total() {
        let line = left_row("old/thing", Some(210), true, false, Palette::colored());
        assert_eq!(line.to_string(), "  old/thing ! 210xp");
        assert!(colours(&line).contains(&Some(Color::Red)));
    }

    #[test]
    fn a_bad_file_row_carries_the_bang_and_no_total() {
        let line = left_row("abc.json", None, true, false, Palette::plain());
        assert_eq!(line.to_string(), "  abc.json !");
    }

    #[test]
    fn a_healthy_row_has_no_bang_and_the_selection_is_marked() {
        let line = left_row("dotfiles", Some(380), false, true, Palette::colored());
        assert_eq!(line.to_string(), "> dotfiles 380xp");
        assert_eq!(colours(&line)[0], Some(Color::Cyan));
    }

    #[test]
    fn the_plain_palette_paints_no_colour_and_keeps_the_dimming() {
        let line = left_row("old/thing", Some(210), true, true, Palette::plain());
        assert!(colours(&line).iter().all(Option::is_none));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::DIM));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn gains_are_green_only_with_colour_and_the_text_is_the_same() {
        let event = Event {
            sha: "a".into(),
            timestamp: 0,
            subject: "feat".into(),
            added: 3,
            deleted: 1,
            xp: 6,
            rewrite: false,
        };
        let colour = event_stat_line(&event, Palette::colored());
        let plain = event_stat_line(&event, Palette::plain());
        assert_eq!(colour.to_string(), plain.to_string());
        assert_eq!(colours(&colour).last(), Some(&Some(Color::Green)));
        assert!(colours(&plain).iter().all(Option::is_none));
    }

    #[test]
    fn a_rewrite_line_reads_rewritten_rather_than_a_diffstat() {
        let event = Event {
            sha: "a".into(),
            timestamp: 0,
            subject: "fix: off-by-one".into(),
            added: 2,
            deleted: 2,
            xp: 2,
            rewrite: true,
        };
        assert_eq!(
            event_time_line(&event, 0, 0).to_string(),
            "0m ago  fix: off-by-one (redo)"
        );
        assert_eq!(
            event_stat_line(&event, Palette::plain()).to_string(),
            "          rewritten  +2xp"
        );
    }

    #[test]
    fn an_ordinary_commit_line_carries_its_diffstat() {
        let event = Event {
            sha: "a".into(),
            timestamp: 0,
            subject: "feat: parse reflog".into(),
            added: 120,
            deleted: 34,
            xp: 222,
            rewrite: false,
        };
        assert_eq!(
            event_stat_line(&event, Palette::plain()).to_string(),
            "          +120 −34  +222xp"
        );
    }
}
