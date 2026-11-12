//! The search query language.
//!
//! ```text
//! lease timeout              every word must match (title, description, labels,
//!                            criteria, key or comments)
//! "lease timeout"            an exact phrase
//! sched*                     any word starting with "sched"
//! retry OR backoff           either one (OR must be upper case)
//! -flaky  -"known issue"     exclude matches
//! state:ready kind:bug       structured filters, combined with AND
//! assignee:none  is:actionable
//! label:"needs review"       quote a value that contains spaces
//! ```
//!
//! Qualifiers: `project`, `kind`, `state`, `priority`, `assignee` (`none` for
//! unassigned), `label`, `sprint`, `parent`, and `is:actionable` / `is:unassigned`.
//! Each may appear once.
//!
//! Text is compiled to FTS5, but user input is never passed to it raw: every term is
//! quoted, so punctuation such as the dash in `AG-12` cannot be misread as an operator
//! or raise a syntax error. An unknown `word:` qualifier is rejected rather than
//! silently searched for; quote it (`"error:timeout"`) to search for the text.
use crate::{IssueFilter, IssueKind, Priority, State};

const MAX_LENGTH: usize = 1000;
const MAX_TERMS: usize = 32;
const QUALIFIERS: &str = "project, kind, state, priority, assignee, label, sprint, parent, is";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub filter: IssueFilter,
    /// An FTS5 `MATCH` expression, absent for a query with only qualifiers.
    pub fts: Option<String>,
}

#[derive(Debug)]
enum Token {
    Word {
        text: String,
        prefix: bool,
        negated: bool,
    },
    Phrase {
        text: String,
        negated: bool,
    },
    Or,
    Qualifier {
        name: String,
        value: String,
    },
}

fn err(message: impl Into<String>) -> String {
    message.into()
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = vec![];
    let mut i = 0;
    let quoted = |i: &mut usize| -> Result<String, String> {
        // `*i` is on the opening quote.
        let start = *i + 1;
        let mut end = start;
        while end < chars.len() && chars[end] != '"' {
            end += 1;
        }
        if end >= chars.len() {
            return Err(err("unterminated quote: add a closing \""));
        }
        *i = end + 1;
        Ok(chars[start..end].iter().collect())
    };
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let negated = chars[i] == '-' && chars.get(i + 1).is_some_and(|c| !c.is_whitespace());
        if negated {
            i += 1;
        }
        if chars[i] == '"' {
            let text = quoted(&mut i)?;
            tokens.push(Token::Phrase { text, negated });
            continue;
        }
        let mut bare = String::new();
        let mut value = None;
        while i < chars.len() && !chars[i].is_whitespace() {
            if chars[i] == '"' && bare.ends_with(':') && !negated {
                value = Some(quoted(&mut i)?);
                break;
            }
            bare.push(chars[i]);
            i += 1;
        }
        if let Some(value) = value {
            let name = bare.trim_end_matches(':').to_lowercase();
            tokens.push(Token::Qualifier { name, value });
        } else if let Some((name, value)) = bare.split_once(':').filter(|(n, _)| !n.is_empty()) {
            if negated {
                return Err(err(format!(
                    "'-{bare}': exclusion works on words and phrases, not on {name}: filters"
                )));
            }
            tokens.push(Token::Qualifier {
                name: name.to_lowercase(),
                value: value.to_owned(),
            });
        } else if bare == "OR" && !negated {
            tokens.push(Token::Or);
        } else {
            let prefix = bare.len() > 1 && bare.ends_with('*');
            let text = if prefix {
                bare[..bare.len() - 1].to_owned()
            } else {
                bare
            };
            tokens.push(Token::Word {
                text,
                prefix,
                negated,
            });
        }
    }
    Ok(tokens)
}

fn searchable(text: &str) -> Result<(), String> {
    if text.chars().any(char::is_alphanumeric) {
        Ok(())
    } else {
        Err(err(format!(
            "'{text}' contains nothing searchable (no letters or digits)"
        )))
    }
}
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}
/// Enum values are written as in the JSON API, but `in-progress` is accepted too.
fn choice<T: serde::de::DeserializeOwned>(
    name: &str,
    value: &str,
    allowed: &str,
) -> Result<T, String> {
    let normalized = value.to_lowercase().replace('-', "_");
    serde_json::from_value(serde_json::Value::String(normalized))
        .map_err(|_| err(format!("{name}:{value} is not valid; use one of {allowed}")))
}
fn once<T>(slot: &mut Option<T>, name: &str, value: T) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(err(format!("{name}: was given more than once")));
    }
    Ok(())
}

