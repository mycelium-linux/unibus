//! Node grants: the hub's static statement of who a node is and what it may
//! do. Registration claims are checked against the grant and fail closed.
//! Capabilities are an open set of namespaced strings so a new domain never
//! requires an L1 release.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Authoritative origin for content and state.
    Origin,
    /// Site pull-through cache; serves reads, writes go through origin.
    Cache,
    /// Display/audio/sensor surface with no authority of its own.
    #[default]
    Edge,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Origin => "origin",
            Self::Cache => "cache",
            Self::Edge => "edge",
        };
        f.write_str(name)
    }
}

/// One bus slot: identity plus permitted addressing scope. Unknown config
/// fields are a hard error — a typo'd grant must not silently widen scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeGrant {
    pub producer_id: String,
    /// OIDC subject (fpl-auth) this node's tokens must present. Empty only in
    /// v1 compatibility mode (shared-token auth).
    #[serde(default)]
    pub auth_subject: String,
    #[serde(default)]
    pub zones: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub publishes: Vec<String>,
    #[serde(default)]
    pub publish_zones: Vec<String>,
    #[serde(default)]
    pub publish_screen_anchors: Vec<String>,
    #[serde(default)]
    pub publish_producers: Vec<String>,
    #[serde(default)]
    pub screen_anchors: Vec<String>,
    /// Declared topology label: `home-lan`, `wan`, `agora-lan`, ...
    #[serde(default = "default_site")]
    pub site: String,
    #[serde(default)]
    pub role: Role,
}

fn default_site() -> String {
    "default".into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    InvalidName { field: &'static str, value: String },
    Duplicate(String),
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { field, value } => {
                write!(f, "invalid {field} name: {value:?}")
            }
            Self::Duplicate(value) => write!(f, "duplicate grant entry: {value:?}"),
        }
    }
}

impl std::error::Error for GrantError {}

/// Namespaced, lowercase, boring: `canvas.display`, `audio.out`,
/// `presence.capture`, `sensor.temp`. At least two segments.
pub fn is_valid_namespaced_name(value: &str) -> bool {
    let mut segments = value.split('.');
    let count = value.match_indices('.').count() + 1;
    segments.all(is_valid_slug) && count >= 2
}

/// A message-kind pattern: a namespaced name, or the namespace followed by
/// `.*`.
///
/// The wildcard is one trailing segment and nothing else — `arborist.*` is a
/// namespace, `arborist.fin*` is a typo. Everything before the star still has to
/// be a namespaced name, so a pattern can only ever be narrower than "everything
/// this producer could name".
pub fn is_valid_kind_pattern(value: &str) -> bool {
    match value.strip_suffix(".*") {
        // A namespace is one or more slugs: `arborist` names a prefix of one
        // segment, `fpl.node` a prefix of two, and `*` names everything, which
        // is a policy a person has to write rather than a grant a node gets.
        Some(namespace) => !namespace.is_empty() && namespace.split('.').all(is_valid_slug),
        None => is_valid_namespaced_name(value),
    }
}

/// Whether `kind` is covered by `allowed`: the same kind, or a declared
/// namespace.
///
/// `arborist.*` covers `arborist.finding` and `arborist.scan` and does not
/// cover `arboristx.foo`, because the character after the prefix has to be the
/// dot that made it a namespace.
fn kind_matches(allowed: &str, kind: &str) -> bool {
    match allowed.strip_suffix(".*") {
        Some(namespace) => kind
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('.')),
        None => allowed == kind,
    }
}

