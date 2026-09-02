use std::path::{Path, PathBuf};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexSearch};
use gpui::{Context, Entity, Subscription, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, Size,
};

use super::view::TerminalView;
use crate::ui::i18n::{L10nKey, t};

const MAX_MATCHES: usize = 10_000;

/// How long a printing pane has to stay quiet before an open search bar
/// rescans it. Short enough that a command's output is re-counted by the time
/// the eye gets back to the bar, long enough that a flood costs one scan per
/// pause rather than one per frame.
pub(super) const SCAN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// How many debounce windows a pane that never stops printing may push the
/// rescan out before it gets one anyway.
///
/// A debounce alone waits for a pause, and a coding agent streaming a reply
/// does not give one for a minute at a time — the count would sit frozen for
/// the whole flood and the highlights would sit on text that has since been
/// rewritten under them. Rebasing keeps a highlight under text that *scrolled*;
/// only a scan catches up with text that changed in place.
pub(super) const SCAN_LAG_LIMIT: u32 = 4;

/// The search bar floats over the top of the grid rather than pushing it down,
/// so these two decide how many rows it hides. Keep them next to the `.top()`
/// and `.h()` that use them — a match parked under the bar is on screen and
/// still unreadable, which is the one thing "next match" must never do.
const BAR_TOP: f32 = 8.;
const BAR_HEIGHT: f32 = 34.;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Url(String),
    File {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
        /// Set when the path is a directory, which is a link to a *place* —
        /// the file tree shows it, the editor cannot.
        is_dir: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LinkMatch {
    pub start: usize,
    pub end: usize,
    pub target: LinkTarget,
}

pub struct SearchState {
    pub input: Entity<InputState>,
    pub matches: Vec<Match>,
    pub current_index: Option<usize>,
    /// Scrollback depth the `matches` lines are counted against. Match points
    /// are grid-relative, so this is what turns one back into the absolute row
    /// it named — see [`TerminalView::refresh_matches_after_output`]. It is the
    /// depth at the last scan until output scrolls the grid, which moves the
    /// lines and this together (see
    /// [`TerminalView::rebase_matches_after_scroll`]) so the anchors hold.
    scanned_history: usize,
    _subs: Vec<Subscription>,
}

impl SearchState {
    pub fn current(&self) -> Option<&Match> {
        self.current_index.and_then(|i| self.matches.get(i))
    }
}

/// One read of the grid: every match in it, whether the pattern compiled, and
/// how deep the scrollback was at the time.
struct Scan {
    matches: Vec<Match>,
    regex_error: bool,
    history: usize,
}

impl Scan {
    fn empty(regex_error: bool) -> Self {
        Self {
            matches: Vec::new(),
            regex_error,
            history: 0,
        }
    }
}

/// A match point's row counted from the top of scrollback, which survives the
/// grid scrolling under it — the same anchor a command mark or a placed image
/// uses, with the same caveat once the scrollback is full and the discard count
/// stops being observable.
fn anchor_row(history: usize, point: &Point) -> i64 {
    history as i64 + point.line.0 as i64
}

impl TerminalView {
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.search.is_none();
        // When the query is seeded from the grid selection, that selection is
        // the thing being searched for — not a casualty of opening the bar.
        // `recompute_matches` clears it because its other callers are the user
        // *changing* the query (typing, toggles), where the old selection no
        // longer names anything; here the query IS the selection, so it is
        // put back after the scan (#584).
        let mut seeded_selection = None;
        if fresh {
            let seed = self.selected_search_seed();
            if seed.is_some() {
                seeded_selection = self.terminal.term.lock().selection.clone();
            }
            let seed = seed.unwrap_or_else(|| self.search_last_query.clone());
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t(L10nKey::SearchFind))
                    .default_value(seed)
            });
            let subs = vec![cx.subscribe_in(&input, window, Self::on_search_event)];
            self.search = Some(SearchState {
                input,
                matches: Vec::new(),
                current_index: None,
                scanned_history: 0,
                _subs: subs,
            });
        }
        if let Some(input) = self.search.as_ref().map(|s| s.input.clone()) {
            input.update(cx, |state, cx| state.focus(window, cx));
            // The box keeps the last query on purpose — reopening on the word
            // you just looked for is most of what a find bar is for — so the
            // caret must not treat it as text to type around.
            crate::ui::prefill::select_all_when_drawn(&input, window, cx);
        }
        if fresh {
            self.recompute_matches(cx);
            if let Some(selection) = seeded_selection {
                self.terminal.term.lock().selection = Some(selection);
            }
        }
        cx.notify();
    }

    fn selected_search_seed(&self) -> Option<String> {
        let text = self.terminal.term.lock().selection_to_string()?;
        let trimmed = text.trim_matches(['\n', '\r']);
        if trimmed.is_empty() || trimmed.contains('\n') {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.search.as_ref() {
            self.search_last_query = s.input.read(cx).value().to_string();
        }
        self.search = None;
        self.search_focused = false;
        self.search_regex_error = false;
        // The grid selection stays: it is the user's, not the bar's. The same
        // discipline `refresh_matches_after_output` states — the search bar
        // opening and closing around a selection must not erase it (#584).
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.recompute_matches(cx),
            InputEvent::PressEnter { shift, .. } => {
                let dir = if *shift {
                    Direction::Left
                } else {
                    Direction::Right
                };
                self.step_match(dir, cx);
            }
            InputEvent::Focus => {
                self.search_focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                self.search_focused = false;
                cx.notify();
            }
        }
    }

    /// Every match of the current query in the whole grid, and whether the
    /// pattern failed to compile.
    ///
    /// A match point is a line *of the grid it was read from*, and output
    /// scrolling the grid makes every one of them name a different line —
    /// which is what [`TerminalView::rebase_matches_after_scroll`] undoes
    /// between scans. Nothing here caches, and both callers re-read the grid.
    fn scan_matches(&self, query: &str) -> Scan {
        let mut matches: Vec<Match> = Vec::new();
        if query.is_empty() {
            return Scan::empty(false);
        }
        let pattern = self.effective_search_pattern(query);
        let Ok(mut regex) = RegexSearch::new(&pattern) else {
            return Scan::empty(true);
        };
        let term = self.terminal.term.lock();
        let grid = term.grid();
        let history = grid.history_size();
        let mut origin = Point::new(grid.topmost_line(), Column(0));

        while matches.len() < MAX_MATCHES {
            let Some(m) = term.search_next(&mut regex, origin, Direction::Right, Side::Left, None)
            else {
                break;
            };
            if matches.last().is_some_and(|last| m.start() <= last.start()) {
                break;
            }
            origin = m.end().add(grid, Boundary::None, 1);
            let wrapped = origin <= *m.end();
            matches.push(m);
            if wrapped {
                break;
            }
        }
        Scan {
            matches,
            regex_error: false,
            history,
        }
    }

    /// The match a fresh query starts on: the last one at or above the bottom
    /// of what is on screen, so Enter walks forward from where the eye is.
    fn match_nearest_the_viewport(&self, matches: &[Match]) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        let term = self.terminal.term.lock();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let bottom = Point::new(
            Line(grid.screen_lines() as i32 - 1 - display_offset),
            grid.last_column(),
        );
        Some(
            matches
                .iter()
                .rposition(|m| *m.start() <= bottom)
                .unwrap_or(0),
        )
    }

    /// Note that output changed the grid an open search bar is describing.
    ///
    /// Two things happen here, because output breaks a scan in two ways.
    ///
    /// The cheap one runs every wakeup: text that scrolled took the highlights
    /// off their rows, and [`Self::rebase_matches_after_scroll`] puts them back
    /// without reading the grid.
    ///
    /// The rescan behind it is debounced rather than run per wakeup: it reads
    /// the whole grid, which is up to `MAX_SCROLLBACK` lines, and a pane
    /// mid-flood is repainting far faster than anyone can read a match count
    /// off it. One task waits for the printing to pause and then rescans once;
    /// further output while it waits pushes the deadline out, up to
    /// [`SCAN_LAG_LIMIT`] windows — a pane that never pauses still gets scanned.
    pub(super) fn note_output_under_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.rebase_matches_after_scroll(cx);
        self.search_scan_epoch = self.search_scan_epoch.wrapping_add(1);
        if self.search_scan_armed {
            return;
        }
        self.search_scan_armed = true;
        cx.spawn(async move |this, cx| {
            let mut waited = 0;
            loop {
                let Ok(epoch) = this.update(cx, |view, _| view.search_scan_epoch) else {
                    break;
                };
                cx.background_executor().timer(SCAN_DEBOUNCE).await;
                waited += 1;
                let settled = this.update(cx, |view, cx| {
                    if view.search_scan_epoch != epoch && waited < SCAN_LAG_LIMIT {
                        return false;
                    }
                    view.search_scan_armed = false;
                    view.refresh_matches_after_output(cx);
                    true
                });
                match settled {
                    Ok(true) | Err(_) => break,
                    Ok(false) => continue,
                }
            }
        })
        .detach();
    }

    /// Slide the stored match points along with the text they name.
    ///
    /// A match point is a grid line, and output moves the text under it: every
    /// line that scrolls off the top of the screen shifts the whole grid up
    /// one. Between two scans that leaves each highlight washing the row its
    /// text has just left — one row off per line scrolled, for as long as the
    /// printing keeps the debounced rescan from firing, which is exactly when
    /// someone is watching the highlight. Absolute rows (see [`anchor_row`]) do
    /// not move, so re-deriving each line from its anchor puts the highlight
    /// back under its text. Nothing here re-reads the grid, so it is cheap
    /// enough for every wakeup.
    ///
    /// Matches carried off the top of the scrollback are dropped. A grid that
    /// *shrank* — a cleared scrollback, a swap to the alt screen — moved every
    /// anchor by an amount this cannot see, so that case rescans instead.
    fn rebase_matches_after_scroll(&mut self, cx: &mut Context<Self>) {
        let history = self.terminal.term.lock().grid().history_size();
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let drift = history as i64 - s.scanned_history as i64;
        if drift == 0 {
            return;
        }
        if drift < 0 {
            self.refresh_matches_after_output(cx);
            return;
        }

        let drift = drift as i32;
        let topmost = -(history as i32);
        let moved = |p: &Point| Point::new(Line(p.line.0 - drift), p.column);
        // The matches are in grid order, so the ones that fell off the top are
        // a prefix — and a match whose start survived kept its end too.
        let gone = s
            .matches
            .partition_point(|m| m.start().line.0 - drift < topmost);
        s.matches.drain(..gone);
        for m in &mut s.matches {
            let (start, end) = (moved(m.start()), moved(m.end()));
            *m = start..=end;
        }
        s.current_index = s.current_index.and_then(|i| i.checked_sub(gone));
        s.scanned_history = history;
    }

    /// Re-run the query against the grid as it now stands, keeping the count
    /// and the highlights honest while the pane is still printing.
    ///
    /// This is the same scan as `recompute_matches`, minus the two things that
    /// only a *user* action has the right to do: it does not drop the
    /// selection, and it does not scroll. Output arriving under an open search
    /// bar must not yank the viewport or erase what the user was selecting.
    pub(super) fn refresh_matches_after_output(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().to_string())
        else {
            return;
        };
        let scan = self.scan_matches(&query);
        // Stay on the match the user stepped to if it survived. Compare where
        // it was by absolute row, not by line: a line is only meaningful with
        // the scrollback depth it was counted against, and matching on the raw
        // number would latch the selection onto whichever occurrence has taken
        // that grid row over. When it is gone, fall back to where a fresh query
        // would land rather than to a stale ordinal.
        let previous = self
            .search
            .as_ref()
            .and_then(|s| Some((*s.current()?.start(), s.scanned_history)))
            .map(|(p, history)| (anchor_row(history, &p), p.column));
        let current_index = previous
            .and_then(|(row, column)| {
                scan.matches.iter().position(|m| {
                    anchor_row(scan.history, m.start()) == row && m.start().column == column
                })
            })
            .or_else(|| self.match_nearest_the_viewport(&scan.matches));

        if let Some(s) = self.search.as_mut() {
            s.matches = scan.matches;
            s.current_index = current_index;
            s.scanned_history = scan.history;
        }
        self.search_regex_error = scan.regex_error;
        cx.notify();
    }

    pub(super) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().to_string())
        else {
            return;
        };

        let scan = self.scan_matches(&query);
        let current_index = self.match_nearest_the_viewport(&scan.matches);

        if let Some(s) = self.search.as_mut() {
            s.matches = scan.matches;
            s.current_index = current_index;
            s.scanned_history = scan.history;
        }
        self.search_regex_error = scan.regex_error;

        let current = self.search.as_ref().and_then(|s| s.current().cloned());
        let hidden = self.rows_behind_the_search_bar();
        let mut term = self.terminal.term.lock();
        term.selection = None;
        if let Some(m) = current {
            scroll_match_into_view(&mut term, &m, hidden);
        }
        drop(term);
        cx.notify();
    }

    pub(super) fn step_match(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let current = {
            let Some(s) = self.search.as_mut() else {
                return;
            };
            if s.matches.is_empty() {
                return;
            }
            let len = s.matches.len();
            let cur = s.current_index.unwrap_or(0);
            let next = match direction {
                Direction::Right => (cur + 1) % len,
                Direction::Left => (cur + len - 1) % len,
            };
            s.current_index = Some(next);
            s.matches[next].clone()
        };
        let hidden = self.rows_behind_the_search_bar();
        scroll_match_into_view(&mut self.terminal.term.lock(), &current, hidden);
        cx.notify();
    }

    fn rows_behind_the_search_bar(&self) -> i32 {
        rows_under_the_bar(self.line_height.as_f32())
    }

    fn toggle_search_case(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.search_case_sensitive = !self.search_case_sensitive;
        self.recompute_matches(cx);
    }

    fn toggle_search_regex(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.search_regex = !self.search_regex;
        self.recompute_matches(cx);
    }

    fn effective_search_pattern(&self, query: &str) -> String {
        let base = if self.search_regex {
            query.to_string()
        } else {
            regex_escape(query)
        };
        if self.search_case_sensitive {
            format!("(?-i){base}")
        } else {
            base
        }
    }

    pub(super) fn render_search_bar(
        &self,
        state: &SearchState,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let popover = theme.popover;
        let accent = theme.accent;
        let danger = theme.red;

        let total = state.matches.len();
        let has_query = !state.input.read(cx).value().is_empty();
        let has_matches = !state.matches.is_empty();
        let focused = self.search_focused;
        let regex_error = self.search_regex_error;
        let case_on = self.search_case_sensitive;
        let regex_on = self.search_regex;

        let field = Input::new(&state.input)
            .appearance(false)
            .with_size(Size::Small);

        let count = has_query.then(|| {
            let current = if has_matches {
                state.current_index.map(|i| i + 1).unwrap_or(0)
            } else {
                0
            };
            // The scan stops at MAX_MATCHES. Without the mark, a query that hit
            // the ceiling reads as if the scrollback held exactly that many.
            let more = match total >= MAX_MATCHES {
                true => "+",
                false => "",
            };
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(format!("{current}/{total}{more}"))
        });

        let case_toggle = Button::new("search-case")
            .label("Aa")
            .ghost()
            .small()
            .selected(case_on)
            .tooltip(t(L10nKey::SearchMatchCase))
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_case(cx);
            }));
        let regex_toggle = Button::new("search-regex")
            .label(".*")
            .ghost()
            .small()
            .selected(regex_on)
            .tooltip(t(L10nKey::SearchUseRegex))
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_regex(cx);
            }));

        let divider = div().flex_none().w(px(1.)).h(px(16.)).bg(border);

        let prev = Button::new("search-prev")
            .icon(IconName::ChevronUp)
            .ghost()
            .small()
            .tooltip(t(L10nKey::AppMenuFindPrevious))
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Left, cx);
            }));
        let next = Button::new("search-next")
            .icon(IconName::ChevronDown)
            .ghost()
            .small()
            .tooltip(t(L10nKey::AppMenuFindNext))
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Right, cx);
            }));
        let close = Button::new("search-close")
            .icon(IconName::Close)
            .ghost()
            .small()
            .tooltip(t(L10nKey::Close))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_search(window, cx);
            }));

        div()
            .absolute()
            .top(px(BAR_TOP))
            .right_4()
            .occlude()
            .flex()
            .items_center()
            .gap_1p5()
            .w(px(400.))
            .h(px(BAR_HEIGHT))
            .pl_3()
            .pr_1()
            .rounded_lg()
            .border_1()
            .border_color(if regex_error {
                danger
            } else if focused {
                accent
            } else {
                border
            })
            .bg(popover)
            .shadow_md()
            .child(div().flex_1().min_w_0().child(field))
            .children(count)
            .child(case_toggle)
            .child(regex_toggle)
            .child(divider)
            .child(prev)
            .child(next)
            .child(close)
    }
}