pub fn parse(input: &str) -> Result<Parsed, String> {
    if input.len() > MAX_LENGTH {
        return Err(err(format!("query is longer than {MAX_LENGTH} bytes")));
    }
    // A NUL ends the string early inside SQLite, leaving a quote unterminated.
    if input.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return Err(err("query contains a control character"));
    }
    let tokens = tokenize(input)?;
    if tokens.len() > MAX_TERMS {
        return Err(err(format!("query has more than {MAX_TERMS} terms")));
    }
    let mut filter = IssueFilter::default();
    let (mut kind, mut state, mut priority) = (None::<IssueKind>, None::<State>, None::<Priority>);
    let (mut project, mut assignee, mut label) = (None, None, None);
    let (mut sprint, mut parent) = (None, None);
    // Positive terms, with OR as a marker; exclusions are collected apart.
    let mut positive: Vec<String> = vec![];
    let mut excluded: Vec<String> = vec![];
    // Whether the previous item was a term, so OR must sit between two terms.
    let mut after_term = false;
    let mut pending_or = false;

    for token in tokens {
        match token {
            Token::Qualifier { name, value } => {
                if value.is_empty() {
                    return Err(err(format!("{name}: needs a value")));
                }
                match name.as_str() {
                    "project" => once(&mut project, "project", value)?,
                    "kind" => once(
                        &mut kind,
                        "kind",
                        choice("kind", &value, "epic, story, task, bug, subtask, spike")?,
                    )?,
                    "state" => once(
                        &mut state,
                        "state",
                        choice(
                            "state",
                            &value,
                            "backlog, ready, in_progress, blocked, in_review, done, canceled",
                        )?,
                    )?,
                    "priority" => once(
                        &mut priority,
                        "priority",
                        choice("priority", &value, "critical, high, medium, low")?,
                    )?,
                    "assignee" => once(&mut assignee, "assignee", value)?,
                    "label" => once(&mut label, "label", value)?,
                    "sprint" => once(&mut sprint, "sprint", value)?,
                    "parent" => once(&mut parent, "parent", value)?,
                    "is" => match value.to_lowercase().as_str() {
                        "actionable" => filter.actionable = true,
                        "unassigned" => filter.unassigned = true,
                        _ => {
                            return Err(err(format!(
                                "is:{value} is not valid; use is:actionable or is:unassigned"
                            )));
                        }
                    },
                    other => {
                        return Err(err(format!(
                            "unknown filter '{other}:'. Filters are {QUALIFIERS}. To search for the text itself, quote it: \"{other}:{value}\""
                        )));
                    }
                }
            }
            Token::Or => {
                if !after_term || pending_or {
                    return Err(err("OR must sit between two search terms"));
                }
                pending_or = true;
                after_term = false;
            }
            Token::Word {
                text,
                prefix,
                negated,
            } => {
                searchable(&text)?;
                let term = if prefix {
                    format!("{}*", quote(&text))
                } else {
                    quote(&text)
                };
                add(
                    term,
                    negated,
                    &mut positive,
                    &mut excluded,
                    &mut after_term,
                    &mut pending_or,
                )?;
            }
            Token::Phrase { text, negated } => {
                searchable(&text)?;
                add(
                    quote(&text),
                    negated,
                    &mut positive,
                    &mut excluded,
                    &mut after_term,
                    &mut pending_or,
                )?;
            }
        }
    }
    if pending_or {
        return Err(err("OR must sit between two search terms"));
    }
    if positive.is_empty() && !excluded.is_empty() {
        return Err(err("excluding words needs at least one word to search for"));
    }
    filter.kind = kind;
    filter.state = state;
    filter.priority = priority;
    filter.project = project;
    filter.label = label;
    filter.sprint = sprint;
    filter.parent = parent;
    match assignee.as_deref() {
        Some("none" | "unassigned") => filter.unassigned = true,
        Some(_) => filter.assignee = assignee,
        None => {}
    }
    if filter.unassigned && filter.assignee.is_some() {
        return Err(err("assignee: and is:unassigned cannot be combined"));
    }
    let fts = (!positive.is_empty()).then(|| {
        let matches = positive.join(" ");
        if excluded.is_empty() {
            matches
        } else {
            format!("({matches}) NOT ({})", excluded.join(" OR "))
        }
    });
    Ok(Parsed { filter, fts })
}

