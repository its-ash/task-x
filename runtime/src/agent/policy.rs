//! Permission policy. Decisions come only from this configuration and the user's approvals;
//! model output, repository text, and instructions can never change them.

use crate::tools::{Permission, ToolRisk};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Reads and searches only.
    ReadOnly,
    /// Reads freely; asks before writing, deleting, or running commands.
    #[default]
    AskBeforeEdit,
    /// Reads and writes freely; asks before deleting or running commands.
    AskBeforeExecution,
    /// Reads, writes, and runs project commands freely; asks before deletes and anything else.
    Autonomous,
}

/// How much the agent may do without asking. When set, it replaces `allow_all` and the destructive
/// opt-in for every mode except read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditMode {
    /// Every tool runs with no prompt; only `deny` refuses.
    Auto,
    /// Free inside the workspace: edits, deletes (both undoable through the edit log), and
    /// sandboxed commands. Asks before anything that can write or delete beyond it: commands when
    /// the OS sandbox is off, installs, git writes, network calls, files from outside the workspace,
    /// and MCP tools.
    Safe,
    /// Asks before every write, delete, and command.
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default)]
    pub mode: Mode,
    /// Tool names that never need approval (cannot override `deny` or read-only mode).
    /// Destructive tools (deletes, installs) run only when named here: this is their explicit opt-in.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tool names that are always refused.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Allow every call with no prompt, destructive tools included; only `deny` still refuses.
    /// MCP tools (`mcp__*`) are then offered in every mode, read-only too. Other writes still
    /// follow a read-only mode. Commands stay in the sandbox either way.
    #[serde(default)]
    pub allow_all: bool,
    #[serde(default)]
    pub edit_mode: Option<EditMode>,
    /// Whether commands run in the OS sandbox; None asks the platform (tests set it).
    #[serde(skip)]
    pub sandboxed: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub config: PolicyConfig,
    /// Tools the user approved "for this session".
    session_grants: HashSet<String>,
}

