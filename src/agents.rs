//! Coding-agent identity and the nonempty, canonical image capability set.

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, de};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentId {
    Claude,
    Codex,
    Grok,
}

impl AgentId {
    pub(crate) const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Grok];

    pub(crate) fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Grok => "Grok Build",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }
}

/// Image contents promised by coop. Empty sets cannot be constructed or loaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct AgentSet(Vec<AgentId>);

impl AgentSet {
    pub(crate) fn new(mut agents: Vec<AgentId>) -> Result<Self> {
        if agents.is_empty() {
            bail!("an image must include at least one coding agent");
        }
        agents.sort_unstable();
        agents.dedup();
        Ok(Self(agents))
    }

    pub(crate) fn contains(&self, agent: AgentId) -> bool {
        self.0.binary_search(&agent).is_ok()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.0.iter().copied()
    }

    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[AgentId] {
        &self.0
    }

    pub(crate) fn names(&self) -> String {
        self.iter().map(AgentId::name).collect::<Vec<_>>().join(",")
    }
}

impl Default for AgentSet {
    fn default() -> Self {
        Self(AgentId::ALL.to_vec())
    }
}

impl<'de> Deserialize<'de> for AgentSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let agents = Vec::<AgentId>::deserialize(deserializer)?;
        Self::new(agents).map_err(de::Error::custom)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::{AgentId, AgentSet};

    #[test]
    fn set_is_canonical_and_nonempty() {
        assert!(AgentSet::new(Vec::new()).is_err());
        let set = AgentSet::new(vec![AgentId::Grok, AgentId::Codex, AgentId::Grok]).unwrap();
        assert_eq!(set.as_slice(), &[AgentId::Codex, AgentId::Grok]);
        assert_eq!(serde_json::to_string(&set).unwrap(), r#"["codex","grok"]"#);
        assert!(serde_json::from_str::<AgentSet>("[]").is_err());
        assert!(serde_json::from_str::<AgentSet>(r#"["unknown"]"#).is_err());
    }
}