fn add(
    term: String,
    negated: bool,
    positive: &mut Vec<String>,
    excluded: &mut Vec<String>,
    after_term: &mut bool,
    pending_or: &mut bool,
) -> Result<(), String> {
    if negated {
        if *pending_or {
            return Err(err("OR cannot be combined with an excluded word"));
        }
        excluded.push(term);
        return Ok(());
    }
    if *pending_or {
        positive.push("OR".into());
        *pending_or = false;
    }
    positive.push(term);
    *after_term = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts(q: &str) -> String {
        parse(q).unwrap().fts.unwrap()
    }
    fn fails(q: &str) -> String {
        parse(q).unwrap_err()
    }

    #[test]
    fn words_phrases_and_prefixes_are_always_quoted() {
        assert_eq!(fts("lease timeout"), r#""lease" "timeout""#);
        assert_eq!(fts(r#""lease timeout""#), r#""lease timeout""#);
        assert_eq!(fts("sched*"), r#""sched"*"#);
        // The dash in an issue key would be an operator in raw FTS5.
        assert_eq!(fts("AG-12"), r#""AG-12""#);
        assert_eq!(fts(r#"it's a"b"#), r#""it's" "a""b""#);
    }
    #[test]
    fn fts_operators_in_user_text_are_plain_words() {
        // Unquoted, these would be operators or column filters in raw FTS5.
        assert_eq!(fts("a AND b"), r#""a" "AND" "b""#);
        assert_eq!(fts("NOT a"), r#""NOT" "a""#);
        assert_eq!(fts("NEAR(a b)"), r#""NEAR(a" "b)""#);
        assert_eq!(fts("^a {b}"), r#""^a" "{b}""#);
    }
    #[test]
    fn or_binds_looser_than_implicit_and() {
        assert_eq!(fts("retry OR backoff"), r#""retry" OR "backoff""#);
        assert_eq!(fts("a b OR c"), r#""a" "b" OR "c""#);
        assert_eq!(fts("or"), r#""or""#, "lower-case or is a word");
    }
    #[test]
    fn exclusions_become_not() {
        assert_eq!(fts("lease -flaky"), r#"("lease") NOT ("flaky")"#);
        assert_eq!(
            fts(r#"lease -flaky -"known issue""#),
            r#"("lease") NOT ("flaky" OR "known issue")"#
        );
        assert_eq!(fts("-flaky lease"), r#"("lease") NOT ("flaky")"#);
        // A dash inside a word is part of the word, never an exclusion.
        assert_eq!(fts("well-known"), r#""well-known""#);
        assert!(
            fails("a - b").contains("nothing searchable"),
            "a lone dash has no text"
        );
    }
    #[test]
    fn qualifiers_fill_the_filter_and_leave_no_text() {
        let p =
            parse("state:ready kind:bug priority:high project:AG label:rust sprint:s1 parent:AG-1")
                .unwrap();
        assert_eq!(p.fts, None);
        assert_eq!(p.filter.state, Some(State::Ready));
        assert_eq!(p.filter.kind, Some(IssueKind::Bug));
        assert_eq!(p.filter.priority, Some(Priority::High));
        assert_eq!(p.filter.project.as_deref(), Some("AG"));
        assert_eq!(p.filter.label.as_deref(), Some("rust"));
        assert_eq!(p.filter.sprint.as_deref(), Some("s1"));
        assert_eq!(p.filter.parent.as_deref(), Some("AG-1"));
        let p = parse("STATE:In-Progress assignee:forge").unwrap();
        assert_eq!(
            p.filter.state,
            Some(State::InProgress),
            "names and values are case-insensitive"
        );
        assert_eq!(p.filter.assignee.as_deref(), Some("forge"));
    }
    #[test]
    fn none_unassigned_and_actionable() {
        assert!(parse("assignee:none").unwrap().filter.unassigned);
        assert!(parse("is:unassigned").unwrap().filter.unassigned);
        assert!(parse("is:actionable lease").unwrap().filter.actionable);
        assert!(fails("assignee:forge is:unassigned").contains("cannot be combined"));
    }
    #[test]
    fn qualifiers_mix_with_text_and_accept_quoted_values() {
        let p = parse(r#"lease label:"needs review" state:ready timeout"#).unwrap();
        assert_eq!(p.fts.as_deref(), Some(r#""lease" "timeout""#));
        assert_eq!(p.filter.label.as_deref(), Some("needs review"));
    }
    #[test]
    fn mistakes_are_reported_in_plain_words() {
        assert!(fails("statee:ready").contains("unknown filter 'statee:'"));
        assert!(
            fails("statee:ready").contains("\"statee:ready\""),
            "tells you how to search for the text"
        );
        assert!(fails("state:wip").contains("backlog, ready"));
        assert!(fails("kind:").contains("needs a value"));
        assert!(fails("state:ready state:done").contains("more than once"));
        assert!(fails(r#""open quote"#).contains("unterminated"));
        assert!(fails("a OR").contains("OR must sit between"));
        assert!(fails("OR a").contains("OR must sit between"));
        assert!(fails("a OR OR b").contains("OR must sit between"));
        assert!(fails("a OR -b").contains("OR cannot be combined"));
        assert!(fails("-flaky").contains("needs at least one word"));
        assert!(fails("-state:done lease").contains("exclusion works on words"));
        assert!(fails("---").contains("nothing searchable"));
        assert!(fails("is:done").contains("is:actionable"));
        assert!(fails(&"a ".repeat(40)).contains("more than 32"));
        assert!(fails(&"a".repeat(1001)).contains("longer than"));
        assert!(fails("a\u{0}b").contains("control character"));
        assert!(fails("a\u{7}").contains("control character"));
        assert!(
            parse("a\tb\nc").is_ok(),
            "tabs and newlines are just whitespace"
        );
    }
    #[test]
    fn quoted_qualifier_text_is_searchable_text() {
        assert_eq!(fts(r#""error:timeout""#), r#""error:timeout""#);
    }
    #[test]
    fn unicode_is_preserved() {
        assert_eq!(fts("café naïve"), r#""café" "naïve""#);
        assert_eq!(fts("日本語"), r#""日本語""#);
    }
    #[test]
    fn empty_and_blank_queries_match_everything_without_text() {
        let p = parse("   ").unwrap();
        assert_eq!((p.fts, p.filter.project), (None, None));
    }
}
