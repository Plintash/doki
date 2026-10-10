use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, TS)]
pub enum CommandScope {
    Project,
    User,
    Skill,
    Builtin,
    Waku,
}

impl CommandScope {
    /// Presentation order in the composer picker. Resolution precedence is
    /// separate: project and user commands can still override less-specific
    /// commands with the same name.
    pub const fn display_rank(self) -> u8 {
        match self {
            Self::Builtin | Self::Waku => 0,
            Self::Project => 1,
            Self::User => 2,
            Self::Skill => 3,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Project => tr!("command_scope.project"),
            Self::User => tr!("command_scope.user"),
            Self::Skill => tr!("command_scope.skill"),
            Self::Builtin => tr!("command_scope.builtin"),
            Self::Waku => tr!("command_scope.waku"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub scope: CommandScope,
    pub argument_hint: Option<String>,
    pub template: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
}

/// A submitted `/compact` for a provider whose transport runs the command
/// itself instead of prompting the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactInvocation {
    /// `/compact` alone: the provider compacts with its default framing.
    Bare,
    /// `/compact <focus>`: the provider focuses the summary on this text.
    WithInstructions(String),
}

impl CompactInvocation {
    /// The focus text as the provider's command takes it.
    pub fn instructions(self) -> Option<String> {
        match self {
            Self::Bare => None,
            Self::WithInstructions(instructions) => Some(instructions),
        }
    }
}

/// Parse a `/compact [focus]` invocation.
///
/// This is pure syntax: whether the name still belongs to the provider's
/// built-in is decided by resolution before the prompt reaches a transport,
/// so a project, user or skill command named `compact` expands first and never
/// looks like this. `None` for any other submission.
pub fn parse_compact_invocation(text: &str) -> Option<CompactInvocation> {
    let invocation = text.strip_prefix('/')?;
    let (name, arguments) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, arguments)| {
            (name, arguments.trim())
        });
    if name != "compact" {
        return None;
    }
    Some(if arguments.is_empty() {
        CompactInvocation::Bare
    } else {
        CompactInvocation::WithInstructions(arguments.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_compact_invocation() {
        assert_eq!(
            parse_compact_invocation("/compact"),
            Some(CompactInvocation::Bare)
        );
        assert_eq!(
            parse_compact_invocation("/compact   focus on the API "),
            Some(CompactInvocation::WithInstructions(
                "focus on the API".to_owned()
            ))
        );
        assert_eq!(
            parse_compact_invocation("/compact\nfocus"),
            Some(CompactInvocation::WithInstructions("focus".to_owned()))
        );
        assert_eq!(parse_compact_invocation("compact"), None);
        assert_eq!(parse_compact_invocation("/skill:compact"), None);
        assert_eq!(parse_compact_invocation("/compaction"), None);
        assert_eq!(parse_compact_invocation("please /compact"), None);
        assert_eq!(
            CompactInvocation::WithInstructions("focus".to_owned()).instructions(),
            Some("focus".to_owned())
        );
        assert_eq!(CompactInvocation::Bare.instructions(), None);
    }
}
