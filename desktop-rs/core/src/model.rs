//! Wire DTOs for the library / sync JSON exchanged with the server. Field names
//! are snake_case to match the server's JSON directly. Used by the replica's
//! seed/reconcile path and by the apiclient.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryEpisode {
    pub id: String,
    pub relative_path: String,
    pub position: i64,
    pub updated_at: String,
    #[serde(default)]
    pub watched_at: Option<String>,
    #[serde(default)]
    pub resume_pos: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryShow {
    pub id: String,
    pub playlist: String,
    pub name: String,
    pub root_path: String,
    pub updated_at: String,
    #[serde(default)]
    pub date_added: Option<String>,
    #[serde(default)]
    pub removed_at: Option<String>,
    #[serde(default)]
    pub episodes: Vec<LibraryEpisode>,
}
