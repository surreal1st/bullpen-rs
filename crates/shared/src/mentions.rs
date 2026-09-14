//! Pulling another bot into a conversation by naming it. Port of
//! `src/shared/mentions.ts`.
//!
//! 🔴 Josh asked for group chats, which x.ai describe as "several named Bots
//! coordinating". Bullpen's whole model is one conversation per bot - the
//! rail, the routing, the prompt and the run all assume an owner - so a true
//! group conversation is surgery on every one of those.
//!
//! This is the contained version, and it buys most of the value: in any
//! chat, `@someone` sends that turn to that bot instead, in the same thread,
//! with the history everyone can see. No new conversation type, no rail
//! change, no schema for membership.
//!
//! 🔴 A mention REPLACES the responder rather than adding one. "@jason what
//! do you think" in Riley's chat means Josh wants Jason - having Riley
//! answer too would double the cost of every message and put an unasked-for
//! opinion above the one he asked for. That replacement decision lives at
//! the call site (`routes::messages`), not here - this module only finds
//! names.

/// Every `@name` found in a message. Port of the TS `Mentions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mentions {
    /// Every name found, in the order written, without duplicates, lowercased.
    pub names: Vec<String>,
    /// The message with the mentions left IN.
    ///
    /// 🔴 Deliberately unchanged. Stripping "@jason" would hand Jason a
    /// message that reads as though it were always his, and hide from
    /// everyone else in the transcript why he answered. The bot is told
    /// separately that it was named.
    pub text: String,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A bare `@name` at a word boundary. Names are the ids bots already have.
/// Hand-rolled rather than a regex dependency - `[a-zA-Z][\w-]{0,63}` after
/// an `@` not itself preceded by a word character or another `@` (so
/// `josh@example.com` is not a mention: the char before `@` is `h`, a word
/// character).
pub fn find_mentions(text: &str) -> Mentions {
    let chars: Vec<char> = text.chars().collect();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '@' {
            let boundary_ok = match i.checked_sub(1).and_then(|p| chars.get(p)) {
                None => true,
                Some(&prev) => !is_word_char(prev) && prev != '@',
            };
            let first_name_char = chars.get(i + 1).copied();
            if boundary_ok && first_name_char.is_some_and(|c| c.is_ascii_alphabetic()) {
                let start = i + 1;
                let mut end = start + 1;
                while end < chars.len()
                    && end - start < 64
                    && (is_word_char(chars[end]) || chars[end] == '-')
                {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                let lower = name.to_lowercase();
                if !names.contains(&lower) {
                    names.push(lower);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }

    Mentions {
        names,
        text: text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_one() {
        assert_eq!(
            find_mentions("@jason what do you think?").names,
            vec!["jason"]
        );
    }

    #[test]
    fn finds_one_mid_sentence() {
        assert_eq!(
            find_mentions("I think @morgan should see this").names,
            vec!["morgan"]
        );
    }

    #[test]
    fn keeps_the_order_and_drops_repeats() {
        assert_eq!(
            find_mentions("@jason and @morgan, and @jason again").names,
            vec!["jason", "morgan"]
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(find_mentions("@Jason").names, vec!["jason"]);
    }

    #[test]
    fn takes_hyphens_because_bot_ids_have_them() {
        assert_eq!(
            find_mentions("@cost-probe run it").names,
            vec!["cost-probe"]
        );
    }

    #[test]
    fn does_not_treat_an_email_address_as_a_mention() {
        assert_eq!(
            find_mentions("mail me at josh@example.com").names,
            Vec::<String>::new()
        );
        assert_eq!(
            find_mentions("projectreality@gmail.com").names,
            Vec::<String>::new()
        );
    }

    #[test]
    fn does_not_fire_on_a_bare_at_or_a_number() {
        assert_eq!(find_mentions("meet @ 5").names, Vec::<String>::new());
        assert_eq!(
            find_mentions("@2026 was a year").names,
            Vec::<String>::new()
        );
        assert_eq!(find_mentions("email@").names, Vec::<String>::new());
    }

    #[test]
    fn finds_nothing_in_ordinary_text() {
        assert_eq!(
            find_mentions("Check the watchlist and tell me who is live.").names,
            Vec::<String>::new()
        );
    }

    #[test]
    fn leaves_the_message_exactly_as_written() {
        let text = "@jason what do you think?";
        assert_eq!(find_mentions(text).text, text);
    }

    #[test]
    fn handles_a_mention_at_the_very_start_and_end() {
        assert_eq!(find_mentions("@a").names, vec!["a"]);
        assert_eq!(find_mentions("ask @b").names, vec!["b"]);
    }
}