/// Single-segment identifier: `living-room`, `screen-tv`, `home-lan`.
pub fn is_valid_slug(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl NodeGrant {
    pub fn validate(&self) -> Result<(), GrantError> {
        for (field, values, valid) in [
            (
                "capability",
                &self.capabilities,
                is_valid_namespaced_name as fn(&str) -> bool,
            ),
            ("message_kind", &self.publishes, is_valid_kind_pattern),
            ("zone", &self.zones, is_valid_slug),
            ("publish_zone", &self.publish_zones, is_valid_slug),
            ("screen_anchor", &self.screen_anchors, is_valid_slug),
            (
                "publish_screen_anchor",
                &self.publish_screen_anchors,
                is_valid_slug,
            ),
            ("publish_producer", &self.publish_producers, is_valid_slug),
        ] {
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                if !valid(value) {
                    return Err(GrantError::InvalidName {
                        field,
                        value: value.clone(),
                    });
                }
                if !seen.insert(value.clone()) {
                    return Err(GrantError::Duplicate(value.clone()));
                }
            }
        }
        if !is_valid_slug(&self.producer_id) || !is_valid_slug(&self.site) {
            return Err(GrantError::InvalidName {
                field: "identity",
                value: format!("{}@{}", self.producer_id, self.site),
            });
        }
        if self.auth_subject.trim().is_empty() {
            return Err(GrantError::InvalidName {
                field: "auth_subject",
                value: self.auth_subject.clone(),
            });
        }
        Ok(())
    }

    /// v1 compatibility mode: JARVIS router v1 grants use bare capability
    /// tags (`display`, `audio_output`) and shared bearer tokens. Same
    /// duplicate/blank checks, no namespacing requirement. A v2 deployment
    /// must use [`NodeGrant::validate`].
    pub fn validate_v1_compat(&self) -> Result<(), GrantError> {
        for value in self
            .capabilities
            .iter()
            .chain(self.publishes.iter())
            .chain(self.zones.iter())
            .chain(self.publish_zones.iter())
            .chain(self.screen_anchors.iter())
            .chain(self.publish_screen_anchors.iter())
            .chain(self.publish_producers.iter())
        {
            if value.trim().is_empty() || value.len() > 128 {
                return Err(GrantError::InvalidName {
                    field: "grant entry",
                    value: value.clone(),
                });
            }
        }
        if self.producer_id.trim().is_empty() || self.producer_id.len() > 128 {
            return Err(GrantError::InvalidName {
                field: "identity",
                value: self.producer_id.clone(),
            });
        }
        Ok(())
    }

    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    pub fn in_zone(&self, zone: &str) -> bool {
        self.zones.iter().any(|z| z == zone)
    }

    pub fn serves(&self, zone: &str, capability: &str) -> bool {
        self.in_zone(zone) && self.can(capability)
    }

    pub fn claims_screen(&self, anchor: &str) -> bool {
        self.screen_anchors.iter().any(|a| a == anchor)
    }

    /// True when `claimed` is a subset of the grant. Checked at Register.
    pub fn admits_capabilities(&self, claimed: &[String]) -> bool {
        claimed.iter().all(|c| self.can(c))
    }

    /// Whether this node may publish `kind`.
    ///
    /// Two ways to earn it, and the second is the one that keeps the hub out of
    /// the way: an explicit grant, or the node's own namespace. `arborist` may
    /// publish `arborist.finding` because it is called `arborist` — so a new
    /// event kind is a change to the producer and to nothing else. A producer
    /// can never reach outside its own name this way, and a declared `ns.*`
    /// pattern is still how a node reaches a namespace it does not own.
    pub fn may_publish(&self, kind: &str) -> bool {
        if self
            .publishes
            .iter()
            .any(|allowed| kind_matches(allowed, kind))
        {
            return true;
        }
        kind.strip_prefix(self.producer_id.as_str())
            .is_some_and(|rest| rest.starts_with('.'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grant named `arborist` that declares nothing. Its own namespace is the
    /// only thing it may publish.
    fn own_namespace_only() -> NodeGrant {
        NodeGrant {
            producer_id: "arborist".into(),
            auth_subject: "arborist".into(),
            zones: vec!["default".into()],
            publish_zones: vec!["default".into()],
            capabilities: Vec::new(),
            publishes: Vec::new(),
            publish_screen_anchors: Vec::new(),
            publish_producers: Vec::new(),
            screen_anchors: Vec::new(),
            site: "default".into(),
            role: Role::default(),
        }
    }

    #[test]
    fn a_producer_may_publish_its_own_namespace_without_listing_the_kinds() {
        let grant = own_namespace_only();
        assert!(grant.may_publish("arborist.scan"));
        assert!(grant.may_publish("arborist.finding"));
        // The point of the rule: a new kind is not a hub configuration change.
        assert!(grant.may_publish("arborist.anything.at.all"));
    }

    #[test]
    fn a_producer_may_not_reach_outside_its_own_namespace() {
        let grant = own_namespace_only();
        assert!(!grant.may_publish("elmers.notice"));
        // A prefix is not a namespace: `arboristx` is somebody else.
        assert!(!grant.may_publish("arboristx.scan"));
        // The bare name is not a namespaced kind either.
        assert!(!grant.may_publish("arborist"));
    }

    #[test]
    fn a_declared_pattern_covers_its_namespace_and_the_exact_name_still_works() {
        let mut grant = own_namespace_only();
        grant.publishes = vec!["canvas.*".into(), "fpl.node.health.v1".into()];
        assert!(grant.may_publish("canvas.command"));
        assert!(grant.may_publish("fpl.node.health.v1"));
        assert!(!grant.may_publish("canvasx.command"));
        assert!(!grant.may_publish("fpl.node.health"));
    }

    #[test]
    fn a_pattern_is_one_trailing_star_and_nothing_else() {
        assert!(is_valid_kind_pattern("arborist.*"));
        assert!(is_valid_kind_pattern("arborist.scan"));
        assert!(!is_valid_kind_pattern("arborist.*.scan"));
        assert!(!is_valid_kind_pattern("arborist.fin*"));
        assert!(!is_valid_kind_pattern("*"));
        assert!(!is_valid_kind_pattern("arborist."));
    }

    fn grant() -> NodeGrant {
        NodeGrant {
            producer_id: "home-pi-tv".into(),
            auth_subject: "avery/home-pi".into(),
            zones: vec!["living-room".into()],
            capabilities: vec!["canvas.display".into(), "audio.out".into()],
            publishes: vec!["canvas.command".into()],
            publish_zones: vec!["living-room".into()],
            publish_screen_anchors: vec!["screen-tv".into()],
            publish_producers: vec![],
            screen_anchors: vec!["screen-tv".into()],
            site: "home-lan".into(),
            role: Role::Edge,
        }
    }

    #[test]
    fn namespaced_names_validate() {
        assert!(is_valid_namespaced_name("canvas.display"));
        assert!(is_valid_namespaced_name("sensor.temp-c"));
        assert!(!is_valid_namespaced_name("display"));
        assert!(!is_valid_namespaced_name("Canvas.display"));
        assert!(!is_valid_namespaced_name("canvas."));
        assert!(!is_valid_namespaced_name(".canvas"));
    }

    #[test]
    fn grant_admits_subset_only() {
        let g = grant();
        assert!(g.validate().is_ok());
        assert!(g.admits_capabilities(&["canvas.display".into()]));
        assert!(!g.admits_capabilities(&["canvas.display".into(), "presence.capture".into()]));
        assert!(g.serves("living-room", "audio.out"));
        assert!(!g.serves("bedroom", "audio.out"));
        assert!(g.claims_screen("screen-tv"));
        assert!(!g.claims_screen("screen-left"));
    }

    #[test]
    fn unknown_grant_fields_are_rejected() {
        let json = serde_json::to_value(grant()).unwrap();
        let mut obj = json.as_object().unwrap().clone();
        obj.insert("admin".into(), serde_json::json!(true));
        let parsed: Result<NodeGrant, _> = serde_json::from_value(serde_json::Value::Object(obj));
        assert!(parsed.is_err());
    }

    #[test]
    fn duplicates_and_blanks_fail() {
        let mut g = grant();
        g.zones = vec!["living-room".into(), "living-room".into()];
        assert!(matches!(g.validate(), Err(GrantError::Duplicate(_))));
        let mut g = grant();
        g.site = " ".into();
        assert!(matches!(g.validate(), Err(GrantError::InvalidName { .. })));
    }
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn zones_and_anchors_are_slugs_but_capabilities_must_namespace() {
        let mut g = NodeGrant {
            producer_id: "home-pi-tv".into(),
            auth_subject: "avery/home-pi".into(),
            zones: vec!["living-room".into()],
            capabilities: vec!["display".into()],
            publishes: vec![],
            publish_zones: vec![],
            publish_screen_anchors: vec![],
            publish_producers: vec![],
            screen_anchors: vec!["screen-tv".into()],
            site: "home-lan".into(),
            role: Role::Edge,
        };
        assert!(matches!(
            g.validate(),
            Err(GrantError::InvalidName {
                field: "capability",
                ..
            })
        ));
        g.capabilities = vec!["canvas.display".into()];
        assert_eq!(g.validate(), Ok(()));
    }
}
