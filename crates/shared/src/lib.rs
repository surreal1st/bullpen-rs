//! Types both halves of Bullpen use. Port of `src/shared` in the TS original.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bot {
    pub id: String,
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    pub model: Option<String>,
    pub archived: bool,
    pub has_routine: bool,
    pub section_id: Option<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub avatar: Option<String>,
    pub shape: Option<String>,
    pub effort: Effort,
    pub is_template: bool,
    pub voice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub id: String,
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    pub model: Option<String>,
    pub archived: bool,
    pub has_routine: bool,
    pub section_id: Option<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub avatar: Option<String>,
    pub shape: Option<String>,
    pub effort: Effort,
    pub is_template: bool,
    pub voice: Option<String>,
    pub unread: u32,
    pub preview: String,
    pub last_at: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(&self) -> &str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

impl FromStr for Effort {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "low" => Effort::Low,
            "high" => Effort::High,
            _ => Effort::Medium,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub id: String,
    pub name: String,
    pub position: i32,
}
