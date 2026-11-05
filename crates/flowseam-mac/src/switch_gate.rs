//! Tap-side config and clock/cursor source. The switch decision itself lives in
//! `flowseam_core::forward::Forwarder`; the tap only samples.

use flowseam_core::config::Config;
use flowseam_core::proto::{Edge, Point};
use flowseam_core::chord::ChordError;
use flowseam_core::switch::SwitchPolicy;

/// Where the tap gets its time, cursor position and boundary test. Production reads the OS;
/// tests script it.
pub trait SwitchEnv: Send {
    fn now_ms(&self) -> u64;
    fn cursor(&self) -> Option<Point>;
}

/// What the tap needs from the user's config: the policy and the dwell zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapConfig {
    pub policy: SwitchPolicy,
    /// One entry per peer. Empty means no boundary zone, so only the chord can switch.
    pub edges: Vec<Edge>,
    pub boundary_px: i32,
}

impl Default for TapConfig {
    fn default() -> Self {
        Self { policy: SwitchPolicy::default(), edges: Vec::new(), boundary_px: 2 }
    }
}

impl TapConfig {
    pub fn from_config(cfg: &Config) -> Result<Self, ChordError> {
        Ok(Self {
            policy: SwitchPolicy::from_config(&cfg.switch)?,
            edges: cfg.peers.iter().map(|p| p.edge.into()).collect(),
            boundary_px: cfg.switch.boundary_px,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowseam_core::config::{Edge as CfgEdge, Peer};

    #[test]
    fn tap_config_from_config_maps_edges_policy_and_width() {
        let mut cfg = Config::default();
        cfg.switch.dwell_ms = 400;
        cfg.switch.boundary_px = 5;
        cfg.peers = vec![Peer {
            name: "mini".into(),
            addr: "192.168.1.2:24800".into(),
            edge: CfgEdge::Top,
            fingerprint: "ab".repeat(32),
            keys: Default::default(),
        }];
        let t = TapConfig::from_config(&cfg).unwrap();
        assert_eq!(t.edges, vec![Edge::Top]);
        assert_eq!(t.boundary_px, 5);
        assert_eq!(t.policy.dwell_ms, 400);
        cfg.switch.chord = "bogus".into();
        assert!(TapConfig::from_config(&cfg).is_err());
    }
}
