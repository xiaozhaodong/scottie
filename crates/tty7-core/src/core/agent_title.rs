//! Semantic titles reported by coding agents.
//!
//! Agent terminals mix two different things into OSC 0/2: the task's name and
//! a small activity glyph (`✳`, `◐`, `◑`).  The machine tree needs the former
//! to survive a later `claude`/UUID reset, while the latter is presentation
//! state that a viewer may choose to draw.  Keep that split here so the live
//! window, mirrored workspaces and the CLI all agree on what counts as a task.

use std::borrow::Cow;

use crate::core::cli_agent::CLIAgent;

const TASK_TITLE_MAX: usize = 120;
const ACTIVITY_PREFIXES: &[char] = &['✳', '◐', '◑'];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTitle {
    pub activity_prefix: Option<char>,
    pub title: String,
}

impl AgentTitle {
    pub fn display(&self, show_activity_prefix: bool) -> String {
        match (show_activity_prefix, self.activity_prefix) {
            (true, Some(prefix)) => format!("{prefix} {}", self.title),
            _ => self.title.clone(),
        }
    }
}

/// Turns an agent's raw OSC/hook title into a stable task title.
///
/// The raw value remains stored by the terminal.  This function only decides
/// what is safe and useful to cache/display: status glyphs are split out,
/// control characters become one space, agent self-names and session UUIDs
/// are rejected, and an unexpectedly long title is clamped on a character
/// boundary.
pub fn parse_agent_title(
    agent: CLIAgent,
    session_id: Option<&str>,
    raw: &str,
) -> Option<AgentTitle> {
    let folded = fold_one_line(raw);
    let mut title = folded.as_str();
    let activity_prefix = title
        .chars()
        .next()
        .filter(|c| ACTIVITY_PREFIXES.contains(c));
    if let Some(prefix) = activity_prefix {
        title = title[prefix.len_utf8()..].trim_start();
    }
    let after_host = crate::core::tab_view::strip_host_prefix(title);
    if title.is_empty()
        || after_host != title
        || after_host.trim().is_empty()
        || agent.is_own_name(title)
        || session_id.is_some_and(|id| id.trim() == title)
        || uuid::Uuid::parse_str(title).is_ok()
    {
        return None;
    }

    Some(AgentTitle {
        activity_prefix,
        title: clamp(title, TASK_TITLE_MAX),
    })
}

/// Resolves the visible semantic title from the daemon cache and current OSC.
///
/// A current explicit hook title wins, then a valid OSC title; the cache exists
/// for a later self-name, UUID or empty reset. If OSC describes the same title
/// as the hook, it may still supply the optional activity glyph.
pub fn resolve_agent_title<'a>(
    agent: CLIAgent,
    session_id: Option<&str>,
    raw_title: Option<&str>,
    explicit_task_title: Option<&'a str>,
    last_task_title: Option<&'a str>,
    show_activity_prefix: bool,
) -> Option<Cow<'a, str>> {
    let current = raw_title.and_then(|raw| parse_agent_title(agent, session_id, raw));
    if let Some(explicit) = explicit_task_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        if let Some(current) = current.filter(|current| current.title == explicit) {
            return Some(Cow::Owned(current.display(show_activity_prefix)));
        }
        return Some(Cow::Borrowed(explicit));
    }
    if let Some(current) = current {
        return Some(Cow::Owned(current.display(show_activity_prefix)));
    }
    // The cache is re-checked for a shell location rather than trusted, because
    // it can predate this rule: a daemon built before it cached whatever the
    // shell had titled the pane during the handoff into the agent, and an
    // `execve` self-upgrade deliberately carries that cache across. Reading it
    // back is what makes the fix retroactive on a machine that upgraded in
    // place. Hook titles are not re-checked — those come from the agent's own
    // structured field, so an address in one was meant.
    if let Some(cached) = last_task_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .filter(|title| crate::core::tab_view::strip_host_prefix(title) == *title)
    {
        return Some(Cow::Borrowed(cached));
    }
    None
}

