//! S11-02: SQL filter for per-person list queries. NULL `user_id` rows belong
//! to the owner (`COALESCE(user_id, owner_id) = caller_id`).

/// When present, list queries only return rows owned by this person.
#[derive(Debug, Clone)]
pub struct ListScope {
    pub owner_id: String,
    pub user_id: String,
}

impl ListScope {
    /// Append to a WHERE clause (includes leading `AND`). Two bind params:
    /// owner id, then effective user id — same order as TS `scopeParams`.
    pub fn and_sql(&self, table_alias: &str) -> String {
        format!(
            " AND COALESCE({table_alias}.user_id, ?) = ?",
            table_alias = table_alias
        )
    }

    pub fn bind_values(&self) -> (&str, &str) {
        (&self.owner_id, &self.user_id)
    }
}
