use crate::model::Hit;
use crate::model::stable_hit_uid;

#[derive(Clone, Debug)]
pub struct DamageHitFact {
    pub hit_uid: String,
    pub timestamp: f64,
    pub hp_before: f64,
    pub hp_after: f64,
    pub hp_reported_max: f64,
}

impl From<&Hit> for DamageHitFact {
    fn from(hit: &Hit) -> Self {
        Self {
            hit_uid: stable_hit_uid(hit),
            timestamp: hit.timestamp,
            hp_before: hit.target_hp_before,
            hp_after: hit.target_hp_after,
            hp_reported_max: hit.target_max_hp,
        }
    }
}
