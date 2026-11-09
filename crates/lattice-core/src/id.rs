//! Global entity identity (spec §26).
//!
//! No application creates a permanently isolated identity namespace. Global
//! URIs such as `entity://person/<uuid>`, `entity://file/<id>`,
//! `artifact://blake3/<hash>` are the durable cross-product identity.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

const ENTITY_SCHEME: &str = "entity";
const ARTIFACT_SCHEME: &str = "artifact";

/// A durable, cross-product entity reference (spec §26).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GlobalId {
    /// URI scheme+authority namespace in the form `entity://<type>/<id>`.
    pub uri: String,
}

impl GlobalId {
    pub fn entity(kind: &str, id: &str) -> Self {
        Self {
            uri: format!("{ENTITY_SCHEME}://{kind}/{id}"),
        }
    }

    pub fn artifact_blake3(hash_hex: &str) -> Self {
        Self {
            uri: format!("{ARTIFACT_SCHEME}://blake3/{hash_hex}"),
        }
    }

    pub fn kind(&self) -> Option<&str> {
        let rest = self.uri.strip_prefix(&format!("{ENTITY_SCHEME}://"))?;
        rest.split_once('/').map(|(k, _)| k)
    }

    pub fn is_artifact(&self) -> bool {
        self.uri.starts_with(&format!("{ARTIFACT_SCHEME}://"))
    }

    /// Parse a canonical `GlobalId` from a URI string. Returns `None` for a
    /// malformed shape (wrong scheme, missing kind/id, empty id).
    pub fn from_uri(uri: &str) -> Option<Self> {
        if uri.starts_with(&format!("{ENTITY_SCHEME}://")) {
            let rest = &uri[ENTITY_SCHEME.len() + 3..];
            let (kind, id) = rest.split_once('/')?;
            if kind.is_empty() || id.is_empty() {
                return None;
            }
            return Some(Self { uri: uri.to_string() });
        }
        if uri.starts_with(&format!("{ARTIFACT_SCHEME}://")) {
            let rest = &uri[ARTIFACT_SCHEME.len() + 3..];
            let (scheme_kind, id) = rest.split_once('/')?;
            if scheme_kind.is_empty() || id.is_empty() {
                return None;
            }
            return Some(Self { uri: uri.to_string() });
        }
        None
    }
}

impl fmt::Display for GlobalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.uri)
    }
}

impl FromStr for GlobalId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self { uri: s.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_ids_are_uri_shaped_and_kind_aware() {
        let person = GlobalId::entity("person", "d3f...");
        assert_eq!(person.to_string(), "entity://person/d3f...");
        assert_eq!(person.kind(), Some("person"));

        let model = GlobalId::artifact_blake3("ab31f9...");
        assert!(model.is_artifact());
        assert!(model.kind().is_none());
    }
}