/// How many grid rows the floating bar covers, at this line height.
fn rows_under_the_bar(line_height: f32) -> i32 {
    if line_height <= 0. {
        return 0;
    }
    ((BAR_TOP + BAR_HEIGHT) / line_height).ceil() as i32
}

#[derive(Debug, PartialEq, Eq)]
enum Reveal {
    /// Already clear of the bar and inside the viewport.
    Stay,
    /// Below the viewport — alacritty's own minimal scroll lands it on the
    /// last row, which nothing covers.
    ToPoint,
    /// Above the readable area. `scroll_to_point` would park it on row 0, the
    /// row most likely to be behind the bar, so walk back this many lines and
    /// land it just clear instead.
    Back(i32),
}

fn reveal(line: i32, display_offset: i32, screen_lines: i32, hidden: i32) -> Reveal {
    let top = -display_offset + hidden;
    let bottom = screen_lines - 1 - display_offset;
    if line > bottom {
        Reveal::ToPoint
    } else if line < top {
        Reveal::Back(top - line)
    } else {
        Reveal::Stay
    }
}

fn scroll_match_into_view<T: EventListener>(term: &mut Term<T>, m: &Match, hidden: i32) {
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;
    let screen_lines = grid.screen_lines() as i32;
    match reveal(m.start().line.0, display_offset, screen_lines, hidden) {
        Reveal::Stay => {}
        Reveal::ToPoint => term.scroll_to_point(*m.start()),
        Reveal::Back(lines) => term.scroll_display(Scroll::Delta(lines)),
    }
}

fn regex_escape(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
pub(super) fn url_at(text: &str, col: usize) -> Option<String> {
    url_span_at(text, col).map(|(_, _, url)| url)
}

/// What checking one candidate path came back with.
///
/// The third arm is what makes remote panes work at all. A local pane answers
/// out of the filesystem in microseconds, but a pane whose paths live on
/// another machine has to ask that machine, and the answer arrives frames
/// later — long after the mouse event that wanted it. [`Probe::Unknown`] is
/// that gap: it means "nobody has asked yet", and the prober is expected to go
/// ask so the *next* look finds a real answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Probe {
    /// Something is there. Whether it is a directory travels with the answer:
    /// a directory link belongs in the file tree, not in the editor.
    Hit {
        is_dir: bool,
    },
    Miss,
    Unknown,
}

/// Answers for paths on the machine tty7 itself is running on.
pub(super) fn local_probe(path: &Path, require_file: bool) -> Probe {
    if path.is_file() {
        return Probe::Hit { is_dir: false };
    }
    match !require_file && path.is_dir() {
        true => Probe::Hit { is_dir: true },
        false => Probe::Miss,
    }
}

