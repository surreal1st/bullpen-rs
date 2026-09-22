//! S12-07: `purchase` tool spec — logic lives in `crate::purchasing`.

use model::ToolSpec;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "purchase".to_string(),
        description: "Buy something on your own virtual card. This always asks Josh first - it cannot be set to always-allow, by him or by a rule - and is refused outright, before he is even asked, if it would put this month's spending over your limit.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "merchant": { "type": "string", "description": "The merchant's name, as it will show up on the card charge." },
                "amount_usd": { "type": "number", "description": "The amount, in US dollars." },
                "reason": { "type": "string", "description": "Why you want to buy this." }
            },
            "required": ["merchant", "amount_usd", "reason"]
        }),
    }
}
