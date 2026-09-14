//! Roster JSON shape from `/api/roster`. Kept local to `client` rather than
//! `crates/shared` - S0-05 is not allowed to touch `shared` (another
//! builder owns `store`/`server`/`model`), and the real shared type lands
//! whenever S0-04 does.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(rename = "sectionId", default)]
    pub section_id: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Roster {
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub bots: Vec<Bot>,
}