/// Where a relative path printed by a pane is measured from.
///
/// `local_home` is the part that is easy to miss. `~` has to become a real
/// directory before anything can be looked up, and the only clue a pane
/// usually offers is its own cwd; when that does not reveal a home, this
/// machine's `$HOME` is the last resort. Sound for a pane on this machine, and
/// a fabrication for one whose paths live elsewhere — `/Users/me/.zshrc` is
/// not what `~/.zshrc` means on a Linux box, and asking that box about it is
/// at best a miss and at worst somebody else's file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LinkRoots {
    /// Directories a relative path is tried against, nearest first.
    pub dirs: Vec<PathBuf>,
    /// Whether this machine's `$HOME` may stand in for a `~` the roots cannot
    /// explain.
    pub local_home: bool,
}

impl LinkRoots {
    /// Roots on the machine tty7 is running on, where `$HOME` means what it
    /// says.
    pub fn local(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            local_home: true,
        }
    }

    /// The directory the pane is in — the first root, and the one a `~` is
    /// read out of.
    pub fn cwd(&self) -> Option<&Path> {
        self.dirs.first().map(PathBuf::as_path)
    }
}

/// What a lookup found: the link, or — when nothing answered — the reading of
/// the token worth naming in a report.
///
/// The two travel together because the readings are computed once. A caller
/// that asked and got nothing already holds everything needed to say what the
/// click *said*; going back to the text for it would re-tokenise the whole
/// line on a path that runs on every hover.
pub(super) struct LinkLookup {
    pub link: Option<LinkMatch>,
    /// Only ever set when `link` is `None`. See [`report_candidate`].
    pub candidate: Option<FileCandidate>,
}

/// The link alone, for callers with nothing to report when there is none.
#[cfg(test)]
pub(super) fn link_at(
    text: &str,
    col: usize,
    roots: &LinkRoots,
    include_files: bool,
    probe: &mut dyn FnMut(&Path, bool) -> Probe,
) -> Option<LinkMatch> {
    link_lookup_at(text, col, roots, include_files, probe).link
}

pub(super) fn link_lookup_at(
    text: &str,
    col: usize,
    roots: &LinkRoots,
    include_files: bool,
    probe: &mut dyn FnMut(&Path, bool) -> Probe,
) -> LinkLookup {
    if let Some((start, end, url)) = url_span_at(text, col) {
        return LinkLookup {
            link: Some(LinkMatch {
                start,
                end,
                target: LinkTarget::Url(url),
            }),
            candidate: None,
        };
    }
    if !include_files {
        return LinkLookup {
            link: None,
            candidate: None,
        };
    }
    // Every reading of the token gets its own look and the first one something
    // answers for wins. The winner's span is what gets underlined, so on a
    // Chinese line the underline lands on the path alone rather than on the
    // prose that happened to touch it.
    let candidates = file_candidates_at(text, col);
    for candidate in &candidates {
        let Some((path, is_dir)) = resolve_candidate(candidate, roots, probe) else {
            continue;
        };
        return LinkLookup {
            link: Some(LinkMatch {
                start: candidate.start,
                end: candidate.end,
                target: LinkTarget::File {
                    path,
                    line: candidate.line,
                    column: candidate.column,
                    is_dir,
                },
            }),
            candidate: None,
        };
    }
    LinkLookup {
        link: None,
        candidate: report_candidate(candidates),
    }
}

fn report_candidate(candidates: Vec<FileCandidate>) -> Option<FileCandidate> {
    let narrowest = candidates
        .iter()
        .filter(|candidate| candidate.looks_like_a_path())
        .min_by_key(|candidate| candidate.end - candidate.start)
        .cloned();
    narrowest.or_else(|| candidates.into_iter().next())
}

/// The first of `candidate`'s possible paths that something answers for.
///
/// Roots are tried in order and the first [`Probe::Hit`] wins, which is what
/// makes the pane's own directory beat the repository root when both hold a
/// file by that name. A [`Probe::Unknown`] does not stop the walk — a later
/// root may already be cached — it just leaves the token unresolved for now.
pub(super) fn resolve_candidate(
    candidate: &FileCandidate,
    roots: &LinkRoots,
    probe: &mut dyn FnMut(&Path, bool) -> Probe,
) -> Option<(PathBuf, bool)> {
    let require_file = candidate.require_file();
    candidate
        .paths(roots)
        .into_iter()
        .find_map(|path| match probe(&path, require_file) {
            Probe::Hit { is_dir } => Some((path, is_dir)),
            Probe::Miss | Probe::Unknown => None,
        })
}

/// A path-shaped token lifted out of the grid, before anything has checked
/// whether it points at something that exists. Splitting this out from the
/// lookup is what lets one pane answer from the filesystem and another answer
/// from a cache filled by a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileCandidate {
    /// Column of the token's first character in the logical line.
    pub start: usize,
    /// Column of its last, inclusive.
    pub end: usize,
    /// The path exactly as it was written — relative, `~`-prefixed, absolute.
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl FileCandidate {
    /// A line number is a claim about a file's *contents*, so a directory that
    /// happens to carry the name cannot answer for it. Without this,
    /// `localhost:8080` resolves the moment a `localhost/` directory exists.
    pub fn require_file(&self) -> bool {
        self.line.is_some()
    }

    /// Every path this token could mean, best guess first: absolute paths
    /// stand alone, relative ones are joined onto each root in turn.
    pub fn paths(&self, roots: &LinkRoots) -> Vec<PathBuf> {
        let Some(expanded) = expand_home(&self.path, roots.cwd(), roots.local_home) else {
            return Vec::new();
        };
        if expanded.as_os_str().is_empty() {
            return Vec::new();
        }
        if expanded.is_absolute() {
            return vec![expanded];
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for root in &roots.dirs {
            let joined = root.join(&expanded);
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
        out
    }

    /// Whether the token says for itself where it starts, rather than being
    /// measured from anywhere. [`Self::paths`] ignores the roots entirely for
    /// these, so a report about one must not name a directory as the place it
    /// was looked for.
    pub fn is_rooted(&self) -> bool {
        self.path.starts_with('~') || Path::new(&self.path).is_absolute()
    }

    /// Whether the token is written enough like a path to be worth telling the
    /// user about when nothing answers for it. A bare word is not — every
    /// modifier-click on ordinary output would raise a notification saying so.
    pub fn looks_like_a_path(&self) -> bool {
        is_path_shaped(&self.path)
    }
}

/// Whether a run of text is written like a path rather than like a word: it
/// says where it starts, or it names a directory on the way.
fn is_path_shaped(token: &str) -> bool {
    token.starts_with('~') || token.contains('/') || (cfg!(windows) && token.contains('\\'))
}

/// Every reading of the token under `col`, most conservative first.
///
/// Two things widen a token into several readings. Where it *ends* is a
/// question only on a CJK line, because prose written with no spaces in it
/// leaves the path nothing to stop at — [`TokenEdge::CjkPunct`] comes first
/// because it keeps `docs/设计文档.md` whole, and [`TokenEdge::Cjk`] follows to
/// rescue the lines where nothing but a han character separates the prose from
/// the path. Where it *starts* is a question on any line, and
/// [`push_readings`] answers it.
///
/// A token with no CJK in it is tokenised once rather than three times: the
/// three edge sets agree character for character on such a token, so the other
/// two scans can only repeat the first.
pub(super) fn file_candidates_at(text: &str, col: usize) -> Vec<FileCandidate> {
    let Some((start, end, widest)) = token_at(text, col, TokenEdge::Whitespace) else {
        return Vec::new();
    };
    let mut out: Vec<FileCandidate> = Vec::new();
    // The fast path, and the one nearly every hover takes: a token with no CJK
    // character anywhere in it reads the same under all three edge sets, so
    // the other two scans can only repeat this one. The CJK arms of
    // [`TokenEdge::ends_token`] match nothing inside such a token, and the
    // half-width rule needs CJK on the far side of the mark — which, at this
    // token's own edges, is the whitespace that ended it.
    if !widest.chars().any(super::smart_select::is_cjk) {
        push_readings(&mut out, start, end, &widest, col);
        return out;
    }

    for edge in [TokenEdge::CjkPunct, TokenEdge::Cjk] {
        let Some((start, end, token)) = token_at(text, col, edge) else {
            continue;
        };
        push_readings(&mut out, start, end, &token, col);
    }
    push_readings(&mut out, start, end, &widest, col);
    out
}

/// The token as it stands, then the same token read again from after each
/// prefix something glued onto the path.
///
/// The whole token goes first and the anchored readings are the fallback,
/// because `=`, `(` and the rest of [`is_prefix_boundary`] are all legal in a
/// Unix filename: a file really called `my=notes.txt` has to answer for itself
/// before anything reads it as an assignment. An anchored reading that lands
/// on a span already here costs nothing — an emitter that closed the call it
/// opened has been unwrapped by [`trim_file_token`] already, and
/// [`push_candidate`] drops the duplicate.
fn push_readings(out: &mut Vec<FileCandidate>, start: usize, end: usize, token: &str, col: usize) {
    push_candidate(out, file_candidate_from(start, end, token.to_string(), col));
    for (offset, rest) in prefix_anchors(token) {
        let start = start + token[..offset].chars().count();
        push_candidate(out, file_candidate_from(start, end, rest.to_string(), col));
    }
}

/// Where inside `token` a path starts over, as a byte offset and the rest of
/// the token from there.
///
/// [`trim_file_token`] walks wrappers off the *front*, which is enough for
/// `(src/main.rs)` and useless for `Bash(D=/tmp/x.md`: the shell assignment
/// and the call that never closes on this line each leave a *name* in front of
/// the path, and no amount of front-trimming reaches past a name. A coding
/// agent echoing the command it is about to run prints exactly that shape, on
/// every tool call it makes.
///
/// Two things hold the readings down to the ones worth a probe. The rest has
/// to be path-shaped, which is what keeps `foo(bar)` and `src/foo(bar.txt` —
/// filenames that are legally called that — from being quietly re-read as
/// `bar`. And an anchor whose rest opens at another boundary is skipped, so
/// `Bash(D=/tmp/x` yields the one reading that matters rather than one per
/// boundary crossed.
fn prefix_anchors(token: &str) -> impl Iterator<Item = (usize, &str)> {
    token.char_indices().filter_map(move |(i, c)| {
        if !is_prefix_boundary(c) {
            return None;
        }
        let offset = i + c.len_utf8();
        let rest = &token[offset..];
        let starts_over = rest.chars().next().is_some_and(|c| !is_prefix_boundary(c));
        (starts_over && is_path_shaped(rest)).then_some((offset, rest))
    })
}

/// The characters that glue a prefix onto a path rather than standing inside
/// one: a shell assignment (`D=/tmp/x`), a flag (`--config=/etc/app.conf`), a
/// wrapper the emitter opens and does not close on this line (`Bash(/tmp/x`),
/// and the marks a list of paths is separated with.
///
/// Two neighbours are deliberately absent. `|` and `&&` are followed by a
/// *command*, not by a path, so an anchor there buys nothing and costs a probe.
/// And `@` would read `user@host:/tmp/x` — a path on another machine — out of
/// this filesystem, which is how a click opens somebody else's file; the same
/// judgement [`home_dir`] makes about `~`.
fn is_prefix_boundary(c: char) -> bool {
    matches!(c, '=' | '(' | '[' | '{' | '<' | ',' | ';' | '`')
}

/// Adds a reading unless one covering the same span is already there.
fn push_candidate(out: &mut Vec<FileCandidate>, candidate: Option<FileCandidate>) {
    let Some(candidate) = candidate else {
        return;
    };
    if out
        .iter()
        .any(|seen| seen.start == candidate.start && seen.end == candidate.end)
    {
        return;
    }
    out.push(candidate);
}

/// The first reading of the token under `col`, for callers that only need to
/// say what the click *said* rather than resolve it.
#[cfg(test)]
pub(super) fn file_candidate_at(text: &str, col: usize) -> Option<FileCandidate> {
    file_candidates_at(text, col).into_iter().next()
}

/// The syntactic half of file detection: everything that can be decided from
/// the token alone, with no filesystem behind it.
fn file_candidate_from(
    start: usize,
    end: usize,
    token: String,
    col: usize,
) -> Option<FileCandidate> {
    let (start, mut end, mut token) = trim_file_token(start, end, token);
    if token.is_empty() {
        return None;
    }

    let mut location = split_file_location(&token);
    if location.line.is_none() && token.ends_with(':') {
        token.pop();
        end = end.saturating_sub(1);
        location = split_file_location(&token);
    }
    if location.path.is_empty() {
        return None;
    }
    (start..=end).contains(&col).then_some(FileCandidate {
        start,
        end,
        path: location.path,
        line: location.line,
        column: location.column,
    })
}

pub(super) fn url_span_at(text: &str, col: usize) -> Option<(usize, usize, String)> {
    let (mut start, _, mut token) = token_at(text, col, TokenEdge::CjkPunct)?;
    trim_trailing_punct(&mut token);

    const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "ftp://"];
    if let Some(off) = SCHEMES.iter().filter_map(|s| token.find(s)).min() {
        start += token[..off].chars().count();
        token.drain(..off);
        if let Some(bad) = token.find(|c| !is_url_char(c)) {
            token.truncate(bad);
        }
        truncate_at_unbalanced_close(&mut token);
        trim_trailing_punct(&mut token);
        let end = start + token.chars().count() - 1;
        return (start..=end).contains(&col).then_some((start, end, token));
    }

    while token
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '(' | '[' | '<' | '\'' | '"' | '{'))
    {
        token.remove(0);
        start += 1;
    }
    trim_trailing_punct(&mut token);
    if token.starts_with("www.") && token.contains('.') {
        let end = start + token.chars().count() - 1;
        (start..=end)
            .contains(&col)
            .then(|| (start, end, format!("https://{token}")))
    } else {
        None
    }
}

