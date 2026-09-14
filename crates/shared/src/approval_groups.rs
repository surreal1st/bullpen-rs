//! Collapsing identical pending approvals into one decision. Port of
//! `src/shared/approvalGroups.ts`.
//!
//! Josh: *"Doesn't look like the dialogs got halved."* Each card HAD halved -
//! he was looking at two of them. His "Pres live ping" routine fires every 30
//! minutes and asks to run the same command each time, so at 48 firings a day
//! the approvals area grows without bound and eats the conversation, however
//! small one card is.
//!
//! 🔴 Grouped on bot + tool + ARGUMENTS, never on bot + tool alone. Two
//! `shell` calls from the same routine running different commands are two
//! different decisions, and folding them behind one Approve button would
//! have Josh approving a command he never saw.
//!
//! A pure, client-agnostic port: the TS original is used only from
//! `src/client/Approvals.tsx` (grouping is a display concern, not a server
//! one - `GET /api/approvals` itself answers the flat list, same as the TS
//! route), so this lives in `shared` for whichever side ends up rendering
//! the approvals pane.

use std::collections::HashMap;

/// What a type needs to be groupable: enough to build the `bot + tool +
/// args` key the TS original keys on.
pub trait Groupable {
    fn id(&self) -> &str;
    fn bot_id(&self) -> &str;
    fn tool_name(&self) -> &str;
    fn tool_args(&self) -> &str;
}

/// One group of identical pending requests.
#[derive(Debug, Clone)]
pub struct ApprovalGroup<T> {
    /// The most recent request in the group; what the card renders.
    pub head: T,
    /// Every pending id, in the order they arrived. Deciding the group
    /// decides all of them.
    pub ids: Vec<String>,
    pub count: usize,
}

/// Groups identical requests, keeping the order the list arrived in.
///
/// The list comes back newest-first (see `approvals::list_pending`), so the
/// first member of each group is the most recent - which is the one worth
/// showing, since an older duplicate tells Josh nothing the newer one does
/// not.
pub fn group_approvals<T: Groupable + Clone>(items: Vec<T>) -> Vec<ApprovalGroup<T>> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, ApprovalGroup<T>> = HashMap::new();

    for item in items {
        let key = format!(
            "{} {} {}",
            item.bot_id(),
            item.tool_name(),
            item.tool_args()
        );
        match groups.get_mut(&key) {
            Some(existing) => {
                existing.ids.push(item.id().to_string());
                existing.count += 1;
            }
            None => {
                order.push(key.clone());
                groups.insert(
                    key,
                    ApprovalGroup {
                        head: item.clone(),
                        ids: vec![item.id().to_string()],
                        count: 1,
                    },
                );
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Item {
        id: &'static str,
        bot_id: &'static str,
        tool_name: &'static str,
        tool_args: &'static str,
    }

    impl Groupable for Item {
        fn id(&self) -> &str {
            self.id
        }
        fn bot_id(&self) -> &str {
            self.bot_id
        }
        fn tool_name(&self) -> &str {
            self.tool_name
        }
        fn tool_args(&self) -> &str {
            self.tool_args
        }
    }

    #[test]
    fn collapses_identical_bot_tool_and_arguments_into_one_group() {
        let items = vec![
            Item {
                id: "a2",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "{\"command\":\"ls\"}",
            },
            Item {
                id: "a1",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "{\"command\":\"ls\"}",
            },
        ];

        let groups = group_approvals(items);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].head.id, "a2");
        assert_eq!(groups[0].ids, vec!["a2", "a1"]);
        assert_eq!(groups[0].count, 2);
    }

    #[test]
    fn different_arguments_never_fold_into_one_decision() {
        let items = vec![
            Item {
                id: "a1",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "{\"command\":\"ls\"}",
            },
            Item {
                id: "a2",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "{\"command\":\"rm -rf /work\"}",
            },
        ];

        let groups = group_approvals(items);

        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn preserves_the_order_groups_first_appeared_in() {
        let items = vec![
            Item {
                id: "a1",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "1",
            },
            Item {
                id: "b1",
                bot_id: "wren",
                tool_name: "shell",
                tool_args: "2",
            },
            Item {
                id: "a2",
                bot_id: "arthur",
                tool_name: "shell",
                tool_args: "1",
            },
        ];

        let groups = group_approvals(items);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].head.id, "a1");
        assert_eq!(groups[1].head.id, "b1");
    }
}
