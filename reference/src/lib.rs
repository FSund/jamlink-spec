//! Reference implementation of the Jamlink 1.0 protocol (see `SPEC.md`).
//!
//! Start a node with [`start`], then drive it through the returned [`Handle`]
//! and react to the [`Notice`]s it sends:
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use jamlink::{start, Config, Entity, Notice};
//!
//! let (node, mut notices) = start(Config::new("My Game", "my_game")).await?;
//! let mut hero = Entity::new(node.new_entity_id(), "player", [0.0, 0.0, 0.0]);
//! hero.name = Some("Hero".into());
//! node.upsert(hero.clone());
//!
//! while let Some(notice) = notices.recv().await {
//!     match notice {
//!         Notice::EntityUpdated { entity, .. } => { /* draw the replica */ }
//!         Notice::Event { from, event } if event.event == "damage" => {
//!             // apply it to our entity, then upsert the new state
//!         }
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! The node does the protocol bookkeeping: discovery, the full mesh and its
//! dial rule, hello and snapshots, ownership checks, event routing and
//! timeouts, transfers, heartbeats and departures. It also checks everything
//! it receives and reports rule breaks as [`Notice::Violation`].

mod node;
pub mod proto;
pub mod schema;

pub use node::{Config, Handle, IncomingEvent, Notice, PeerInfo, Severity, Violation, start};
pub use proto::{DisplayHints, Entity};
