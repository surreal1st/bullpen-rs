//! Attention counts for badges and `/api/attention`. Port of `roster.ts` `attentionCount`.

use crate::Db;
use crate::list_scope::ListScope;
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Attention {
    pub approvals: i64,
    pub unread: i64,
    pub total: i64,
}

pub fn attention_count(db: &Db, scope: Option<&ListScope>) -> rusqlite::Result<Attention> {
    let scope_sql = scope.map(|s| s.and_sql("b")).unwrap_or_default();
    let bind = scope.map(ListScope::bind_values);

    let approvals_sql = format!(
        "SELECT COUNT(*) FROM approvals a
           JOIN bots b ON b.id = a.bot_id
          WHERE a.status = 'pending'{scope_sql}"
    );
    let approvals: i64 = if let Some((owner, user)) = bind {
        db.conn()
            .query_row(&approvals_sql, params![owner, user], |row| row.get(0))?
    } else {
        db.conn().query_row(&approvals_sql, [], |row| row.get(0))?
    };

    let unread_sql = format!(
        "SELECT COUNT(*) FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           JOIN bots b ON b.id = c.bot_id
          WHERE m.role = 'assistant'
            AND c.kind != 'room'
            AND b.archived_at IS NULL
            AND b.hidden_at IS NULL
            AND m.created_at > COALESCE(b.last_seen_at, ''){scope_sql}"
    );
    let unread: i64 = if let Some((owner, user)) = bind {
        db.conn()
            .query_row(&unread_sql, params![owner, user], |row| row.get(0))?
    } else {
        db.conn().query_row(&unread_sql, [], |row| row.get(0))?
    };

    Ok(Attention {
        approvals,
        unread,
        total: approvals + unread,
    })
}
