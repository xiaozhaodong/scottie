//! Glyph dilation off by default on macOS.
//!
//! CoreGraphics thickens glyph strokes by an amount that depends on the
//! foreground colour's luminance, and gpui replicates that ladder in
//! `glyph_dilation_for_color`. On a Retina panel the extra weight does not read
//! as ink, it reads as blur: the same font looks heavier and less defined than
//! it does in a browser or in Terminal.app.
//!
//! Apple's per-app escape hatch is the `AppleFontSmoothing` default, and gpui
//! honours it — `font_smoothing_allowed_by_user()` reads the key with
//! `CFPreferencesCopyAppValue` and treats *only* an explicit integer `0` as
//! "off". Three properties of that reader shape this module:
//!
//! * The value has to be a number. A string `"0"` fails gpui's `CFNumber`
//!   downcast and is silently ignored — the dilation would stay on with no
//!   sign that anything was written.
//! * gpui caches the answer in a `OnceLock`, so the write must land before the
//!   first glyph is rasterized, and a later change only takes effect on the
//!   next launch. That is also why this is not a Settings toggle: it would look
//!   live and not be.
//! * The read goes to `kCFPreferencesCurrentApplication`, which resolves to the
//!   bundle identifier under `Scottie.app` and to something else entirely for a
//!   bare `cargo run` binary. Writing through the same constant — rather than a
//!   hard-coded `ai.scottie.app` — keeps the two halves in one domain either
//!   way, so what a developer sees locally is what a bundled run does.
//!
//! Nothing carries over from tty7: the fork changed the bundle identifier, so
//! Scottie's domain starts empty (see `BRANDING_UI_PLAN.md`).

use core_foundation::base::{CFType, TCFType};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::preferences::{
    CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, CFPreferencesSetAppValue,
    kCFPreferencesCurrentApplication,
};

const KEY: &str = "AppleFontSmoothing";

/// Turns glyph dilation off unless the domain already carries a value.
///
/// Call once at GUI startup, before the first window exists. Best-effort and
/// silent: the worst case is the platform default, which is what every other
/// Mac app renders with anyway.
pub fn disable_dilation_by_default() {
    let key = CFString::new(KEY);
    if has_value(&key) {
        return;
    }
    let off = CFNumber::from(0i32);
    unsafe {
        CFPreferencesSetAppValue(
            key.as_concrete_TypeRef(),
            off.as_CFTypeRef(),
            kCFPreferencesCurrentApplication,
        );
        CFPreferencesAppSynchronize(kCFPreferencesCurrentApplication);
    }
}

/// Whether the domain already holds this key — ours from an earlier launch, or
/// one the user set by hand. Both are left alone: rewriting our own value would
/// be pointless, and overwriting theirs would make the key impossible to
/// override from the outside.
///
/// The consequence is worth stating plainly, because it is not what the shape
/// of the code suggests: `defaults delete` is *not* how a user gets the system
/// rendering back, since the next launch would write the `0` again. Setting the
/// key to a non-zero integer is — gpui reads any other number as "smoothing
/// allowed", and this function then leaves it standing. The docs say so too.
fn has_value(key: &CFString) -> bool {
    let value = unsafe {
        CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesCurrentApplication)
    };
    if value.is_null() {
        return false;
    }
    // Copy rule: this reference is ours. Wrapping it hands the release to Drop
    // rather than leaking one CFNumber per launch.
    let _owned = unsafe { CFType::wrap_under_create_rule(value) };
    true
}
