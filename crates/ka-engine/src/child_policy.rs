//! The posture every child voice inherits from its parent.
//!
//! A child voice — pathfinder, a delegate agent, a `/tan` job, a team
//! member, or the calls a `code` script routes — is a `Voice` of its
//! own, built by its own hand. While each piece of the parent's policy
//! had to be remembered at each construction site, some site always
//! forgot one: hooks reached no child at all, rules missed pathfinder
//! and `/tan`, and a model could route any call through the child that
//! forgot. So the policy is ONE value, built once from the parent, and
//! every child applies it whole.
//!
//! Scope of the inherited hooks: the tool-call policy only
//! (`pre_tool_use` / `post_tool_use`). Turn-, prompt- and
//! session-scoped hooks describe the user's own session and stay with
//! the parent.

use crate::config::HookEvent;
use crate::voice::Voice;

/// The parent posture a child voice is born with.
#[derive(Clone, Default)]
pub struct ChildPolicy {
    /// The parent's fs sandbox (`None` leaves the child's default).
    pub sandbox: Option<ka_sandbox::Policy>,
    /// `[[rules]]`, first-match-wins — the same list the parent gate uses.
    pub rules: Vec<crate::config::Rule>,
    /// `[shell]` environment policy.
    pub shell_env: Option<crate::config::ShellEnvPolicy>,
    /// The parent's feature toggles, shared live: a `/features` change
    /// reaches a running child's next call.
    pub features: Option<crate::features::FeatureSlot>,
    /// The parent's tool-call hooks (see [`ChildPolicy::tool_hooks`]).
    pub hooks: Vec<crate::config::Hook>,
}

impl ChildPolicy {
    /// The parent's tool-call policy hooks: `pre_tool_use` (veto,
    /// argument patch, pre-approval) and `post_tool_use` (result
    /// flags, feedback). Every other event is session-scoped and
    /// stays with the parent.
    pub fn tool_hooks(hooks: &[crate::config::Hook]) -> Vec<crate::config::Hook> {
        hooks
            .iter()
            .filter(|hook| matches!(hook.event, HookEvent::PreToolUse | HookEvent::PostToolUse))
            .cloned()
            .collect()
    }

    /// Apply the whole posture to a freshly built child voice.
    pub fn apply(&self, voice: &mut Voice) {
        if let Some(sandbox) = &self.sandbox {
            voice.set_sandbox(sandbox.clone());
        }
        voice.set_rules(self.rules.clone());
        if let Some(shell_env) = &self.shell_env {
            voice.set_shell_env(shell_env.clone());
        }
        if let Some(features) = &self.features {
            voice.set_feature_slot(features.clone());
        }
        voice.set_hooks(self.hooks.clone());
    }
}
