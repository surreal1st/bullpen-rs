//! S12-08: `query_db` tool spec — logic lives in `crate::databases`.

use model::ToolSpec;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "query_db".to_string(),
        description: "Run a read-only SQL query against one of Josh's named database targets (Settings > Computer > Databases). Only a single SELECT (or WITH ... SELECT) reaches the database - anything else, or any table not on that target's own allow list, is refused before it runs. Capped at 1000 rows; pass limit for fewer. Results come back as a compact table.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The target's label, exactly as it reads in Settings." },
                "sql": {
                    "type": "string",
                    "description": "A single SELECT (or WITH ... SELECT) statement. No semicolons, comments, PRAGMA or ATTACH."
                },
                "limit": {
                    "type": "number",
                    "description": "How many rows to return, 1-1000. Default 200.",
                    "minimum": 1,
                    "maximum": 1000
                }
            },
            "required": ["target", "sql"]
        }),
    }
}
