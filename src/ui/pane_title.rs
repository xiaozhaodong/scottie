//! The name a pane wears along its top edge.
//!
//! A tab strip can only ever name one pane; a window of splits running three
//! different things needs each of them to say what it is. The header is the
//! cheapest way to do that — one line of quiet text, no chrome around it.
//!
//! It is drawn quietly on purpose: no background of its own, no rule under it,
//! no icon. The pane's own background shows through and the text is a neutral
//! grey rather than a faded copy of the terminal foreground, so a strongly
//! tinted theme does not drag the header's colour with it.
//!
//! **This is tty7's own, not a copy of anybody's.** Otty is where the look was
//! taken from — a centred, unadorned line above the grid — but Otty does not
//! put a header on a pane. Its own docs describe the window title and the tab
//! name living in the title bar and the tab UI, and a split pane's grip as a
//! small capsule that appears on hover. A persistent per-pane name is a tty7
//! extension in that visual language, and the numbers below are chosen for it
//! rather than measured off a shipped feature.
//!
//! Only a tab with something to tell apart gets one. A single pane is already
//! named by the window title, the tab strip and the sidebar, and a fourth copy
//! of that name would cost it 30px of grid for nothing — which is also why a
//! zoomed pane drops the header while it owns the tab.
//!
//! On a pane that can be rearranged the header is also the grip: the whole
//! strip picks the pane up. That replaces the three dots
//! [`crate::ui::pane_drag`] draws when there is no header, which would
//! otherwise sit exactly where the centred title does — and a 30px full-width
//! target is a good deal easier to hit than a 12px one.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AnyView, App, AppContext as _, EntityId, FocusHandle, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, v_flex};

use crate::ui::i18n::{L10nKey, t};
use crate::ui::pane_drag::{DragPane, PaneDragState, begin};

/// How much of a pane's height the header takes.
///
/// Not measured off a shipped header anywhere — see the module docs for why
/// there is none to measure. 30px is what a centred `text_xs` line wants
/// around it at the density the rest of this window is drawn at, and it
/// swallows [`crate::ui::pane_drag::HANDLE_STRIP`] whole, so a split pane pays
/// this *instead of* the grip strip rather than on top of it.
pub(crate) const PANE_TITLE_HEIGHT: f32 = 30.;

/// How opaque the title paints in a pane that does not have focus, before the
/// pane's own `dim` is applied on top.
///
/// Kept even when `dim_inactive_panes` is off: which pane the keyboard is
/// talking to is exactly what a window of identically-titled shells needs the
/// header to answer, and dimming is not always there to answer it.
const UNFOCUSED_INK: f32 = 0.65;

/// How much background the strip picks up while the pointer is over it, on a
/// pane that can be dragged by it. Nothing at rest — the whole point of the
/// header is that it reads as text floating over the pane.
const GRIP_HOVER_BG: f32 = 0.06;

/// What a header needs to know to draw itself.
///
/// `label` and `full` are the same string on all but a long path: the caller
/// shortens through [`crate::ui::path_display::short_title`], and the tooltip
/// shows the whole of it either way. Nothing here re-derives either — a pane's
/// name is settled once, so its header, its tab and its sidebar row cannot
/// disagree about it.
pub(crate) struct Header<'a> {
    pub pane: EntityId,
    /// The name as drawn.
    pub label: String,
    /// The name in full, which the grip's tooltip always spells out.
    ///
    /// Always, rather than only when `label` differs from it, because the two
    /// cuts a name can take are measured in different units and only one of
    /// them is known here. `short_title` cuts on a *glyph count*; the strip
    /// then cuts again on *pixels*, through `text_ellipsis_start`. A name that
    /// cleared the first cut untouched — `~/work/src`, `vim — main.rs`, three
    /// segments and well under forty glyphs — is still cut by the second one
    /// on a pane narrow enough, and a header exists on narrow panes by
    /// construction. Raising `ui_font_size` moves the pixel budget and leaves
    /// the glyph count where it was, so the population that overflows without
    /// having been shortened grows with it.
    ///
    /// Measuring the painted width instead would mean a custom element
    /// comparing shaped text against its bounds every frame, to save one line
    /// in a tooltip that pops up regardless to carry the drag hint. The line
    /// is the cheaper answer.
    pub full: String,
    /// The pane's own focus handle, so a click on the strip hands the keyboard
    /// to the pane the strip names.
    pub focus: FocusHandle,
    pub focused: bool,
    pub dim: f32,
    /// `Some` only where the pane can actually be moved, and what turns the
    /// strip into the grip.
    pub drag: Option<&'a PaneDragState>,
}

/// The strip along a pane's top edge, carrying its name.
pub(crate) fn bar(header: Header<'_>, cx: &App) -> AnyElement {
    let Header {
        pane,
        label,
        full,
        focus,
        focused,
        dim,
        drag,
    } = header;

    let ink = cx
        .theme()
        .muted_foreground
        .opacity(if focused { 1.0 } else { UNFOCUSED_INK })
        .opacity(dim);
    let hover_bg = cx.theme().foreground.opacity(GRIP_HOVER_BG * dim);
    // Elided from the *front*. Everything the shortener already dropped came
    // off the head for the same reason: `me@mac:~/work/tty7/src` reads
    // identically to its neighbours until the last two segments, so a cut that
    // kept the head would throw away the only part that answers "which pane is
    // this".
    let text = div()
        .max_w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis_start()
        .text_xs()
        .text_color(ink)
        .child(label);

    let strip = div()
        .id(("pane-title", pane.as_u64() as usize))
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(PANE_TITLE_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        // Clear of the grid's own padding, so a title as wide as the pane
        // still stops short of the edges rather than running into them.
        .px(px(12.))
        // A press on the strip belongs to the pane the strip names. Nothing
        // else here would hand the keyboard over: the terminal's own surface
        // begins *below* the header, so without this the name of a pane is a
        // 30px band that looks like part of it and answers to nothing. The
        // press is then stopped, or the pane underneath reads a grab as the
        // start of a text selection.
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx);
            cx.stop_propagation();
        });

    // No tooltip on the pane that has none of the above: `drag` is only `None`
    // while some pane in this tab is already lifted, and a name popping up
    // under a pointer mid-drag is noise.
    let Some(state) = drag else {
        return strip.child(text).into_any_element();
    };

    let state = state.clone();
    let full = SharedString::from(full);
    strip
        .map(crate::ui::reorder::cursor_grab)
        .hover(move |s| s.bg(hover_bg))
        .tooltip(move |window, cx| grip_tooltip(full.clone(), window, cx))
        .on_drag(DragPane, move |_, _, _, cx| {
            cx.stop_propagation();
            begin(&state, pane);
            cx.new(|_| DragPane)
        })
        .child(text)
        .into_any_element()
}

/// What the grip says while the pointer rests on it: the pane's whole name,
/// and under it the hint that this strip picks the pane up.
///
/// The hint is why the tooltip exists at all — the dots this header replaced
/// carried [`L10nKey::PaneDragHandleTooltip`], and a header that dropped it
/// would leave the drag harder to find than it was before, a bare cursor
/// change over a strip that looks like a label. The name rides along
/// unconditionally; see [`Header::full`] for why it is not made conditional on
/// the name having visibly been cut.
fn grip_tooltip(full: SharedString, window: &mut Window, cx: &mut App) -> AnyView {
    let hint = t(L10nKey::PaneDragHandleTooltip);
    Tooltip::element(move |_, cx| {
        v_flex().gap_0p5().child(full.clone()).child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint),
        )
    })
    .build(window, cx)
}
