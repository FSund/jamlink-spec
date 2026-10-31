//! Protocol constants and the entity type shared by all Jamlink messages.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Protocol version this crate speaks.
pub const VERSION: &str = "1.0";
/// Major version; peers with a different major are refused.
pub const MAJOR: u32 = 1;
/// UDP port for discovery beacons.
pub const BEACON_PORT: u16 = 45800;
/// Default WebSocket port range.
pub const WS_PORTS: std::ops::RangeInclusive<u16> = 45801..=45809;
/// WebSocket path.
pub const WS_PATH: &str = "/jamlink";
/// Largest message accepted, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The complete state of one entity, as carried by an `entity` message.
///
/// Optional fields that are `None` mean "the default", not "unchanged":
/// every `entity` message replaces the receiver's replica.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entity {
    pub id: String,
    /// Owner's peer ID. Filled in by the node for entities you own.
    #[serde(default)]
    pub owner: String,
    pub kind: String,
    /// Owner's clock in milliseconds. Filled in by the node for entities you own.
    #[serde(default)]
    pub t: u64,
    pub pos: [f64; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vel: Option<[f64; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rot: Option<[f64; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hp: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hp_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qty: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayHints>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext: Option<Map<String, Value>>,
}

/// Rendering hints that let any game draw any entity.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DisplayHints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Entity {
    /// A new entity with the required fields set.
    pub fn new(id: impl Into<String>, kind: impl Into<String>, pos: [f64; 3]) -> Self {
        Entity { id: id.into(), kind: kind.into(), pos, ..Default::default() }
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// Velocity, defaulting to zero.
    pub fn vel_or_default(&self) -> [f64; 3] {
        self.vel.unwrap_or([0.0; 3])
    }
    pub fn yaw_or_default(&self) -> f64 {
        self.yaw.unwrap_or(0.0)
    }
    pub fn radius_or_default(&self) -> f64 {
        self.radius.unwrap_or(0.5)
    }
    pub fn height_or_default(&self) -> f64 {
        self.height.unwrap_or(1.0)
    }
    pub fn faction_or_default(&self) -> &str {
        self.faction.as_deref().unwrap_or("neutral")
    }
    pub fn state_or_default(&self) -> &str {
        self.state.as_deref().unwrap_or("idle")
    }
    pub fn qty_or_default(&self) -> u64 {
        self.qty.unwrap_or(1)
    }

    /// The `entity` message for this entity.
    pub fn to_message(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("entity serializes");
        v.as_object_mut().expect("entity is an object").insert("type".into(), Value::from("entity"));
        v
    }
}

/// Parses the major number of a `"major.minor"` version string.
pub fn major_of(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

/// Generates a random 16-hex-character peer ID.
pub fn new_peer_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}