/// How far a token reaches before something ends it.
///
/// CJK prose puts no spaces between words, so a path printed inside a Chinese
/// or Japanese sentence has no whitespace to stop at — `文档：docs/a.md，未提交`
/// is a single whitespace-delimited token and the path inside it is
/// unreachable. Widening the boundary set is what pulls the path back out.
///
/// The two CJK levels are kept apart because they disagree about a path that
/// *contains* CJK: `docs/设计文档.md` survives [`Self::CjkPunct`] whole and is
/// cut short by [`Self::Cjk`], while `修改了src/main.rs和docs/b.md` needs
/// [`Self::Cjk`] because no punctuation separates the prose from the path.
/// Neither is right for every line, which is why [`file_candidates_at`] tries
/// them in turn rather than choosing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenEdge {
    /// Whitespace alone — what every line without CJK in it wants.
    Whitespace,
    /// Whitespace and CJK punctuation. Keeps CJK filenames whole.
    ///
    /// "Punctuation" here also covers the half-width marks a CJK line leans
    /// against a path, but only there — see [`ends_token_at`].
    CjkPunct,
    /// Whitespace and any CJK character, for prose written flush against a
    /// path with no punctuation in between.
    Cjk,
}

impl TokenEdge {
    fn ends_token(self, c: char) -> bool {
        c.is_whitespace()
            || match self {
                Self::Whitespace => false,
                Self::CjkPunct => is_cjk_punct(c),
                Self::Cjk => super::smart_select::is_cjk(c),
            }
    }
}

/// CJK punctuation — the full-width comma, colon, brackets and ideographic
/// space, which sit *between* words rather than inside them.
///
/// Han, kana, hangul and the full-width letters and digits are all
/// alphanumeric, so this tells the marks apart from the writing without a
/// hand-maintained code point table.
fn is_cjk_punct(c: char) -> bool {
    super::smart_select::is_cjk(c) && !c.is_alphanumeric()
}

/// Whether `c` ends the token, given `outward` — the character on its far
/// side, away from the token being grown.
///
/// The lookout is what lets a half-width mark separate CJK prose from a path:
/// `文档:src/main.rs` and `见:www.example.com,谢谢` are written with ASCII
/// punctuation just as often as with the full-width kind, and neither offers
/// any other boundary. Deciding on the mark alone is not open to us — `:` also
/// carries the line suffix in `src/main.rs:12` and the scheme in `https://` —
/// so the mark only ends a token when the writing it leans against is CJK.
fn ends_token_at(c: char, outward: Option<char>, edge: TokenEdge) -> bool {
    if edge.ends_token(c) {
        return true;
    }
    if edge == TokenEdge::Whitespace || !is_ascii_prose_punct(c) {
        return false;
    }
    outward.is_some_and(super::smart_select::is_cjk)
}

/// The half-width marks CJK prose borrows from ASCII, kept to the ones that
/// only ever stand *between* words. `/`, `.`, `-` and `_` are absent because
/// they live inside paths; `:` is here only on the strength of the CJK lookout
/// in [`ends_token_at`].
fn is_ascii_prose_punct(c: char) -> bool {
    matches!(c, ':' | ',' | ';' | '!' | '?')
}

fn token_at(text: &str, col: usize, edge: TokenEdge) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() || edge.ends_token(chars[col]) {
        return None;
    }

    let mut start = col;
    while start > 0
        && !ends_token_at(
            chars[start - 1],
            start.checked_sub(2).map(|i| chars[i]),
            edge,
        )
    {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !ends_token_at(chars[end + 1], chars.get(end + 2).copied(), edge)
    {
        end += 1;
    }
    Some((start, end, chars[start..=end].iter().collect()))
}

fn trim_file_token(mut start: usize, mut end: usize, mut token: String) -> (usize, usize, String) {
    while token
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '(' | '[' | '<' | '\'' | '"' | '{' | '`'))
    {
        token.remove(0);
        start += 1;
    }
    strip_trailing_punct(&mut token, &mut end);
    // `Update(src/main.rs)` — an emitter that writes the path inside a call.
    // The strip above only walks characters off the *front*, so a name in
    // front of the paren leaves the token starting at `U` and the path
    // unreachable however far the front is trimmed.
    //
    // What tells a call from a filename is the shape of the whole token rather
    // than the paren alone, because `foo(bar)`, `my(file).txt` and
    // `src/foo(bar)` are all legal names on Unix and none of them is a call.
    // Three things have to hold at once: the token ends at the `)` this `(`
    // opened, what stands in front of the paren reads like a name rather than
    // a path, and what stands inside it reads like a path. The last of those
    // costs `Update(main.rs)` — a call naming a file in the pane's own
    // directory — and is worth it: without it, a file that is really called
    // `foo(bar)` gets silently rewritten to `bar`, which is how a click ends
    // up opening a different file rather than none.
    if let Some(open) = call_wrapper_open(&token) {
        token.pop();
        end = end.saturating_sub(1);
        start += token[..open].chars().count() + 1;
        token.drain(..=open);
        strip_trailing_punct(&mut token, &mut end);
    }
    (start, end, token)
}

/// Walks trailing punctuation off the token.
///
/// Runs before the call-wrapper rule as well as after it, because that rule
/// reads the token's *final* character and a sentence puts its own punctuation
/// there first: `Update(src/main.rs).` has to lose the full stop before the
/// `)` underneath it can be recognised as the wrapper's. A closing bracket the
/// token opened for itself survives either pass, which is what keeps
/// `src/foo(bar).` a filename with a full stop after it.
fn strip_trailing_punct(token: &mut String, end: &mut usize) {
    while let Some(c) = token.chars().next_back() {
        if !is_file_trailing_punct(c) || closes_own_pair(token, c) {
            return;
        }
        token.pop();
        *end = end.saturating_sub(1);
    }
}

/// The `(` a call wrapper opens with, as a byte index into `token`.
///
/// See [`trim_file_token`] for what has to hold before a paren counts as a
/// wrapper rather than as part of a name.
fn call_wrapper_open(token: &str) -> Option<usize> {
    let close = token.strip_suffix(')').map(str::len)?;
    let open = matching_open_paren(token, close)?;
    let name = &token[..open];
    if name.is_empty() || name.contains(['/', '\\', '(', ')']) {
        return None;
    }
    is_path_shaped(&token[open + 1..close]).then_some(open)
}

