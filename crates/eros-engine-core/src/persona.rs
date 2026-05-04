// SPDX-License-Identifier: AGPL-3.0-only
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaGenome {
    pub id: Uuid,
    pub name: String,
    pub system_prompt: String,
    pub tip_personality: Option<String>,
    pub avatar_url: Option<String>,
    pub art_metadata: Value, // JSONB: gender/age/mbti/backstory/speech_style/quirks/topics/model
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaInstance {
    pub id: Uuid,
    pub genome_id: Uuid,
    pub owner_uid: Uuid,
    pub status: String,
}

/// Joined view used by the engine pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionPersona {
    pub instance_id: Uuid,
    pub genome: PersonaGenome,
    pub instance: PersonaInstance,
}
