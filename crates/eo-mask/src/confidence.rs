//! Mask confidence reporting — used to tag a provider's overall reliability.

use serde::{Deserialize, Serialize};

/// Self-reported quality of a cloud mask result.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaskConfidence {
    /// Low — heuristic-only (e.g. simple QA bit decoding).
    Low,
    /// Medium — multi-band rule based (Fmask without ML).
    #[default]
    Medium,
    /// High — ML-derived (s2cloudless) on a model the provider trusts.
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_medium() {
        assert_eq!(MaskConfidence::default(), MaskConfidence::Medium);
    }

    #[test]
    fn ordering_via_match_arms() {
        // Just confirm all three variants are distinct.
        let s: std::collections::HashSet<MaskConfidence> = [
            MaskConfidence::Low,
            MaskConfidence::Medium,
            MaskConfidence::High,
        ]
        .into_iter()
        .collect();
        assert_eq!(s.len(), 3);
    }
}
