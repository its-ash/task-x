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
        if self.config.deny.iter().any(|d| d == tool) || (ToolRisk::from(p) == ToolRisk::Destructive && !listed) {
            return Decision::Deny;
        }
        match self.base(p) {
            Decision::Ask if listed || self.session_grants.contains(tool) => Decision::Allow,
            d => d,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(mode: Mode) -> Policy {
        Policy::new(PolicyConfig { mode, ..Default::default() })
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
        let mut p = Policy::new(PolicyConfig { mode: Mode::Autonomous, allow: vec!["delete_file".into()], deny: vec![] });
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
        });
        assert_eq!(p.decide("run_tests", Permission::Execute), Decision::Allow);
        assert_eq!(p.decide("read_file", Permission::Read), Decision::Deny);
        assert_eq!(p.decide("delete_file", Permission::Delete), Decision::Deny);
        p.grant_session("delete_file");
        assert_eq!(p.decide("delete_file", Permission::Delete), Decision::Deny, "deny list wins over grants");
        p.grant_session("replace_text");
        assert_eq!(p.decide("replace_text", Permission::Write), Decision::Allow);
        let ro = Policy::new(PolicyConfig { mode: Mode::ReadOnly, allow: vec!["replace_text".into()], deny: vec![] });
        assert_eq!(ro.decide("replace_text", Permission::Write), Decision::Deny, "allow list cannot lift read-only");
    }
}
