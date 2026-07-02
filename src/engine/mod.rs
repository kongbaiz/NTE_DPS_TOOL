//! Core capture → parse → model pipeline: Npcap capture and replay, UE
//! transport-layer decoding, damage/skill parsing, the combat domain model and
//! the abyss static dataset.

pub mod abyss_data;
pub mod capture;
pub mod model;
pub mod parser;
pub mod protocol;