fn fold_one_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn clamp(title: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if title.chars().count() <= max {
        return title.to_string();
    }
    title.chars().take(max - 1).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_metadata_not_part_of_the_cached_title() {
        for prefix in ACTIVITY_PREFIXES {
            let parsed = parse_agent_title(
                CLIAgent::Claude,
                Some("session-1"),
                &format!("{prefix}  武汉明天天气查询"),
            )
            .unwrap();
            assert_eq!(parsed.title, "武汉明天天气查询");
            assert_eq!(parsed.activity_prefix, Some(*prefix));
            assert_eq!(parsed.display(false), "武汉明天天气查询");
            assert_eq!(parsed.display(true), format!("{prefix} 武汉明天天气查询"));
        }
    }

    #[test]
    fn self_names_and_session_identifiers_are_not_tasks() {
        for title in [
            "claude",
            "Claude Code",
            "session-1",
            "01a0368e-41d6-7ec2-9543-315d193d1d64",
        ] {
            assert_eq!(
                parse_agent_title(CLIAgent::Claude, Some("session-1"), title),
                None,
                "{title:?}"
            );
        }
        assert_eq!(parse_agent_title(CLIAgent::Codex, None, "codex"), None);
    }

    #[test]
    fn shell_titles_are_not_agent_tasks() {
        for title in ["user@host:", "user@host:~/repo/tty7", "user@host: /srv/app"] {
            assert_eq!(
                parse_agent_title(CLIAgent::Claude, None, title),
                None,
                "{title:?} is a shell location, not an agent task"
            );
        }
        assert!(
            parse_agent_title(CLIAgent::Claude, None, "deploy@10.0.0.5:2222").is_some(),
            "a port-only address is intentionally not stripped by the shared helper"
        );
        // And the cost of borrowing that helper's idea of a head: it asks only
        // whether an `@` appears before the first colon, so prose carrying an
        // address mid-sentence is refused too. Rejecting it loses a title the
        // agent could have shown; accepting the shapes above would put a shell's
        // location in the daemon's cache, where it outlives the shell. Falling
        // back to `Claude Code` is the cheaper of the two mistakes.
        assert_eq!(
            parse_agent_title(CLIAgent::Claude, None, "fix user@host: routing"),
            None
        );
    }

    #[test]
    fn prose_is_folded_and_clamped_without_breaking_unicode() {
        let parsed =
            parse_agent_title(CLIAgent::Claude, None, "  修复标题\n\t并补充测试\u{0007}  ")
                .unwrap();
        assert_eq!(parsed.title, "修复标题 并补充测试");

        let long = "汉".repeat(TASK_TITLE_MAX + 20);
        let parsed = parse_agent_title(CLIAgent::Claude, None, &long).unwrap();
        assert_eq!(parsed.title.chars().count(), TASK_TITLE_MAX);
        assert!(parsed.title.ends_with('…'));
    }

    #[test]
    fn a_prefix_without_a_task_is_empty() {
        assert_eq!(parse_agent_title(CLIAgent::Claude, None, " ◐  "), None);
    }

    #[test]
    fn a_zero_clamp_limit_is_safe() {
        assert_eq!(clamp("title", 0), "");
    }

    #[test]
    fn a_shell_location_cached_by_an_older_daemon_is_not_shown() {
        assert_eq!(
            resolve_agent_title(
                CLIAgent::Claude,
                None,
                Some("claude"),
                None,
                Some("user@host:~/repo/tty7"),
                false,
            )
            .as_deref(),
            None,
            "the cache is re-read under this rule, so an in-place upgrade heals"
        );
    }

    #[test]
    fn a_current_semantic_title_wins_and_the_cache_handles_invalid_resets() {
        assert_eq!(
            resolve_agent_title(
                CLIAgent::Claude,
                None,
                Some("✳ old task"),
                None,
                Some("new hook title"),
                true,
            )
            .as_deref(),
            Some("✳ old task")
        );
        assert_eq!(
            resolve_agent_title(
                CLIAgent::Claude,
                None,
                Some("claude"),
                None,
                Some("new hook title"),
                true,
            )
            .as_deref(),
            Some("new hook title")
        );
    }

    #[test]
    fn an_explicit_title_outranks_stale_osc_and_matching_osc_can_supply_activity() {
        assert_eq!(
            resolve_agent_title(
                CLIAgent::Claude,
                None,
                Some("✳ stale osc"),
                Some("fresh hook title"),
                Some("fresh hook title"),
                true,
            )
            .as_deref(),
            Some("fresh hook title")
        );
        assert_eq!(
            resolve_agent_title(
                CLIAgent::Claude,
                None,
                Some("✳ fresh hook title"),
                Some("fresh hook title"),
                Some("fresh hook title"),
                true,
            )
            .as_deref(),
            Some("✳ fresh hook title")
        );
    }
}
