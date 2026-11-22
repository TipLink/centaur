//! The connector policy shared by sandbox processes and the MCP control plane.
//!
//! Nothing here authorizes a credential: iron-proxy only injects what a principal holds.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Which Code Mode connectors are visible and callable under the deployment or request filter.
///
/// An entry matches a connector only when it equals the connector's directory name exactly.
/// Nothing here authorizes a credential: iron-proxy only injects what a principal holds.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectorPolicy {
    /// None is unrestricted. An explicitly empty set allows no connectors.
    pub allowlist: Option<BTreeSet<String>>,
    pub blocklist: Option<BTreeSet<String>>,
}

impl ConnectorPolicy {
    /// Read the same four settings from either the sandbox spec or the runner's environment.
    pub fn from_env<S: AsRef<str>>(get: impl Fn(&str) -> Option<S>) -> Self {
        fn names(raw: impl AsRef<str>) -> BTreeSet<String> {
            raw.as_ref()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        }
        let allowlist = get("CENTAUR_CODEMODE_ALLOWLIST").map(names).or_else(|| {
            get("TOOL_ALLOWLIST")
                .map(names)
                .filter(|names| !names.is_empty())
        });
        let blocklist: BTreeSet<_> = [get("TOOL_BLOCKLIST"), get("CENTAUR_CODEMODE_BLOCKLIST")]
            .into_iter()
            .flatten()
            .flat_map(names)
            .collect();
        Self {
            allowlist,
            blocklist: (!blocklist.is_empty()).then_some(blocklist),
        }
    }

    pub fn from_allowlist(names: Vec<String>) -> Self {
        Self {
            allowlist: Some(names.into_iter().collect()),
            blocklist: None,
        }
    }

    pub fn allows(&self, name: &str) -> bool {
        self.allowlist.as_ref().is_none_or(|set| set.contains(name))
            && !self
                .blocklist
                .as_ref()
                .is_some_and(|set| set.contains(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedicated_allowlist_replaces_legacy_and_preserves_explicit_empty() {
        for (dedicated, attio, websearch) in [
            (None, true, false),
            (Some(" websearch,mercator "), false, true),
            (Some(""), false, false),
            (Some(" , "), false, false),
        ] {
            let policy = ConnectorPolicy::from_env(|key| match key {
                "TOOL_ALLOWLIST" => Some("attio,linear"),
                "CENTAUR_CODEMODE_ALLOWLIST" => dedicated,
                _ => None,
            });
            assert_eq!(policy.allows("attio"), attio);
            assert_eq!(policy.allows("websearch"), websearch);
        }
    }

    #[test]
    fn blocklists_are_additive_with_or_without_an_allowlist() {
        for dedicated in [None, Some("attio,slack,websearch")] {
            let policy = ConnectorPolicy::from_env(|key| match key {
                "TOOL_ALLOWLIST" => Some(" , "),
                "CENTAUR_CODEMODE_ALLOWLIST" => dedicated,
                "TOOL_BLOCKLIST" => Some("attio"),
                "CENTAUR_CODEMODE_BLOCKLIST" => Some("slack"),
                _ => None,
            });
            assert!(policy.allows("websearch"));
            assert!(!policy.allows("attio"));
            assert!(!policy.allows("slack"));
        }
    }
}