impl Policy {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config, session_grants: HashSet::new() }
    }

    pub fn grant_session(&mut self, tool: &str) {
        self.session_grants.insert(tool.to_owned());
    }

    fn base(&self, p: Permission) -> Decision {
        use Decision::*;
        use Permission as P;
        match (self.config.mode, p) {
            (_, P::Read) => Allow,
            (Mode::ReadOnly, _) => Deny,
            (Mode::AskBeforeEdit, _) => Ask,
            (Mode::AskBeforeExecution, P::Write) => Allow,
            (Mode::AskBeforeExecution, _) => Ask,
            (Mode::Autonomous, P::Write | P::Execute) => Allow,
            (Mode::Autonomous, _) => Ask,
        }
    }

    pub fn decide(&self, tool: &str, p: Permission) -> Decision {
        let listed = self.config.allow.iter().any(|a| a == tool);
        if self.config.deny.iter().any(|d| d == tool) {
            return Decision::Deny;
        }
        if let Some(em) = self.config.edit_mode.filter(|_| self.config.mode != Mode::ReadOnly) {
            return self.decide_edit(em, tool, p, listed);
        }
        if self.config.allow_all && (tool.starts_with("mcp__") || self.config.mode != Mode::ReadOnly) {
            return Decision::Allow;
        }
        if ToolRisk::from(p) == ToolRisk::Destructive && !listed {
            return Decision::Deny;
        }
        match self.base(p) {
            Decision::Ask if listed || self.session_grants.contains(tool) => Decision::Allow,
            d => d,
        }
    }

    fn decide_edit(&self, em: EditMode, tool: &str, p: Permission, listed: bool) -> Decision {
        use Permission as P;
        let mcp = tool.starts_with("mcp__");
        let free = match em {
            EditMode::Auto => true,
            EditMode::Safe => {
                !mcp && match p {
                    P::Read | P::Write | P::Delete => true,
                    P::Execute => self.sandboxed(),
                    P::Network | P::GitWrite | P::Install | P::External => false,
                }
            }
            EditMode::Ask => p == P::Read && !mcp,
        };
        if free || listed || self.session_grants.contains(tool) { Decision::Allow } else { Decision::Ask }
    }

    fn sandboxed(&self) -> bool {
        self.config.sandboxed.unwrap_or_else(|| {
            crate::tools::exec::SANDBOX.load(std::sync::atomic::Ordering::Relaxed) && crate::tools::exec::sandbox_available()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(mode: Mode) -> Policy {
        Policy::new(PolicyConfig { mode, ..Default::default() })
    }

    #[test]
    fn edit_modes_ask_beyond_their_scope() {
        use Permission as P;
        let em = |edit_mode, sandboxed| {
            Policy::new(PolicyConfig {
                mode: Mode::Autonomous,
                edit_mode: Some(edit_mode),
                sandboxed: Some(sandboxed),
                ..Default::default()
            })
        };
        let auto = em(EditMode::Auto, false);
        for (tool, p) in
            [("delete_file", P::Delete), ("install_pkg", P::Install), ("run_command", P::Execute), ("mcp__figma__get", P::Network)]
        {
            assert_eq!(auto.decide(tool, p), Decision::Allow, "auto runs {tool}");
        }
        let safe = em(EditMode::Safe, true);
        for (tool, p) in [("replace_text", P::Write), ("delete_file", P::Delete), ("run_command", P::Execute), ("read_file", P::Read)] {
            assert_eq!(safe.decide(tool, p), Decision::Allow, "safe runs {tool} inside the workspace");
        }
        for (tool, p) in [
            ("git_push", P::GitWrite),
            ("install_pkg", P::Install),
            ("web_fetch", P::Network),
            ("copy_file", P::External),
            ("mcp__figma__get", P::Network),
        ] {
            assert_eq!(safe.decide(tool, p), Decision::Ask, "safe asks before {tool}");
        }
        assert_eq!(
            em(EditMode::Safe, false).decide("run_command", P::Execute),
            Decision::Ask,
            "no sandbox: a command could write anywhere"
        );
        let mut ask = em(EditMode::Ask, true);
        assert_eq!(ask.decide("read_file", P::Read), Decision::Allow);
        assert_eq!(ask.decide("replace_text", P::Write), Decision::Ask);
        assert_eq!(ask.decide("delete_file", P::Delete), Decision::Ask, "a delete asks; it is not refused");
        ask.grant_session("replace_text");
        assert_eq!(ask.decide("replace_text", P::Write), Decision::Allow);
        let denied = Policy::new(PolicyConfig {
            mode: Mode::Autonomous,
            edit_mode: Some(EditMode::Auto),
            deny: vec!["run_command".into()],
            ..Default::default()
        });
        assert_eq!(denied.decide("run_command", P::Execute), Decision::Deny, "deny wins in every edit mode");
        let ro = Policy::new(PolicyConfig { mode: Mode::ReadOnly, edit_mode: Some(EditMode::Auto), ..Default::default() });
        assert_eq!(ro.decide("replace_text", P::Write), Decision::Deny, "a read-only chat mode stays read-only");
    }

    #[test]
    fn modes_map_permissions() {
        use Permission as P;
        assert_eq!(pol(Mode::ReadOnly).decide("replace_text", P::Write), Decision::Deny);
        assert_eq!(pol(Mode::ReadOnly).decide("read_file", P::Read), Decision::Allow);
        assert_eq!(pol(Mode::AskBeforeEdit).decide("replace_text", P::Write), Decision::Ask);
        assert_eq!(pol(Mode::AskBeforeExecution).decide("replace_text", P::Write), Decision::Allow);
        assert_eq!(pol(Mode::AskBeforeExecution).decide("run_tests", P::Execute), Decision::Ask);
        assert_eq!(pol(Mode::Autonomous).decide("run_tests", P::Execute), Decision::Allow);
        assert_eq!(pol(Mode::Autonomous).decide("delete_file", P::Delete), Decision::Deny, "destructive needs an explicit allow");
    }

    #[test]
    fn destructive_tools_need_an_explicit_allow() {
        use Permission as P;
        let mut p = Policy::new(PolicyConfig { mode: Mode::Autonomous, allow: vec!["delete_file".into()], ..Default::default() });
        assert_eq!(p.decide("delete_file", P::Delete), Decision::Allow);
        assert_eq!(p.decide("install_pkg", P::Install), Decision::Deny);
        p.grant_session("install_pkg");
        assert_eq!(p.decide("install_pkg", P::Install), Decision::Deny, "a session approval is not an opt-in");
        assert_eq!(p.decide("replace_text", P::Write), Decision::Allow);
    }

    #[test]
    fn lists_and_session_grants() {
        let mut p = Policy::new(PolicyConfig {
            mode: Mode::AskBeforeEdit,
            allow: vec!["run_tests".into()],
            deny: vec!["delete_file".into(), "read_file".into()],
            ..Default::default()
        });
        assert_eq!(p.decide("run_tests", Permission::Execute), Decision::Allow);
        assert_eq!(p.decide("read_file", Permission::Read), Decision::Deny);
        assert_eq!(p.decide("delete_file", Permission::Delete), Decision::Deny);
        p.grant_session("delete_file");
        assert_eq!(p.decide("delete_file", Permission::Delete), Decision::Deny, "deny list wins over grants");
        p.grant_session("replace_text");
        assert_eq!(p.decide("replace_text", Permission::Write), Decision::Allow);
        let ro = Policy::new(PolicyConfig { mode: Mode::ReadOnly, allow: vec!["replace_text".into()], ..Default::default() });
        assert_eq!(ro.decide("replace_text", Permission::Write), Decision::Deny, "allow list cannot lift read-only");
    }

    #[test]
    fn allow_all_lets_every_call_run() {
        use Permission as P;
        let all = |mode| Policy::new(PolicyConfig { mode, allow_all: true, deny: vec!["mcp__x__drop".into()], ..Default::default() });
        assert_eq!(all(Mode::Autonomous).decide("mcp__figma__get_screenshot", P::Network), Decision::Allow);
        assert_eq!(all(Mode::Autonomous).decide("delete_file", P::Delete), Decision::Allow, "destructive tools need no opt-in");
        assert_eq!(all(Mode::AskBeforeEdit).decide("web_fetch", P::Network), Decision::Allow, "no prompt");
        assert_eq!(all(Mode::ReadOnly).decide("mcp__chrome__navigate", P::Network), Decision::Allow, "MCP tools in every mode");
        assert_eq!(all(Mode::ReadOnly).decide("replace_text", P::Write), Decision::Deny, "ask mode still does not edit");
        assert_eq!(all(Mode::Autonomous).decide("mcp__x__drop", P::Network), Decision::Deny, "the deny list still wins");
    }
}