/// The `(` that the `)` at byte index `close` closes.
fn matching_open_paren(token: &str, close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in token[..close].char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => return Some(i),
            '(' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether a trailing bracket is the closing half of a pair the token opened
/// for itself.
///
/// `src/foo(bar)` and `notes[1]` are names; `src/foo.txt)` is a name somebody
/// wrote inside brackets. Counting is enough to tell them apart — the same
/// judgement [`trim_trailing_punct`] makes for a URL.
fn closes_own_pair(token: &str, c: char) -> bool {
    let open = match c {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => return false,
    };
    count_char(token, c) <= count_char(token, open)
}

fn is_file_trailing_punct(c: char) -> bool {
    matches!(
        c,
        ')' | ']'
            | '}'
            | '.'
            | ','
            | ';'
            | '\''
            | '"'
            | '>'
            | '`'
            | '）'
            | '］'
            | '】'
            | '》'
            | '」'
            | '。'
            | '，'
            | '；'
    )
}

struct FileLocation {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

fn split_file_location(token: &str) -> FileLocation {
    let Some((prefix, last)) = strip_numeric_suffix(token) else {
        return FileLocation {
            path: token.to_string(),
            line: None,
            column: None,
        };
    };
    if let Some((path, line)) = strip_numeric_suffix(prefix) {
        FileLocation {
            path: path.to_string(),
            line: Some(line),
            column: Some(last),
        }
    } else {
        FileLocation {
            path: prefix.to_string(),
            line: Some(last),
            column: None,
        }
    }
}

/// The `:line` a path carries, as the token without it and the number.
///
/// A range counts as the line it starts on: `remote.rs:1215-1247` is how a
/// review note or a coding agent names a span, and the file it points at is
/// the same one a bare `:1215` would.
///
/// What keeps a date out is that a span ends at or after it starts.
/// `2026-09-02` fails on its second hyphen and `2026-09` on `9 < 2026`, while
/// `12-30` and `1215-1247` pass. A token that reads as both — `log:2024-2025`
/// — is taken as the range; nothing in the text settles that one, and `log`
/// has to exist as a file before any of it matters.
fn strip_numeric_suffix(token: &str) -> Option<(&str, u32)> {
    let (prefix, suffix) = token.rsplit_once(':')?;
    if prefix.is_empty() {
        return None;
    }
    let (first, last) = match suffix.split_once('-') {
        Some((first, last)) => (first, Some(last)),
        None => (suffix, None),
    };
    let line = parse_line_number(first)?;
    if let Some(last) = last {
        if parse_line_number(last)? < line {
            return None;
        }
    }
    Some((prefix, line))
}

/// A non-empty run of ASCII digits, parsed. The digits are checked before the
/// parse rather than left to it: `u32::from_str` would also take a leading
/// `+`, which is not how anybody writes a line number.
fn parse_line_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn expand_home(path: &str, cwd: Option<&Path>, local_home: bool) -> Option<PathBuf> {
    if path == "~" {
        return home_dir(cwd, local_home);
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir(cwd, local_home).map(|home| home.join(rest));
    }
    Some(PathBuf::from(path))
}

/// The home `~` stands for, read out of the cwd where it can be and out of the
/// environment where it cannot.
///
/// The environment half only applies to a pane on this machine. Anywhere else
/// it would be guessing with our own answer: a cwd of `/srv/app` on a Linux
/// box says nothing about that box's home, and turning `~/.zshrc` into
/// `/Users/me/.zshrc` and asking the far side about it is how a link ends up
/// pointing at a file nobody meant.
fn home_dir(cwd: Option<&Path>, local_home: bool) -> Option<PathBuf> {
    if let Some(home) = cwd.and_then(home_from_cwd) {
        return Some(home);
    }
    if !local_home {
        return None;
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn home_from_cwd(cwd: &Path) -> Option<PathBuf> {
    let mut components = cwd.components();
    let root = components.next()?;
    let base = components.next()?;
    let user = components.next()?;
    let base = base.as_os_str().to_str()?;
    matches!(base, "Users" | "home").then(|| {
        let mut home = PathBuf::new();
        home.push(root.as_os_str());
        home.push(base);
        home.push(user.as_os_str());
        home
    })
}

#[cfg(not(unix))]
fn home_from_cwd(_cwd: &Path) -> Option<PathBuf> {
    None
}

fn trim_trailing_punct(token: &mut String) {
    loop {
        let strip = match token.chars().next_back() {
            Some(')') => count_char(token, ')') > count_char(token, '('),
            Some(']') => count_char(token, ']') > count_char(token, '['),
            Some(
                '.' | ',' | ';' | ':' | '\'' | '"' | '>' | '）' | '］' | '】' | '》' | '」' | '。'
                | '，' | '；' | '：',
            ) => true,
            _ => false,
        };
        if !strip {
            return;
        }
        token.pop();
    }
}

fn count_char(s: &str, needle: char) -> usize {
    s.chars().filter(|&c| c == needle).count()
}

fn truncate_at_unbalanced_close(token: &mut String) {
    let mut parens = 0usize;
    let mut brackets = 0usize;
    for (i, c) in token.char_indices() {
        match c {
            '(' => parens += 1,
            '[' => brackets += 1,
            ')' if parens == 0 => {
                token.truncate(i);
                return;
            }
            ']' if brackets == 0 => {
                token.truncate(i);
                return;
            }
            ')' => parens -= 1,
            ']' => brackets -= 1,
            _ => {}
        }
    }
}

pub(super) fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capped_match_count_says_it_is_capped() {
        // Same rule the count uses. Below the ceiling the number is the truth;
        // at the ceiling it is a floor, and has to read like one.
        let mark = |total: usize| match total >= MAX_MATCHES {
            true => "+",
            false => "",
        };
        assert_eq!(mark(0), "");
        assert_eq!(mark(MAX_MATCHES - 1), "");
        assert_eq!(mark(MAX_MATCHES), "+");
    }

    #[test]
    fn the_bar_hides_whole_rows_however_tall_they_are() {
        // 8px inset + 34px tall, so at the default 17px line height the bar
        // sits over rows 0, 1 and part of 2 — all three count as hidden.
        assert_eq!(rows_under_the_bar(17.), 3);
        assert_eq!(rows_under_the_bar(42.), 1);
        assert_eq!(rows_under_the_bar(43.), 1);
        assert_eq!(rows_under_the_bar(10.), 5);
        assert_eq!(rows_under_the_bar(0.), 0, "a zero height must not divide");
    }

    #[test]
    fn a_match_under_the_bar_counts_as_off_screen() {
        // 24 rows on screen, sitting at the live end, bar over the first 3.
        let at = |line| reveal(line, 0, 24, 3);
        assert_eq!(at(0), Reveal::Back(3), "row 0 is fully covered");
        assert_eq!(at(2), Reveal::Back(1), "row 2 is partly covered");
        assert_eq!(at(3), Reveal::Stay, "row 3 is the first readable one");
        assert_eq!(at(23), Reveal::Stay, "the last row is still on screen");
        assert_eq!(at(24), Reveal::ToPoint, "one past the end is not");
    }

    #[test]
    fn scrolled_back_into_history_the_readable_band_moves_with_it() {
        // 10 lines of history above the viewport: rows now run -10..=13.
        let at = |line| reveal(line, 10, 24, 3);
        assert_eq!(at(-10), Reveal::Back(3), "the top row is behind the bar");
        assert_eq!(at(-7), Reveal::Stay);
        assert_eq!(at(13), Reveal::Stay);
        assert_eq!(at(14), Reveal::ToPoint);
    }

    #[test]
    fn without_a_bar_nothing_on_screen_is_moved() {
        assert_eq!(reveal(0, 0, 24, 0), Reveal::Stay);
        assert_eq!(reveal(-1, 0, 24, 0), Reveal::Back(1))
    }

    #[test]
    fn regex_escape_neutralizes_metacharacters() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("foo(bar)"), r"foo\(bar\)");
        assert_eq!(regex_escape("1+1=2"), r"1\+1=2");
        assert_eq!(regex_escape("hello"), "hello");
    }

    #[test]
    fn url_at_detects_http_and_strips_trailing_punct() {
        let line = "go https://example.com, now";
        assert_eq!(url_at(line, 6).as_deref(), Some("https://example.com"));
    }

    #[test]
    fn url_at_promotes_bare_www_and_ignores_plain_words() {
        assert_eq!(
            url_at("visit www.rust-lang.org now", 8).as_deref(),
            Some("https://www.rust-lang.org")
        );
        assert_eq!(url_at("just a word", 6), None);
        assert_eq!(url_at("word ", 4), None);
        assert_eq!(url_at("word", 99), None);
    }

    #[test]
    fn url_span_at_reports_inclusive_columns_without_trailing_punct() {
        let line = "go https://example.com, now";
        assert_eq!(
            url_span_at(line, 10),
            Some((3, 21, "https://example.com".to_string()))
        );
        assert_eq!(&line[3..=21], "https://example.com");
    }

    #[test]
    fn url_span_at_accepts_file_and_ftp_schemes() {
        assert_eq!(
            url_at("open file:///etc/hosts here", 5).as_deref(),
            Some("file:///etc/hosts")
        );
        assert_eq!(
            url_at("get ftp://host/pub done", 4).as_deref(),
            Some("ftp://host/pub")
        );
    }

    #[test]
    fn url_span_at_strips_various_trailing_punctuation() {
        assert_eq!(
            url_at("open https://a.com] done", 7).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("open https://a.com> done", 7).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("open https://a.com';: done", 7).as_deref(),
            Some("https://a.com")
        );
    }

    #[test]
    fn url_span_at_strips_leading_wrappers() {
        assert_eq!(
            url_at("see (https://a.com) ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("see [https://a.com] ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("see <https://a.com> ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("say \"https://a.com\" ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("(www.rust-lang.org)", 5).as_deref(),
            Some("https://www.rust-lang.org")
        );
    }

    #[test]
    fn url_span_at_reports_trimmed_span_after_stripping_both_ends() {
        let line = "log [https://a.com] end";
        let (start, end, url) = url_span_at(line, 8).expect("URL inside the brackets");
        assert_eq!(url, "https://a.com");
        assert_eq!(&line[start..=end], "https://a.com");
        assert_eq!(&line[start - 1..start], "[");
        assert_eq!(&line[end + 1..end + 2], "]");
    }

    #[test]
    fn url_at_detects_url_glued_to_cjk_prefix() {
        let url = "https://github.com/acme/app/pull/42";
        let line = format!("已创建：{url}");
        let scheme_col = 4;
        assert_eq!(url_at(&line, scheme_col).as_deref(), Some(url));
        assert_eq!(url_at(&line, 12).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, scheme_col).expect("URL after prefix");
        assert_eq!(start, scheme_col);
        assert_eq!(got, url);
        assert_eq!(end, line.chars().count() - 1);

        let row = format!("PR 已创建:{url} 🎉收尾:删除临时");
        let h = row.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&row, h).as_deref(), Some(url));
        assert_eq!(url_at(&row, 0), None);
    }

    #[test]
    fn url_at_ignores_hover_on_cjk_prefix_before_url() {
        let line = "已创建：https://a.com";
        assert_eq!(url_at(line, 0), None);
        assert_eq!(url_at(line, 3), None);
        assert_eq!(url_at(line, 4).as_deref(), Some("https://a.com"));
    }

    #[test]
    fn url_at_strips_full_width_trailing_punctuation() {
        assert_eq!(
            url_at("见（https://a.com）", 3).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("详见 https://a.com。", 5).as_deref(),
            Some("https://a.com")
        );
    }

    #[test]
    fn url_at_stops_at_full_width_open_bracket_glued_after_url() {
        let url = "https://github.com/acme/app/pull/343";
        let line = format!("PR 已创建：{url}（fix/cache-write-tokens → dev）");
        let h = line.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&line, h).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, h + 10).expect("URL before bracket");
        assert_eq!(got, url);
        assert_eq!(start, h);
        assert_eq!(line.chars().nth(end + 1), Some('（'));
        let f = line.chars().position(|c| c == 'f').expect("`fix` start");
        assert_eq!(url_at(&line, f), None);
    }

    #[test]
    fn url_at_keeps_ascii_parens_inside_a_url() {
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)/history";
        assert_eq!(url_at(url, 40).as_deref(), Some(url));
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(url_at(url, 40).as_deref(), Some(url));
        let line = format!("see ({url}) ok");
        assert_eq!(url_at(&line, 8).as_deref(), Some(url));
        let url = "http://[::1]:8080/status";
        let line = format!("probe [{url}] done");
        assert_eq!(url_at(&line, 10).as_deref(), Some(url));
    }

    #[test]
    fn url_at_stops_at_unbalanced_close_paren_glued_after_url() {
        let url = "https://github.com/l0ng-ai/tty7/pull/43";
        let line = format!("PR 已开:#43 ({url})(Fixes #42),分支 fix-x。");
        let h = line.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&line, h).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, h + 10).expect("URL inside parens");
        assert_eq!(got, url);
        assert_eq!(line.chars().nth(start - 1), Some('('));
        assert_eq!(line.chars().nth(end + 1), Some(')'));
        let f = line.chars().position(|c| c == 'F').expect("`Fixes` start");
        assert_eq!(url_at(&line, f), None);
        assert_eq!(
            url_at("read https://a.com/x]next now", 8).as_deref(),
            Some("https://a.com/x")
        );
    }

    #[test]
    fn url_span_at_rejects_www_without_a_dot_and_empty_tokens() {
        assert_eq!(url_at("www near text", 1), None);
        assert_eq!(url_at("...", 1), None);
        assert_eq!(url_at("httpsomething", 3), None);
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-link-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temporary link-test dir");
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create temporary parent dir");
        }
        std::fs::write(&path, b"").expect("create temporary file");
        path
    }

    /// `link_at` against the real filesystem, the way a local pane resolves.
    fn local_link_at(line: &str, col: usize, roots: &LinkRoots, files: bool) -> Option<LinkMatch> {
        link_at(line, col, roots, files, &mut local_probe)
    }

    fn one_root(cwd: &Path) -> LinkRoots {
        LinkRoots::local(vec![cwd.to_path_buf()])
    }

    fn assert_file_link(
        line: &str,
        col: usize,
        cwd: &Path,
        expected_path: &Path,
        expected_line: Option<u32>,
        expected_column: Option<u32>,
    ) {
        let link = local_link_at(line, col, &one_root(cwd), true).expect("file link under cursor");
        match link.target {
            LinkTarget::File {
                path, line, column, ..
            } => {
                assert_eq!(path, expected_path);
                assert_eq!(line, expected_line);
                assert_eq!(column, expected_column);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn link_at_detects_relative_file_paths_from_cwd() {
        let path = temp_file("src/main.rs");
        let cwd = path.parent().and_then(Path::parent).unwrap();

        assert_file_link(
            "error src/main.rs:10:2 failed",
            8,
            cwd,
            &path,
            Some(10),
            Some(2),
        );

        let link = local_link_at("error src/main.rs:10:2 failed", 8, &one_root(cwd), true)
            .expect("file link under cursor");
        assert_eq!((link.start, link.end), (6, 21));
    }

    #[test]
    #[cfg(unix)]
    fn tilde_expansion_prefers_home_inferred_from_the_pane_cwd() {
        let cwd = Path::new("/Users/alice/clone/tty7");
        assert_eq!(
            expand_home("~/clone/tty7/src/main.rs", Some(cwd), true),
            Some(PathBuf::from("/Users/alice/clone/tty7/src/main.rs"))
        );
    }

    /// A cwd that reveals no home is the end of the road for a pane on another
    /// machine: this machine's `$HOME` describes nobody there, and a path built
    /// out of it would be asked about — and possibly answered — on the far side.
    #[test]
    #[cfg(unix)]
    fn tilde_expansion_does_not_borrow_this_machines_home_for_another_one() {
        let cwd = Path::new("/srv/app");
        assert_eq!(expand_home("~/.zshrc", Some(cwd), false), None);
        assert_eq!(
            expand_home("~/.zshrc", Some(Path::new("/home/deploy/app")), false),
            Some(PathBuf::from("/home/deploy/.zshrc")),
            "a cwd that does reveal the home needs nothing from us"
        );
        assert!(
            expand_home("~/.zshrc", Some(cwd), true).is_some(),
            "a local pane still falls back to the environment"
        );
    }

    #[test]
    fn link_at_detects_absolute_file_paths_and_single_line_suffix() {
        let path = temp_file("absolute.log");
        let line = format!("open {}:42 now", path.display());
        let col = line.chars().position(|c| c == '/').unwrap_or(5);

        assert_file_link(&line, col, Path::new("/"), &path, Some(42), None);
    }

    #[test]
    fn link_at_trims_wrappers_and_trailing_punctuation_around_file_paths() {
        let path = temp_file("wrapped/src/lib.rs");
        let cwd = path.parent().and_then(Path::parent).unwrap();
        let line = "see (src/lib.rs:7), now";

        let link = local_link_at(line, 7, &one_root(cwd), true).expect("wrapped file link");
        assert_eq!((link.start, link.end), (5, 16));
        match link.target {
            LinkTarget::File {
                path: got,
                line,
                column,
                is_dir,
            } => {
                assert_eq!(got, path);
                assert_eq!(line, Some(7));
                assert_eq!(column, None);
                assert!(!is_dir);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn link_at_rejects_missing_files_and_file_detection_can_be_disabled() {
        let cwd = std::env::temp_dir();

        assert_eq!(
            local_link_at("missing src/nope.rs:1", 9, &one_root(&cwd), true),
            None
        );
        assert_eq!(
            local_link_at("missing src/nope.rs:1", 9, &one_root(&cwd), false),
            None
        );

        let path = temp_file("disabled.rs");
        let line = format!("open {}", path.display());
        assert!(local_link_at(&line, 6, &one_root(&cwd), false).is_none());
    }

    #[test]
    fn link_at_keeps_url_detection_ahead_of_file_detection() {
        let url = "https://example.com/src/main.rs";
        let link = local_link_at(url, 10, &one_root(Path::new("/")), true).expect("URL link");
        assert_eq!(
            link,
            LinkMatch {
                start: 0,
                end: url.len() - 1,
                target: LinkTarget::Url(url.to_string()),
            }
        );
    }

    #[test]
    fn link_at_detects_directory_paths() {
        let file = temp_file("dircase/nested/inner.txt");
        let dir = file.parent().unwrap();
        let cwd = dir.parent().and_then(Path::parent).unwrap();

        let link = local_link_at("artifacts in dircase/nested here", 14, &one_root(cwd), true)
            .expect("directory link");
        assert_eq!((link.start, link.end), (13, 26));
        match link.target {
            LinkTarget::File {
                path,
                line,
                column,
                is_dir,
            } => {
                assert_eq!(path, dir);
                assert_eq!(line, None);
                assert_eq!(column, None);
                assert!(is_dir, "a directory link says so, so the tree can take it");
            }
            LinkTarget::Url(url) => panic!("expected directory link, got URL {url}"),
        }

        assert!(local_link_at("ls dircase/nested/ done", 5, &one_root(cwd), true).is_some());
        assert!(
            local_link_at(
                "artifacts in dircase/nested here",
                14,
                &one_root(cwd),
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_relative_path_falls_back_to_the_repository_root() {
        // What `cargo` prints from a member directory: the path is measured
        // from the workspace root, not from where the shell happens to be.
        let path = temp_file("multiroot/crates/inner/src/lib.rs");
        let root = path
            .ancestors()
            .nth(4)
            .expect("multiroot/ above crates/inner/src")
            .to_path_buf();
        let cwd = root.join("crates/inner");
        std::fs::create_dir_all(cwd.join("empty")).expect("create a cwd with no src/");

        let line = "warning: unused import crates/inner/src/lib.rs:3";
        let col = line.find("crates").expect("path start");
        assert_eq!(local_link_at(line, col, &one_root(&cwd), true), None);

        let roots = LinkRoots::local(vec![cwd.clone(), root.clone()]);
        let link = local_link_at(line, col, &roots, true).expect("resolved from the repo root");
        match link.target {
            LinkTarget::File { path: got, .. } => assert_eq!(got, path),
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn the_nearer_root_wins_when_both_hold_the_name() {
        let near = temp_file("ambiguous/crate/src/main.rs");
        let far = temp_file("ambiguous/src/main.rs");
        let cwd = near
            .ancestors()
            .nth(2)
            .expect("ambiguous/crate above src/")
            .to_path_buf();
        let root = far.ancestors().nth(2).expect("ambiguous/").to_path_buf();

        let roots = LinkRoots::local(vec![cwd, root]);
        let link = local_link_at("at src/main.rs:1", 3, &roots, true).expect("file link");
        match link.target {
            LinkTarget::File { path, .. } => assert_eq!(
                path, near,
                "the pane's own directory is tried before the repository around it"
            ),
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn an_unanswered_probe_resolves_to_nothing_and_asks_about_every_root() {
        let mut asked: Vec<PathBuf> = Vec::new();
        let roots = LinkRoots::local(vec![PathBuf::from("/work/crate"), PathBuf::from("/work")]);
        let link = link_at("see src/lib.rs:9 there", 5, &roots, true, &mut |path, _| {
            asked.push(path.to_path_buf());
            Probe::Unknown
        });

        assert_eq!(link, None, "nothing underlines until the host has answered");
        assert_eq!(
            asked,
            vec![
                PathBuf::from("/work/crate/src/lib.rs"),
                PathBuf::from("/work/src/lib.rs"),
            ],
            "every root is asked about, so one round trip covers them all"
        );
    }

    /// `Update(src/main.rs)` is how Claude Code — and anything else that
    /// writes a path inside a call — prints the file it just touched. The
    /// leading strip in [`trim_file_token`] only walks characters off the
    /// front, so before the call-wrapper rule this token stayed
    /// `Update(src/main.rs` however far it was trimmed: a path nothing could
    /// ever answer for, printed on every single tool call.
    #[test]
    fn a_path_written_inside_a_call_is_reachable() {
        let line = "⏺ Update(src/main.rs)";
        let candidate = file_candidate_at(line, 9).expect("candidate");
        assert_eq!(candidate.path, "src/main.rs");
        assert_eq!(
            line.chars()
                .skip(candidate.start)
                .take(candidate.end + 1 - candidate.start)
                .collect::<String>(),
            "src/main.rs",
            "the span has to cover the path alone, or the underline is drawn \
             over the call that wrapped it"
        );

        assert_eq!(
            file_candidate_at("⏺ Update(src/main.rs:12)", 9)
                .expect("candidate")
                .line,
            Some(12),
            "unwrapping the call must not cost the line suffix"
        );
        assert_eq!(
            file_candidate_at("⏺ Update(docs/my(v2).md)", 9)
                .expect("candidate")
                .path,
            "docs/my(v2).md",
            "only the paren the token ends at is the wrapper"
        );

        // A sentence puts its own punctuation after the call, and the wrapper
        // rule reads the token's *final* character — so the tail has to come
        // off before the `)` underneath it can be recognised.
        for line in [
            "⏺ Update(src/main.rs).",
            "⏺ Update(src/main.rs),",
            "⏺ Update(src/main.rs)、",
        ] {
            assert_eq!(
                file_candidate_at(line, 9).expect("candidate").path,
                "src/main.rs",
                "{line} names the same file the unpunctuated line does"
            );
        }
    }

    /// Parens are legal in a Unix filename, and the call-wrapper rule has to
    /// keep its hands off every one of these. The judgement is the token's
    /// whole shape — where it ends, what stands in front of the paren and what
    /// stands inside it — because nothing here guesses at what a function name
    /// looks like.
    #[test]
    fn a_paren_that_belongs_to_the_filename_is_left_alone() {
        for (line, col, want) in [
            ("ls my(file).txt", 3, "my(file).txt"),
            // Ends at a paren, but a path stands in front of it: a name, not a
            // call. Nothing may rewrite this to `bar`.
            ("ls src/foo(bar)", 4, "src/foo(bar)"),
            // Ends at a paren with a name in front, but nothing path-shaped
            // inside it. Rewriting this to `bar` is how a click opens a
            // different file rather than none.
            ("ls foo(bar)", 3, "foo(bar)"),
            // The `(` is unpaired, which the old rule read as a wrapper and
            // silently rewrote to `bar.txt` — a different file, and one that
            // may well exist.
            ("ls src/foo(bar.txt", 4, "src/foo(bar.txt"),
            ("ls docs/notes[1]", 4, "docs/notes[1]"),
            // The trailing strip runs before the wrapper rule now, so it must
            // not take a bracket the name closed for itself with it.
            ("ls src/foo(bar).", 4, "src/foo(bar)"),
        ] {
            assert_eq!(
                file_candidate_at(line, col).expect("candidate").path,
                want,
                "{line} names a file that is legally called that"
            );
        }

        for (line, col) in [("see (src/main.rs) ok", 6), ("at f (src/main.rs:3:9)", 7)] {
            assert_eq!(
                file_candidate_at(line, col).expect("candidate").path,
                "src/main.rs",
                "{line} was already reachable and must stay that way"
            );
        }
    }

    /// The shape a coding agent prints on every tool call: the command it is
    /// about to run, echoed inside a call that opens on this line and closes
    /// on the next. Neither prefix is a wrapper — `Bash` and `D` are names —
    /// so the front-trim in [`trim_file_token`] cannot reach past them, and
    /// the call-wrapper rule wants a `)` this line does not have. The token
    /// stayed `Bash(D=/…/plan.md`, read as a *relative* path and therefore
    /// looked for under the pane's own directory, where it can never be.
    #[test]
    fn a_path_is_reached_past_a_prefix_that_is_not_a_wrapper() {
        let path = temp_file("2026-09-01-plan.md");
        let shown = path.display().to_string();
        let prefix = "⏺ Bash(D=";
        let line = format!("{prefix}{shown}");
        let start = prefix.chars().count();

        let link = local_link_at(&line, start + 2, &one_root(Path::new("/")), true)
            .expect("the path the assignment names");
        assert_eq!(
            (link.start, link.end),
            (start, line.chars().count() - 1),
            "the underline has to cover the path alone — the call and the \
             variable in front of it are not part of the link"
        );
        match link.target {
            LinkTarget::File {
                path: got,
                line,
                column,
                is_dir,
            } => {
                assert_eq!(got, path);
                assert_eq!(line, None);
                assert_eq!(column, None);
                assert!(!is_dir);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    /// The prefixes a path arrives glued to. Each is a name rather than a
    /// wrapper character, so none of them can be walked off the front, and
    /// each one used to swallow the path whole.
    #[test]
    fn a_prefix_glued_onto_a_path_does_not_hide_it() {
        for (line, col, want) in [
            ("⏺ Bash(D=/tmp/plan.md", 12, "/tmp/plan.md"),
            // The call alone, with no assignment inside it.
            ("⏺ Bash(/tmp/run.sh", 10, "/tmp/run.sh"),
            ("cargo --config=/etc/app.conf", 18, "/etc/app.conf"),
            ("PATH=~/bin", 6, "~/bin"),
            // Relative too: the roots are what a reading like this is measured
            // from, exactly as for a bare token.
            ("build --out=dist/app.js", 14, "dist/app.js"),
            ("touched a.txt,docs/b.md", 16, "docs/b.md"),
        ] {
            let readings: Vec<String> = file_candidates_at(line, col)
                .into_iter()
                .map(|candidate| candidate.path)
                .collect();
            assert!(
                readings.iter().any(|path| path == want),
                "{line} names {want} however the prefix in front of it is \
                 written; got {readings:?}"
            );
        }
    }

    /// An anchored reading is a fallback, never a replacement. Every character
    /// in [`is_prefix_boundary`] is legal in a Unix filename, so the token as
    /// written has to be the first thing probed — otherwise a file that really
    /// is called `a=b/c.txt` gets silently re-read as `b/c.txt`, which is how
    /// a click opens a different file rather than none.
    #[test]
    fn the_token_as_written_is_probed_before_any_anchored_reading() {
        for (line, col, whole) in [
            ("ls a=b/c.txt", 3, "a=b/c.txt"),
            ("ls src/foo(bar.txt", 4, "src/foo(bar.txt"),
            ("ls docs/a,b/c.md", 4, "docs/a,b/c.md"),
        ] {
            assert_eq!(
                file_candidates_at(line, col)
                    .first()
                    .expect("a reading")
                    .path,
                whole,
                "{line} has to answer for itself before anything reads part of \
                 it as a prefix"
            );
        }

        // Nothing path-shaped stands after the boundary, so there is no
        // anchored reading to be had and the bare word is left alone.
        for (line, col) in [("ls foo(bar)", 3), ("ls my(file).txt", 3)] {
            assert_eq!(
                file_candidates_at(line, col).len(),
                1,
                "{line} has no reading past its paren worth a probe"
            );
        }
    }

    /// Clicking the call's name is not clicking the path. The span shrinks to
    /// the path, so the column that asked falls outside it — the same answer
    /// `url_at` gives for a hover on a CJK prefix.
    #[test]
    fn a_click_on_the_call_name_is_not_a_click_on_the_path() {
        assert_eq!(file_candidate_at("⏺ Update(src/main.rs)", 3), None);
    }

    /// CJK prose has no spaces in it, so a path printed inside a Chinese
    /// sentence has no whitespace to stop at. Every line here failed before
    /// the CJK edge sets: the first two have no whitespace at all, and the
    /// third has a perfectly good space on the left and still lost, because
    /// `。完成` was glued to the right.
    #[test]
    fn a_path_glued_to_cjk_prose_is_reachable() {
        for (line, col) in [
            ("文档：docs/plan.md，未提交", 3),
            ("路径（docs/plan.md）已生成", 3),
            ("写入 docs/plan.md。完成", 3),
        ] {
            assert_eq!(
                file_candidate_at(line, col).expect("candidate").path,
                "docs/plan.md",
                "{line} names a path the click cannot reach otherwise"
            );
        }
    }

    /// A span is how a review note or a coding agent names a region of a file,
    /// and the file it names is the one a bare `:1215` would name. Before
    /// this, the `-` disqualified the whole suffix, the range was carried into
    /// the path, and nothing on any filesystem answered for
    /// `remote.rs:1215-1247`. A date is held out by the rule that a span ends
    /// at or after it starts — one hyphen is not enough to tell them apart.
    #[test]
    fn a_line_range_resolves_to_the_line_it_starts_on() {
        for (line, col, path, want) in [
            ("see src/main.rs:12-30 here", 5, "src/main.rs", Some(12)),
            (
                "改动（src/terminal/remote.rs:1215-1247）",
                4,
                "src/terminal/remote.rs",
                Some(1215),
            ),
            // A range of one line is still a range.
            ("see src/main.rs:12-12 here", 5, "src/main.rs", Some(12)),
            // Dates: the second hyphen stops the first, `9 < 2026` the second.
            ("see notes:2026-09-02 here", 5, "notes:2026-09-02", None),
            ("see notes:2026-09 here", 5, "notes:2026-09", None),
        ] {
            let candidate = file_candidate_at(line, col).expect("candidate");
            assert_eq!(candidate.path, path, "{line}");
            assert_eq!(candidate.line, want, "{line}");
            assert_eq!(candidate.column, None, "{line}");
        }
    }

    /// The two CJK edge sets disagree here, which is why both are kept: a han
    /// character inside the *filename* must not end the token, while a han
    /// character standing between prose and path must. Neither reading is
    /// right for both lines, so the first that resolves wins.
    #[test]
    fn a_cjk_filename_and_cjk_prose_get_different_readings() {
        assert_eq!(
            file_candidate_at("参见：docs/设计文档.md", 3)
                .expect("candidate")
                .path,
            "docs/设计文档.md",
            "the punctuation edge keeps a CJK filename whole, and it is tried \
             first for exactly that reason"
        );

        let readings: Vec<String> = file_candidates_at("修改了src/main.rs和docs/b.md", 3)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(
            readings.iter().any(|p| p == "src/main.rs"),
            "no punctuation separates the prose from the path here, so only \
             the han edge can reach it; got {readings:?}"
        );
    }

    /// The cost control for the multi-candidate walk. A token with no CJK in
    /// it must collapse to a single reading *off a single scan*, or every
    /// ordinary hover would pay to tokenise the line three times and to probe
    /// candidates that can only repeat each other — and on a remote pane a
    /// probe is a round trip.
    ///
    /// The wrapped call is here for the second half of that: `Update(…)` does
    /// carry a prefix boundary, and the anchored reading of it lands on the
    /// span [`trim_file_token`] had already unwrapped. Charging a probe for
    /// the same span twice is what the deduplication in [`push_candidate`] is
    /// for, and this is what holds it in place.
    #[test]
    fn an_ascii_line_yields_exactly_one_reading() {
        for (line, col) in [
            ("wrote scratchpad/notes.md now", 8),
            ("error in crates/core/src/lib.rs:88:5 here", 12),
            ("see (src/main.rs) ok", 6),
            ("⏺ Update(src/main.rs)", 9),
            // The half-width edge is spelled with a lookout at CJK for exactly
            // this reason: `:` and `,` are ordinary characters on an ASCII
            // line and must not split it into readings.
            ("run a,b,c and see src/lib.rs:9", 20),
        ] {
            assert_eq!(
                file_candidates_at(line, col).len(),
                1,
                "{line} has no CJK in it, so the three edge sets must agree — \
                 and no anchored reading may add a span the token did not \
                 already cover"
            );
            // The premise the fast path rests on: skipping the other two scans
            // is only free because they cannot see the token differently.
            let widest = token_at(line, col, TokenEdge::Whitespace);
            for edge in [TokenEdge::CjkPunct, TokenEdge::Cjk] {
                assert_eq!(
                    token_at(line, col, edge),
                    widest,
                    "{line} must tokenise the same under {edge:?} as under \
                     whitespace, or the single scan is answering for a reading \
                     it never took"
                );
            }
        }
    }

    /// CJK prose is written with half-width punctuation at least as often as
    /// with the full-width kind, and the ASCII marks offer no boundary of
    /// their own — `文档` and `src/main.rs` are one whitespace-delimited token
    /// either way. The mark counts as an edge only because CJK stands on the
    /// far side of it.
    #[test]
    fn half_width_punctuation_separates_cjk_prose_from_a_path() {
        for (line, col) in [("文档:src/main.rs", 3), ("见:src/main.rs,谢谢", 2)] {
            let readings: Vec<String> = file_candidates_at(line, col)
                .into_iter()
                .map(|candidate| candidate.path)
                .collect();
            assert!(
                readings.iter().any(|path| path == "src/main.rs"),
                "{line} names a path the click cannot reach otherwise; got \
                 {readings:?}"
            );
        }

        assert_eq!(
            url_at("见:www.example.com,谢谢", 2).as_deref(),
            Some("https://www.example.com"),
            "the `www.` arm asks `starts_with`, so a glued-on prefix defeats \
             it outright unless the edge cuts the token first"
        );

        assert_eq!(
            file_candidate_at("see src/main.rs:12 here", 5)
                .expect("candidate")
                .line,
            Some(12),
            "a `:` with no CJK against it still carries the line suffix"
        );
    }

    /// What a failed click gets told. The readings are ordered most
    /// conservative first, and the most conservative reading of a Chinese line
    /// is the whole sentence — reporting *that* as a missing file is a
    /// notification the user cannot act on.
    #[test]
    fn an_unresolved_report_names_the_narrowest_path_shaped_reading() {
        let line = "修改了src/main.rs和docs/b.md";
        let lookup = link_lookup_at(
            line,
            3,
            &LinkRoots::local(vec![PathBuf::from("/work")]),
            true,
            &mut |_, _| Probe::Miss,
        );
        assert_eq!(lookup.link, None);
        assert_eq!(
            lookup.candidate.expect("a report candidate").path,
            "src/main.rs",
            "the sentence around the path is not what the user clicked"
        );

        let word = link_lookup_at(
            "wrote notes now",
            8,
            &LinkRoots::local(vec![PathBuf::from("/work")]),
            true,
            &mut |_, _| Probe::Miss,
        );
        assert_eq!(
            word.candidate.expect("a report candidate").path,
            "notes",
            "a token that is not path-shaped still comes back, so the caller \
             is the one that decides it is not worth reporting"
        );
    }

    /// The `www.` arm asks `starts_with`, so unlike the scheme arm it cannot
    /// relocate its own start inside the token — a glued-on Chinese prefix
    /// defeated it outright until the token edge learned about CJK.
    #[test]
    fn url_at_detects_bare_www_glued_to_cjk_prose() {
        assert_eq!(
            url_at("见：www.example.com，谢谢", 2).as_deref(),
            Some("https://www.example.com")
        );
    }

    #[test]
    fn a_path_shaped_token_is_kept_apart_from_a_bare_word() {
        let path_shaped = file_candidate_at("wrote scratchpad/notes.md now", 8).expect("candidate");
        assert_eq!(path_shaped.path, "scratchpad/notes.md");
        assert!(path_shaped.looks_like_a_path());

        let word = file_candidate_at("wrote notes now", 8).expect("candidate");
        assert_eq!(word.path, "notes");
        assert!(
            !word.looks_like_a_path(),
            "a bare word must not raise a notification on every modifier-click"
        );
    }

    #[test]
    fn a_relative_candidate_has_no_paths_without_a_root() {
        let candidate = file_candidate_at("see src/lib.rs here", 5).expect("candidate");
        assert!(
            candidate.paths(&LinkRoots::default()).is_empty(),
            "a pane that never said where it is cannot measure a relative path"
        );
        assert_eq!(
            candidate.paths(&LinkRoots::local(vec![
                PathBuf::from("/w"),
                PathBuf::from("/w")
            ])),
            vec![PathBuf::from("/w/src/lib.rs")],
            "a repo root equal to the cwd is one root, not two probes"
        );
    }

    #[test]
    fn an_absolute_candidate_ignores_the_roots_entirely() {
        let candidate = file_candidate_at("open /etc/hosts now", 6).expect("candidate");
        assert_eq!(
            candidate.paths(&LinkRoots::local(vec![PathBuf::from("/w")])),
            vec![PathBuf::from("/etc/hosts")]
        );
    }

    /// `is_rooted` decides whether a report about an unresolved token may name
    /// a directory it was "looked for under", so it has to agree with
    /// [`FileCandidate::paths`] about when the roots are consulted at all.
    /// Both ask `is_absolute`, and on Windows a leading `/` does not make a
    /// path that — which is why this only claims to hold where it does.
    #[test]
    #[cfg(unix)]
    fn a_rooted_candidate_is_told_apart_from_one_measured_from_a_root() {
        for line in ["open /etc/hosts now", "open ~/.zshrc now"] {
            assert!(
                file_candidate_at(line, 6).expect("candidate").is_rooted(),
                "{line} says for itself where it starts"
            );
        }
        assert!(
            !file_candidate_at("see src/lib.rs here", 5)
                .expect("candidate")
                .is_rooted(),
            "a relative path is only ever found by measuring from somewhere"
        );
    }

    #[test]
    fn link_at_requires_a_file_when_a_line_suffix_is_present() {
        let file = temp_file("localhost/keep.txt");
        let cwd = file.parent().and_then(Path::parent).unwrap();

        assert_eq!(
            local_link_at("listening on localhost:8080", 15, &one_root(cwd), true),
            None
        );
        assert!(local_link_at("listening on localhost", 15, &one_root(cwd), true).is_some());
    }
}
