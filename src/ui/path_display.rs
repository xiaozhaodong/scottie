//! The one place a path is shortened for display.
//!
//! Two jobs live here, and both are here because every surface that draws a
//! path has to answer them the same way or the window contradicts itself:
//! shortening a path to start from `~` ([`abbreviate_home`]), and cutting a
//! raw pane title down to the part that identifies it ([`short_title`]). The
//! tab strip, the sidebar, the workspace switcher and a pane's own header all
//! draw the *same* title, so a second rule anywhere means one of them spells
//! the pane's identity differently from the others.
//!
//! Four rows shorten a path for display — the Info panel's cwd, the tab
//! strip's title, the home picker's recent list, a split pane's header — and
//! they used to spell the check their own way: read `HOME`, compare byte
//! prefixes. On Windows that missed twice over. `HOME` is often unset there (the variable is
//! `USERPROFILE`), and even a set home never matched a pane cwd that spells
//! itself with the other separator or different case — a PowerShell pane
//! reports `C:/Users/x/…` while `USERPROFILE` is `C:\Users\x` (#544).
//!
//! The comparison below normalizes both sides to `/` and folds case before
//! comparing, so every spelling of the same directory shortens. What comes
//! back is for reading only — a `~`-rooted path is spelled the way `~` paths
//! are spelled everywhere, with `/`. Nothing feeds it back to an API: the
//! Info panel's Copy Path and Reveal both carry the untouched `PathBuf`, and
//! the tab strip and picker only ever draw it.
//!
//! *Which* home a path is measured against is the caller's to say, because
//! only the caller knows which machine the path is on. A pane, a workspace
//! row or a tab title can name a directory on another host, and this
//! machine's `$HOME` answers for nothing over there: `/home/deploy/app` on a
//! server shortened to `~/app` on a laptop that happens to log in as
//! `deploy`, and stayed long on one that does not, so the `~` meant the
//! wrong machine either way (#580). [`home_for_host`] is where that question
//! is answered — the same borrow #568 took out of the file-link resolver.

use crate::ui::host_ops::HostId;
use gpui::App;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation as _;

/// The directory `~` stands for on the machine tty7 is running on, or `None`
/// when it won't say.
///
/// `USERPROFILE` is the fallback rather than the only source on Windows so
/// the MSYS/Git-Bash environments that do export `HOME` keep working, and
/// the two agree in every case that matters.
pub(crate) fn local_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// The directory `~` stands for in a path that lives on `host`.
///
/// A remote host reports its home during the control handshake and it is
/// kept per host in [`HostLinks`](crate::ui::remote_connect::HostLinks), so
/// this costs a map lookup and never a round trip. `None` means nothing here
/// can say — no link to that host yet — and a path measured against nothing
/// is shown whole, which is the honest answer and the one #568 settled on
/// for the same question about file links.
pub(crate) fn home_for_host(cx: &App, host: HostId) -> Option<PathBuf> {
    match host.is_local() {
        true => local_home(),
        false => crate::ui::remote_connect::HostLinks::home(cx, host),
    }
}

/// `/`-spelled, case-folded, trailing separators dropped — the form two
/// paths are compared in, never the form either is shown in. Case folding is
/// ASCII-only: drive letters and the ASCII half of real paths are where
/// Windows case instability actually lives, and a full Unicode fold would
/// fold a Unix filename that happened to differ only in case into a match it
/// is not.
fn normalized(s: &str) -> String {
    s.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Re-spells a path on **this** machine with the separators this OS expects.
///
/// On Windows the shell's `IShellFolder::ParseDisplayName` bails out with
/// `E_INVALIDARG` on a mixed-separator path — a forward-slash prefix joined
/// with backslash entries. The forward slashes get in from two routes: the
/// shell's PWD (OSC 7 from Git Bash / MSYS bash reports `/`, and that string
/// survives `Path::ancestors()` when the file tree walks up to find `.git`),
/// and `git rev-parse --show-toplevel` from Git for Windows (MSYS2), which
/// always prints `/` regardless of the calling shell. `reveal_path` swallows
/// that failure (it only logs), so handing it native separators is what makes
/// "open folder" actually open.
///
/// **Only for paths on the machine this window runs on.** A remote host's
/// `/home/u/src` is already native over there; re-spelling it would put a
/// path on the clipboard that names nothing on either machine. Every caller
/// sits behind a locality check for that reason.
///
/// The rewrite runs on the path's own UTF-16 code units, not on a
/// `to_string_lossy` copy of them. A Windows filename may hold unpaired
/// surrogates, which `to_string_lossy` turns into `U+FFFD` — the returned
/// path would then silently name a *different* file, and reveal would open
/// nothing without reporting why. `/` and `\` are ASCII, so a code unit
/// equal to one of them is that character and never half of a surrogate
/// pair, which is what makes the swap safe to do one unit at a time.
#[cfg(windows)]
pub(crate) fn native_separators(path: &Path) -> Cow<'_, Path> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const SLASH: u16 = b'/' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    let os = path.as_os_str();
    // Nothing to fix — including every UNC (`\\wsl$\…`, `\\?\…`) and
    // already-native path — hands the caller's own path straight back.
    if !os.encode_wide().any(|unit| unit == SLASH) {
        return Cow::Borrowed(path);
    }
    let wide: Vec<u16> = os
        .encode_wide()
        .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
        .collect();
    Cow::Owned(PathBuf::from(OsString::from_wide(&wide)))
}

