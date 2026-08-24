//! The name a pane wears in the window chrome.
//!
//! The title bar can name the one pane that is currently visible. A split
//! keeps its panes clean and uses the small top-edge grip for rearranging;
//! putting a permanent title over every pane would compete with the terminal
//! content and duplicate the tab/sidebar labels.
//!
//! It is drawn quietly on purpose: no background of its own, no rule under it,
//! no icon. The titlebar's background shows through and the text is a neutral
//! grey rather than a faded copy of the terminal foreground.
//!
//! **This is tty7's own, not a copy of anybody's.** Otty is where the look was
//! taken from — a centred, unadorned line above the grid — but Otty does not
//! put a header on a pane. Its own docs describe the window title and the tab
//! name living in the title bar and the tab UI, and a split pane's grip as a
//! small capsule that appears on hover. This title follows that restrained
//! visual language without adding another per-pane grid header.
//!
//! The setting behind this title remains named `show_pane_title` for config
//! compatibility. It controls the title-bar label for a single visible pane
//! (including zoom), not a persistent header inside each split pane.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, EntityId, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    div, px,
};
use gpui_component::tooltip::Tooltip;

/// How opaque the title paints in a pane that does not have focus, before the
/// pane's own `dim` is applied on top.
///
/// Kept even when `dim_inactive_panes` is off: which pane the keyboard is
/// talking to is exactly what a window of identically-titled shells needs the
/// header to answer, and dimming is not always there to answer it.
const UNFOCUSED_INK: f32 = 0.65;

/// What a header needs to know to draw itself.
///
/// `label` and `full` are the same string on all but a long path: the caller
/// shortens through [`crate::ui::path_display::short_title`], and the tooltip
/// shows the whole of it either way. Nothing here re-derives either — a pane's
/// name is settled once, so its title, its tab and its sidebar row cannot
/// disagree about it.
pub(crate) struct Header {
    pub pane: EntityId,
    /// The name as drawn.
    pub label: String,
    /// The name in full, which the title's tooltip spells out.
    ///
    /// Always, rather than only when `label` differs from it, because the two
    /// cuts a name can take are measured in different units and only one of
    /// them is known here. `short_title` cuts on a *glyph count*; the title
    /// then cuts again on *pixels*, through `text_ellipsis_start`. A name that
    /// cleared the first cut untouched — `~/work/src`, `vim — main.rs`, three
    /// segments and well under forty glyphs — is still cut by the second one
    /// on a pane narrow enough, and a title exists for every single/zoomed
    /// pane by construction. Raising `ui_font_size` moves the pixel budget and leaves
    /// the glyph count where it was, so the population that overflows without
    /// having been shortened grows with it.
    ///
    /// Measuring the painted width instead would mean a custom element
    /// comparing shaped text against its bounds every frame, to save one line
    /// in a tooltip that pops up to carry the full name. The line
    /// is the cheaper answer.
    pub full: String,
    /// The pane's own focus handle, so a click on the title hands the keyboard
    /// to the pane it names.
    pub focus: FocusHandle,
    pub focused: bool,
    pub dim: f32,
}

/// The title for the one pane visible in the window title bar.
///
/// This is part of the 40px window chrome rather than the terminal grid. It is
/// deliberately not a drag target: a split's hover grip remains the only pane
/// rearrangement affordance.
pub(crate) fn chrome(header: Header, cx: &App) -> AnyElement {
    let Header {
        pane,
        label,
        full,
        focus,
        focused,
        dim,
    } = header;

    let config = cx.global::<crate::core::config::Config>();
    let ink = gpui::Hsla::from(gpui::rgb(config.pane_title_color_rgb()))
        .opacity(if focused { 1.0 } else { UNFOCUSED_INK })
        .opacity(dim);
    let text = div()
        .max_w_full()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis_start()
        .text_size(px(config.pane_title_font_size))
        .text_color(ink)
        .child(label);
    let full = SharedString::from(full);

    div()
        .id(("pane-title-chrome", pane.as_u64() as usize))
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .min_w_0()
        .flex_shrink(1.)
        .px(px(12.))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx);
            cx.stop_propagation();
        })
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        .child(text)
        .into_any_element()
}
