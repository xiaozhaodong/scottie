//! Which sidebar group a tab belongs to, and where that answer came from.
//!
//! Two kinds of group live in the same sidebar and look alike, but they are
//! owned by different things. A [`GroupKey::Repo`] is *derived*: the sidebar
//! recomputes it every frame from the tab's cwd, so a tab that `cd`s into
//! another repository walks into another group on its own. A
//! [`GroupKey::Custom`] is *stated*: the user put the tab there by hand, and
//! nothing about the cwd may move it out again.
//!
//! Keeping the two in one enum is what makes the "never overwrite a stated
//! group" rule enforceable — the alternative, a path plus a `pinned` flag,
//! leaves the flag and the path free to disagree, and a custom group's name
//! is not a path in the first place.

use std::path::{Path, PathBuf};

/// The prefix that marks a custom group in the flat string spelling. No
/// absolute path can collide with it: a POSIX root starts with `/` and a
/// Windows one with a single-letter drive, so neither can begin `custom:`.
const CUSTOM: &str = "custom:";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    /// The repository (or, under `RepoOrDirectory`, the plain directory) the
    /// sidebar worked out for itself. Recomputed from the cwd every frame.
    Repo(PathBuf),
    /// A group the user named and put this tab in. Never recomputed.
    Custom(String),
}

impl GroupKey {
    /// The custom group called `name`, or `None` when the name is blank —
    /// an empty name would print as an unlabelled header and collide with
    /// Scratch, which already owns "no name at all".
    pub fn custom(name: &str) -> Option<Self> {
        let name = name.trim();
        (!name.is_empty()).then(|| Self::Custom(name.to_string()))
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    pub fn repo_root(&self) -> Option<&Path> {
        match self {
            Self::Repo(p) => Some(p),
            Self::Custom(_) => None,
        }
    }

    /// The flat spelling used everywhere a group has to survive as one
    /// string: the control protocol, the session file, and the list of
    /// folded groups in the config.
    pub fn encode(&self) -> String {
        match self {
            // Lossy on purpose. `Config::save` turns a serde failure on a
            // non-UTF-8 `PathBuf` into one `warn!` and a return, so a single
            // repo root with odd bytes in it would silently stop the whole
            // config being written from then on. Two such roots folding
            // together is the cheaper failure by a wide margin.
            Self::Repo(p) => p.to_string_lossy().into_owned(),
            Self::Custom(name) => format!("{CUSTOM}{name}"),
        }
    }

    /// Reads back [`GroupKey::encode`]. Anything without the marker is a
    /// repo root, which is also what every group written before custom
    /// groups existed decodes to.
    pub fn decode(s: &str) -> Option<Self> {
        match s.strip_prefix(CUSTOM) {
            Some(name) => Self::custom(name),
            None => (!s.is_empty()).then(|| Self::Repo(PathBuf::from(s))),
        }
    }
}

impl serde::Serialize for GroupKey {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.encode())
    }
}

impl<'de> serde::Deserialize<'de> for GroupKey {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::decode(&s).ok_or_else(|| serde::de::Error::custom("empty sidebar group key"))
    }
}

/// How a group is named in `Config::sidebar_collapsed_groups`. The scratch
/// group has no key of its own, so it is written as the empty string — which
/// neither a repo root nor a custom group can ever be.
pub fn collapse_key(key: Option<&GroupKey>) -> String {
    key.map(GroupKey::encode).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_root_round_trips_as_a_bare_path() {
        let k = GroupKey::Repo(PathBuf::from("/home/u/tty7"));
        assert_eq!(k.encode(), "/home/u/tty7");
        assert_eq!(GroupKey::decode("/home/u/tty7"), Some(k));
    }

    #[test]
    fn a_custom_group_round_trips_under_its_marker() {
        let k = GroupKey::custom("工作").expect("non-blank");
        assert_eq!(k.encode(), "custom:工作");
        assert_eq!(GroupKey::decode("custom:工作"), Some(k));
    }

    /// The whole point of the marker. A custom group named after something
    /// that looks like a path must not come back as a repo root, or folding
    /// one would fold the other.
    #[test]
    fn a_custom_group_named_like_a_path_stays_custom() {
        let k = GroupKey::custom("/home/u/tty7").expect("non-blank");
        assert_eq!(k.encode(), "custom:/home/u/tty7");
        assert_eq!(GroupKey::decode(&k.encode()), Some(k.clone()));
        assert!(k.is_custom());
        assert_ne!(k, GroupKey::Repo(PathBuf::from("/home/u/tty7")));
    }

    /// Every group written before custom groups existed is a bare path.
    #[test]
    fn an_old_session_key_decodes_as_a_repo() {
        assert_eq!(
            GroupKey::decode("/w/alpha"),
            Some(GroupKey::Repo(PathBuf::from("/w/alpha")))
        );
    }

    #[test]
    fn a_blank_name_is_no_group_at_all() {
        assert_eq!(GroupKey::custom("   "), None);
        assert_eq!(GroupKey::decode("custom:"), None);
        assert_eq!(GroupKey::decode(""), None);
    }

    #[test]
    fn a_name_is_trimmed_before_it_becomes_a_key() {
        assert_eq!(GroupKey::custom("  work "), GroupKey::custom("work"));
    }

    #[test]
    fn scratch_collapses_under_the_empty_string() {
        assert_eq!(collapse_key(None), "");
        assert_eq!(
            collapse_key(Some(&GroupKey::Repo(PathBuf::from("/w/a")))),
            "/w/a"
        );
    }
}