/// Off Windows the OS separator is already `/`, and a backslash in a path is
/// an ordinary filename character — there is nothing to re-spell.
#[cfg(not(windows))]
pub(crate) fn native_separators(path: &Path) -> Cow<'_, Path> {
    Cow::Borrowed(path)
}

/// Shortens `path` to start from `~` when it is (inside) `home` — the home
/// directory of the machine `path` is on, not of this one.
///
/// A `None` home is a path this process cannot place: a pane on a host with
/// no link, or one whose shell has ssh'd somewhere tty7 never spoke to. It
/// comes back untouched rather than measured against a home that belongs to
/// somebody else (#580).
///
/// The `~` replaces the home prefix and the remainder is re-spelled with
/// `/` separators (a `~\work` hybrid reads as a root the path never had),
/// but its case and component spelling are the path's own. A path that is
/// exactly home shortens to `~`, and one whose next character is not a
/// separator (`/home/xavier` under `/home/xa`) does not match at all.
///
/// A path already spelled from `~` is handed straight back: the shell wrote
/// it that way, and measuring it against a home directory spelled in full can
/// only fail to match.
pub(crate) fn abbreviate_home<'a>(path: &'a str, home: Option<&Path>) -> Cow<'a, str> {
    if path.starts_with('~') {
        return Cow::Borrowed(path);
    }
    let Some(home) = home else {
        return Cow::Borrowed(path);
    };
    abbreviate_under(path, &home.to_string_lossy())
}

/// `abbreviate_home` with the home as a plain string, so the tests below can
/// pin one — including a Windows-spelled home on a Unix build, which no
/// `Path` on that platform round-trips.
fn abbreviate_under<'a>(path: &'a str, home: &str) -> Cow<'a, str> {
    let home_norm = normalized(home);
    if home_norm.is_empty() {
        return Cow::Borrowed(path);
    }
    let path_norm = normalized(path);
    if path_norm == home_norm {
        return Cow::Owned("~".to_string());
    }
    if !path_norm.starts_with(&home_norm) {
        return Cow::Borrowed(path);
    }
    // The byte after the home prefix has to be a separator. Where it sits in
    // the *original* string is derived from the normalized one rather than
    // from `home.len()`: a trailing-separator difference (`C:\Users\xa\`
    // recorded as home) makes the two lengths disagree, and slicing by the
    // wrong one can split a UTF-8 boundary. Separator and case substitutions
    // preserve byte length, so the boundary found in the normalized string is
    // the boundary in the original. The remainder is re-spelled with `/`:
    // `~\work` reads as a root the path never had.
    let boundary = home_norm.len();
    if !path_norm[boundary..].starts_with('/') {
        return Cow::Borrowed(path);
    }
    Cow::Owned(format!("~/{}", path[boundary + 1..].replace('\\', "/")))
}

/// How many trailing segments a path too deep to show whole keeps. Three is
/// what tells `~/work/tty7/src` from `~/work/other/src` without spending a
/// whole tab chip on the part they share.
const KEEP_SEGMENTS: usize = 3;

/// The separator a path spells itself with. A path carrying a single `\` is
/// a Windows path and has to be put back together with `\`: rejoining it with
/// `/` would make one label spell its location two ways, `C:\Users\dev\app`
/// while it fits and `C:/…/app` once it has to be elided.
pub(crate) fn path_separator(path: &str) -> char {
    if path.contains('\\') { '\\' } else { '/' }
}

pub(crate) fn join_segments(segments: &[&str], sep: char) -> String {
    segments.join(sep.encode_utf8(&mut [0u8; 4]) as &str)
}

/// Splits `text` into grapheme clusters — what a reader counts as one
/// character, and the only place a label may be cut.
///
/// Slicing by `char` passes every width check and still tears the result:
/// `👨‍👩‍👧` loses the joiner holding it together, `❤️` loses the variation
/// selector that makes it an emoji (and the orphan then attaches itself to the
/// ellipsis), and `🇨🇳` leaves behind a lone regional indicator that renders
/// as a bare letter.
pub(crate) fn clusters(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// Whether [`short_title`] could make anything of `raw` — that is, whether it
/// names something at all.
///
/// One shape does not: a bare `user@host:`, which a shell integration writes
/// for the fraction of a second it has a host but not yet a directory. The
/// head comes off and nothing is left, so whatever the caller was going to
/// fall back to says more than this does.
///
/// Asked *before* shortening rather than by testing the result for emptiness,
/// so a caller choosing between two candidate names does not have to shorten
/// the loser to find out it lost.
pub(crate) fn names_something(raw: &str) -> bool {
    !tty7_core::core::tab_view::strip_host_prefix(raw.trim())
        .trim()
        .is_empty()
}

/// Whether two names point at the same directory, as a reader would see them.
///
/// Asked by the surfaces that draw a row's location *under* its name: that
/// second line is worth having until it repeats the first, and provenance
/// alone cannot say when it does. A shell integration titles its pane
/// `user@host:~/repo`, which is a title by provenance and a path in fact, so
/// the row ends up printing `~/repo` over `~/repo`.
///
/// Both sides go through the same three normalizations the rest of this module
/// draws with — the `user@host:` head off, `~` for the home the names belong
/// to, `/`-spelled and case-folded — so the comparison is between what a
/// reader would have read, not between two spellings of it. `home` is the home
/// of the machine *these* names are on, the same borrow every other function
/// here takes (#580).
///
/// A `None` home means nothing here can place either name, and an empty name
/// places nothing at all: both come back `false`. That is the direction to
/// fail in — a caller that wrongly hears "different" prints a line it did not
/// need, while one that wrongly hears "same" throws a line away.
pub(crate) fn same_place(a: &str, b: &str, home: Option<&Path>) -> bool {
    let (a, b) = (place_key(a, home), place_key(b, home));
    !a.is_empty() && a == b
}

/// One name reduced to the form [`same_place`] compares in.
fn place_key(raw: &str, home: Option<&Path>) -> String {
    let bare = tty7_core::core::tab_view::strip_host_prefix(raw.trim()).trim();
    match bare.is_empty() {
        true => String::new(),
        false => normalized(&abbreviate_home(bare, home)),
    }
}

/// A raw pane title cut down to the part that identifies it.
///
/// This is the one normalization every surface that names a pane runs: the
/// tab strip, the sidebar, the workspace switcher and the pane's own header.
/// A second rule anywhere would let one row spell a pane's identity
/// differently from the next, which is the whole thing a name is for.
///
/// What it does, in order:
///
/// 1. Drops the `user@host:` a shell integration prefixes its title with —
///    identical across every pane in the window, so it distinguishes nothing.
///    An SSH address with no path after it (`deploy@10.0.0.5:2222`) is kept
///    whole; there the host *is* the identity (#438).
/// 2. Shortens the path under `home` — the home of the machine the title came
///    from, never this one's (#580).
/// 3. Keeps the last [`KEEP_SEGMENTS`] segments of a deep path, cutting on
///    either separator so a Windows path read on a Mac still loses its head
///    rather than its tail.
/// 4. Clamps an absurd single name on a cluster boundary.
///
/// Everything it drops comes off the *front*, because the tail is what tells
/// two panes apart. Anything drawing the result must elide the same way —
/// `text_ellipsis_start`, not `truncate`.
pub(crate) fn short_title(raw: &str, home: Option<&Path>) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let after_host = tty7_core::core::tab_view::strip_host_prefix(raw);
    let after_host = after_host.trim();
    if after_host.is_empty() {
        return String::new();
    }
    let abbreviated = abbreviate_home(after_host, home);
    let path: &str = abbreviated.as_ref();

    enum Kind {
        Home,
        Absolute,
        Relative,
    }
    let (kind, body) = if let Some(rest) = path.strip_prefix("~/") {
        (Kind::Home, rest)
    } else if path == "~" {
        return "~".to_string();
    } else if let Some(rest) = path.strip_prefix('/') {
        (Kind::Absolute, rest)
    } else {
        (Kind::Relative, path)
    };

    // Both separators: Windows shells report `C:\Users\…` while git and the
    // terminal integration use `/`, and a path must be cut on either one.
    let segments: Vec<&str> = body.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return match kind {
            Kind::Home => "~",
            Kind::Absolute => "/",
            Kind::Relative => "",
        }
        .to_string();
    }

    let sep = path_separator(path);
    let depth = segments.len() + usize::from(matches!(kind, Kind::Home));
    let mut label = if depth > KEEP_SEGMENTS {
        let tail = &segments[segments.len() - KEEP_SEGMENTS..];
        format!("…{sep}{}", join_segments(tail, sep))
    } else {
        match kind {
            Kind::Home => format!("~{sep}{}", join_segments(&segments, sep)),
            Kind::Absolute => format!("/{}", join_segments(&segments, sep)),
            Kind::Relative => join_segments(&segments, sep),
        }
    };
    // Clamped on cluster boundaries, or a label ending in an emoji comes back
    // holding half of one.
    let cells = clusters(&label);
    if cells.len() > 40 {
        label = format!("{}…", cells[..40].concat());
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most of the `short_title` tests are about where a title is *cut*, not
    /// about what `~` means: the paths they pass either already start with `~`
    /// or are nowhere near anybody's home. Naming no home keeps the assertions
    /// off the process environment — and is what a title of unknown provenance
    /// gets in the app too (#580).
    fn short(raw: &str) -> String {
        short_title(raw, None)
    }

    /// Every way of slicing `text` that lands on a grapheme-cluster boundary.
    fn cluster_prefixes(text: &str) -> Vec<String> {
        let cells = clusters(text);
        (0..=cells.len()).map(|n| cells[..n].concat()).collect()
    }

    #[test]
    fn a_path_under_home_shortens_to_tilde() {
        assert_eq!(abbreviate_under("/home/xa", "/home/xa"), "~");
        assert_eq!(abbreviate_under("/home/xa/work", "/home/xa"), "~/work");
        // A home recorded with its trailing separator matches the same paths,
        // and the slice that follows it is found in the *normalized* string,
        // so the two spellings cannot disagree about where the cut is.
        assert_eq!(abbreviate_under("/home/xa/work", "/home/xa/"), "~/work");
        // A longer name that merely starts with home is not under it.
        assert_eq!(abbreviate_under("/home/xavier", "/home/xa"), "/home/xavier");
        assert_eq!(abbreviate_under("/var/tmp", "/home/xa"), "/var/tmp");
        // A home the environment reports as empty leaves every path alone,
        // rather than turning `/` into `~`.
        assert_eq!(abbreviate_under("/var/tmp", ""), "/var/tmp");
        assert_eq!(abbreviate_under("/var/tmp", "/"), "/var/tmp");
    }

    #[test]
    fn separators_and_case_do_not_change_what_counts_as_home() {
        // The Windows miss: USERPROFILE spells `C:\Users\xa`, a PowerShell
        // pane reports `C:/Users/xa/…`, and neither matched the other.
        let home = "C:\\Users\\xa";
        assert_eq!(abbreviate_under("C:/Users/xa/work", home), "~/work");
        assert_eq!(abbreviate_under("c:\\Users\\XA\\work", home), "~/work");
        assert_eq!(abbreviate_under("C:\\Users\\xa", home), "~");
        // The remainder is re-spelled with `/`, case untouched.
        assert_eq!(abbreviate_under("C:/Users/xa/Mix\\ed", home), "~/Mix/ed");
    }

    /// The #580 borrow: a path whose machine is unknown keeps its full
    /// spelling instead of being read against this one's home.
    #[test]
    fn a_path_with_no_home_to_measure_against_is_left_alone() {
        let deploy = "/home/deploy/app";
        assert_eq!(abbreviate_home(deploy, None), deploy);
        // The same path *does* shorten once the host that owns it has said
        // what its home is.
        assert_eq!(
            abbreviate_home(deploy, Some(Path::new("/home/deploy"))),
            "~/app"
        );
        // And this machine's home is not offered as a stand-in: a laptop
        // that logs in as `deploy` used to shorten a server's path by
        // accident, purely because the two names matched.
        assert_eq!(
            abbreviate_home(deploy, Some(Path::new("/Users/thomas"))),
            deploy
        );
    }

    #[test]
    fn a_non_ascii_component_is_sliced_on_a_character_boundary() {
        // The cut is taken from the normalized string; `replace` and the
        // ASCII case fold both preserve byte length, so a multi-byte
        // component before or after the home prefix cannot move it.
        assert_eq!(abbreviate_under("/home/日本/work", "/home/日本"), "~/work");
        assert_eq!(abbreviate_under("/home/xa/日本語", "/home/xa"), "~/日本語");
    }

    #[test]
    fn native_separators_passes_through_a_path_with_nothing_to_fix() {
        // No forward slashes → nothing to rewrite, and no allocation: the
        // borrowed path is the caller's, handed back untouched.
        assert!(matches!(
            native_separators(Path::new("README.md")),
            Cow::Borrowed(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_rewrites_forward_slashes_to_backslashes_on_windows() {
        // The bug: a repo root from `git rev-parse` (forward slashes) joined
        // with backslash-joined entries yields a mixed path, which
        // `ParseDisplayName` rejects. Every `/` must become `\`.
        assert_eq!(
            native_separators(Path::new("D:/code/tty7\\skills")),
            Path::new("D:\\code\\tty7\\skills")
        );
        assert_eq!(
            native_separators(Path::new("D:/code/tty7")),
            Path::new("D:\\code\\tty7")
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_leaves_unc_and_backslash_paths_alone() {
        // A UNC path (`\\wsl$\…`, `\\?\…`) or an already-native path has no
        // `/`, so it passes through borrowed — the replace is a no-op and
        // must not allocate, nor touch the leading `\\`.
        for p in [
            "\\\\wsl$\\Ubuntu\\home",
            "\\\\?\\C:\\code",
            "C:\\code\\tty7",
        ] {
            let got = native_separators(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)), "{p:?} should not allocate");
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_keeps_a_name_a_string_cannot_hold() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        // `0xD800` is a lone high surrogate — legal in an NTFS name, and not
        // representable in a Rust `str`. Rewriting through
        // `to_string_lossy` would swap it for `U+FFFD` and hand back a path
        // naming a *different* file; because `reveal_path` only logs its
        // failures, that reads to the user as the same silent no-op this fix
        // is here to remove. Working on the UTF-16 units keeps the name.
        let raw: Vec<u16> = "C:/a"
            .encode_utf16()
            .chain([0xD800])
            .chain("/b".encode_utf16())
            .collect();
        let path = PathBuf::from(OsString::from_wide(&raw));
        let want: Vec<u16> = "C:\\a"
            .encode_utf16()
            .chain([0xD800])
            .chain("\\b".encode_utf16())
            .collect();
        assert_eq!(
            native_separators(&path)
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            want
        );
        // The round-trip this replaced really did destroy it.
        assert!(path.to_string_lossy().contains('\u{FFFD}'));
    }

    #[cfg(not(windows))]
    #[test]
    fn native_separators_is_a_no_op_off_windows() {
        // On Unix the OS separator is `/`; a path that happens to contain
        // backslashes is legitimate and must be left alone.
        for p in ["/home/u/tty7", "C:\\Users\\dev", "mixed/path\\here"] {
            let got = native_separators(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)));
        }
    }

    /// The comparison behind every "would this row say the same thing twice".
    #[test]
    fn same_place_sees_through_the_spellings_of_one_directory() {
        let home = Path::new("/Users/x");

        // The shape this exists for: a shell integration's title against the
        // cwd it was written from.
        assert!(same_place(
            "user@host:~/repo/tty7",
            "/Users/x/repo/tty7",
            Some(home)
        ));
        // Debian's stock bash spaces the path off the colon.
        assert!(same_place("user@host: ~/repo", "/Users/x/repo", Some(home)));
        // Windows spells one directory both ways and in either case (#544).
        assert!(same_place(
            r"ann@BOX:C:\Users\App",
            "C:/Users/app",
            Some(Path::new(r"C:\Users\ann"))
        ));
        // A trailing separator is not a different place.
        assert!(same_place("~/repo/", "/Users/x/repo", Some(home)));

        // Neighbours are not the same place, and neither is a prefix of one.
        assert!(!same_place("~/repo", "/Users/x/repo/tty7", Some(home)));
        assert!(!same_place(
            "✳ fixing the switcher",
            "/Users/x/repo",
            Some(home)
        ));

        // Nothing here can place a `~` against a full path with no home to
        // measure by, and the answer that keeps a row's second line is the one
        // to give (#580).
        assert!(!same_place("~/repo", "/Users/x/repo", None));
        // The same two paths spelled in full need no home at all.
        assert!(same_place("/srv/app", "/srv/app", None));

        // A name that is nothing once the head is off places nothing, so it
        // matches nothing — not even another one of itself.
        assert!(!same_place("user@host:", "user@host:", Some(home)));
        assert!(!same_place("   ", "", None));
    }

    #[test]
    fn short_title_strips_user_host_and_shows_shallow_path_in_full() {
        assert_eq!(short("user@host:~/projects/app"), "~/projects/app");
        // Debian's stock bash title, which spaces the path off the colon.
        assert_eq!(short("user@host: ~/projects/app"), "~/projects/app");
        assert_eq!(short("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(short("plain"), "plain");
    }

    /// A title shortens under the home of the machine it came from, and
    /// under no other (#580).
    #[test]
    fn short_title_shortens_under_the_home_it_was_given() {
        let server = Path::new("/home/deploy");
        assert_eq!(short_title("/home/deploy/app", Some(server)), "~/app");
        // This machine's home is not a stand-in for the server's: the same
        // path stays whole when the home naming it is somewhere else.
        assert_eq!(
            short_title("/home/deploy/app", Some(Path::new("/Users/thomas"))),
            "/home/deploy/app"
        );
        // And a pane nothing here can place — no link to its host, or a
        // shell that has ssh'd on — shortens against nothing.
        assert_eq!(short_title("/home/deploy/app", None), "/home/deploy/app");
    }

    /// The name a freshly dialled SSH pane wears until the remote shell says
    /// otherwise. Cutting at the colon left the tab reading "2222" (#438).
    #[test]
    fn short_title_keeps_an_ssh_address_whole() {
        assert_eq!(short("deploy@10.0.0.5:2222"), "deploy@10.0.0.5:2222");
        assert_eq!(short("root@prod"), "root@prod");
        assert_eq!(short("prod-web"), "prod-web");
        // Only a port stops the cut: a drive letter is still a path, and this
        // is the title tty7's own pwsh integration writes on Windows.
        assert_eq!(short(r"ann@BOX:C:/Users/app"), r"C:/Users/app");
    }

    #[test]
    fn short_title_truncates_deep_paths_to_trailing_segments() {
        assert_eq!(short("user@host:~/repo/025/tty7"), "…/repo/025/tty7");
        assert_eq!(short("/usr/local/share/man"), "…/local/share/man");
        assert_eq!(short("a/b/c/d"), "…/b/c/d");
    }

    #[test]
    fn short_title_keeps_home_tilde_and_normalizes_trailing_slash() {
        assert_eq!(short("user@host:~"), "~");
        assert_eq!(short("~"), "~");
        assert_eq!(short("a/b/c/"), "a/b/c");
    }

    #[test]
    fn short_title_blank_input_is_empty_and_long_names_are_clamped() {
        assert_eq!(short("   "), "");
        let long = "a".repeat(50);
        let out = short(&long);
        assert_eq!(out.chars().count(), 41);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn short_title_cuts_windows_paths_on_backslashes() {
        assert_eq!(short(r"C:\Users\dev\projects\app"), r"…\dev\projects\app");
        assert_eq!(
            short(r"C:\Users\dev\repo\deep\path\src\ui"),
            r"…\path\src\ui"
        );
        // A shallow Windows path keeps its drive and its backslashes.
        assert_eq!(short(r"C:\Users\app"), r"C:\Users\app");
    }

    /// `short_title`'s 40-glyph clamp is a `char`-indexed cut in disguise.
    #[test]
    fn short_title_clamps_on_cluster_boundaries() {
        for tail in ["\u{1F1E8}\u{1F1F3}-suffix", "\u{2764}\u{fe0f}-suffix"] {
            for pad in 37..=41 {
                let name = format!("{}{tail}", "a".repeat(pad));
                let out = short(&name);
                let Some(body) = out.strip_suffix('…') else {
                    continue;
                };
                assert!(
                    cluster_prefixes(&name).iter().any(|p| p == body),
                    "clamped to {body:?}, not a cluster-aligned prefix of {name:?}"
                );
            }
        }
    }
}
