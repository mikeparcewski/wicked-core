//! (domain-graph-3) A BOUNDED LOOP over a run's own steps: a Tool phase that declares
//! [`repeat_from`](crate::workflow::PhaseDef::repeat_from) decides, each time it passes, whether
//! the run goes back to that earlier phase for another pass.
//!
//! The decision is the command's, on the host: its output's LAST line that starts with
//! [`MARKER`] carries one compact JSON object,
//!
//! ```json
//! {"next": "continue", "line": "Mapping wicked-crew · 3% → 12% · target 80% · page 2"}
//! {"next": "ask", "line": "…budget reached…",
//!  "choices": [{"id": "continue", "label": "Map 12 more pages"},
//!              {"id": "lower", "label": "Lower the target to 23% for wicked-crew"}],
//!  "recommended": "continue"}
//! {"next": "done", "line": "…"}
//! ```
//!
//! * `continue`: every unit from the target phase through this one is re-armed at a fresh attempt
//!   and the run goes on from the target (`stepRepeated` names the pass and the line);
//! * `ask`: the run stops HERE at a `repeat` gate offering the command's choices (answer tokens
//!   `consent:<id>`, the choice grammar the consent gate introduced, so every door that carries a
//!   consent choice carries these) and `reject` (stop: the run is cancelled, what it did is kept).
//!   A choice re-runs THIS command with [`CHOICE_ENV`] set, and its new line decides again;
//! * `done`, no line, or a line that does not parse: the run moves on as it always did.
//!
//! The engine never runs a pass past [`MAX_PASSES`], whatever the command says. Only a Tool unit
//! whose def declares the loop is read: an agent's output never steers it (a seat could print the
//! marker), and a plain Tool unit's output is never parsed.

use serde::Deserialize;

/// The prefix of the decision line (one space, then the JSON object).
pub const MARKER: &str = "wicked-repeat ";

/// The gate kind of the operator's ask (`awaitingHuman.gateKind`, the interaction row).
pub const GATE_KIND: &str = "repeat";

/// The choice the operator made at the ask, handed to the re-run command.
pub const CHOICE_ENV: &str = "WICKED_REPEAT_CHOICE";

/// The pass the command is deciding after (1 on the first), handed to every run of the command.
pub const PASS_ENV: &str = "WICKED_REPEAT_PASS";

/// The most passes one loop of one run makes: the command's own budget is the one an operator
/// sets; this is the engine's backstop against a command that never stops asking.
pub const MAX_PASSES: u32 = 200;

/// The longest progress line kept (characters): it rides an event and a gate prompt.
pub const MAX_LINE_CHARS: usize = 400;

/// The most choices an ask offers (the operator reads them as buttons; `reject` comes on top).
pub const MAX_CHOICES: usize = 4;

/// What the command decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Continue,
    Ask,
    Done,
}

/// One answer an ask offers besides stop.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

/// The parsed decision line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub next: Next,
    /// The command's progress sentence (trimmed, one line, at most [`MAX_LINE_CHARS`]).
    pub line: String,
    /// The ask's choices (empty unless `next` is [`Next::Ask`]).
    pub choices: Vec<Choice>,
    /// The id of the choice the command recommends (one of `choices`), if any.
    pub recommended: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    next: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    recommended: Option<String>,
}

/// A choice id rides an environment variable and an answer token: a short token only.
fn ok_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// One line of text: control characters become spaces, clipped on a char boundary.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let t = flat.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// The decision in a Tool unit's output: the LAST line that starts with [`MARKER`]. `None` when
/// there is none or it does not parse (the run moves on); an `ask` with no usable choice is still
/// an ask (the operator can stop, or approve, which re-runs the command with no choice).
pub fn parse_signal(output: &str) -> Option<Signal> {
    let raw = output
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix(MARKER))?;
    let r: Raw = serde_json::from_str(raw.trim()).ok()?;
    let next = match r.next.as_str() {
        "continue" => Next::Continue,
        "ask" => Next::Ask,
        "done" => Next::Done,
        _ => return None,
    };
    let mut choices: Vec<Choice> = Vec::new();
    if next == Next::Ask {
        for c in r.choices {
            if ok_id(&c.id) && !choices.iter().any(|k| k.id == c.id) {
                choices.push(Choice {
                    label: one_line(&c.label, 120),
                    id: c.id,
                });
            }
            if choices.len() == MAX_CHOICES {
                break;
            }
        }
    }
    let recommended = r
        .recommended
        .filter(|id| choices.iter().any(|c| &c.id == id));
    Some(Signal {
        next,
        line: one_line(&r.line, MAX_LINE_CHARS),
        choices,
        recommended,
    })
}

/// The answer token of a choice (`consent:<id>`).
pub fn token(id: &str) -> String {
    format!("{}{id}", crate::consent_plan::CHOICE_PREFIX)
}

/// The choice an answer token names at an ask, or `None`.
pub fn choice_for_token<'a>(signal: &'a Signal, token: &str) -> Option<&'a Choice> {
    let id = token.strip_prefix(crate::consent_plan::CHOICE_PREFIX)?;
    signal.choices.iter().find(|c| c.id == id)
}

/// The choice a plain approve takes: the recommended one, else the first.
pub fn default_choice(signal: &Signal) -> Option<&Choice> {
    signal
        .recommended
        .as_deref()
        .and_then(|id| signal.choices.iter().find(|c| c.id == id))
        .or_else(|| signal.choices.first())
}

/// The gate prompt of an ask: the command's line, then every answer.
pub fn ask_prompt(signal: &Signal, ord: u32) -> String {
    let mut s = if signal.line.is_empty() {
        format!("Step {ord} asks before it goes on.")
    } else {
        signal.line.clone()
    };
    if !signal.choices.is_empty() {
        s.push_str(" Choose one:");
        for c in &signal.choices {
            let rec = if default_choice(signal).is_some_and(|d| d.id == c.id) {
                " (recommended)"
            } else {
                ""
            };
            s.push_str(&format!(" \u{2022} {}{rec}.", c.label));
        }
    }
    if signal.choices.is_empty() {
        // (agy r1 on #1226) An ask with no choice of its own is answered approve (the step runs
        // again) or stop — say both.
        s.push_str(" \u{2022} Approve: the step runs again.");
    }
    s.push_str(" \u{2022} Stop here: the run ends and what it did so far is kept.");
    s
}

/// The fields an ask adds to `awaitingHuman`: `choices` (the answer tokens, `reject` last),
/// `recommended` (the default's index; absent when the step offered no choice), `choiceLabels`
/// (per token) and `repeatLine`.
pub fn event_fields(signal: &Signal) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::json;
    let mut m = serde_json::Map::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut labels = serde_json::Map::new();
    for c in &signal.choices {
        let t = token(&c.id);
        labels.insert(t.clone(), json!(c.label));
        tokens.push(t);
    }
    labels.insert("reject".into(), json!("Stop here"));
    tokens.push("reject".into());
    m.insert("choices".into(), json!(tokens));
    // The step's recommendation, when it offered choices; never `reject` by default (agy r1).
    if let Some(i) =
        default_choice(signal).and_then(|d| signal.choices.iter().position(|c| c.id == d.id))
    {
        m.insert("recommended".into(), json!(i));
    }
    m.insert("choiceLabels".into(), serde_json::Value::Object(labels));
    m.insert("repeatLine".into(), json!(signal.line));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_marker_line_decides_and_other_lines_are_ignored() {
        let out = "[extract-next] something\nwicked-repeat {\"next\":\"ask\",\"line\":\"old\"}\n\
                   noise\nwicked-repeat {\"next\":\"continue\",\"line\":\"Mapping x \u{b7} 3% \u{2192} 12%\"}\n\
                   [stderr] trailing";
        let s = parse_signal(out).unwrap();
        assert_eq!(s.next, Next::Continue);
        assert_eq!(s.line, "Mapping x \u{b7} 3% \u{2192} 12%");
        assert!(s.choices.is_empty(), "choices ride an ask only");
    }

    #[test]
    fn no_line_a_bad_object_or_an_unknown_next_moves_on() {
        assert_eq!(parse_signal("all done\n"), None);
        assert_eq!(parse_signal("wicked-repeat not json"), None);
        assert_eq!(parse_signal("wicked-repeat {\"next\":\"forever\"}"), None);
        let done = parse_signal("wicked-repeat {\"next\":\"done\"}").unwrap();
        assert_eq!(done.next, Next::Done);
        assert_eq!(done.line, "");
    }

    #[test]
    fn an_ask_keeps_well_formed_distinct_choices_up_to_the_cap() {
        let out = r#"wicked-repeat {"next":"ask","line":"budget\nreached","choices":[{"id":"continue","label":"Map 12 more"},{"id":"bad id","label":"x"},{"id":"continue","label":"dup"},{"id":"lower","label":"Lower"},{"id":"a","label":"a"},{"id":"b","label":"b"},{"id":"c","label":"c"}],"recommended":"nope"}"#;
        let s = parse_signal(out).unwrap();
        assert_eq!(s.next, Next::Ask);
        assert_eq!(
            s.line, "budget reached",
            "a control character is never kept"
        );
        let ids: Vec<&str> = s.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["continue", "lower", "a", "b"]);
        assert_eq!(
            s.recommended, None,
            "a recommendation must name an offered choice"
        );
        assert_eq!(default_choice(&s).unwrap().id, "continue");
        assert_eq!(
            choice_for_token(&s, "consent:lower").unwrap().label,
            "Lower"
        );
        assert!(choice_for_token(&s, "consent:bad id").is_none());
        assert!(choice_for_token(&s, "lower").is_none());
        let f = event_fields(&s);
        assert_eq!(
            f["choices"],
            serde_json::json!([
                "consent:continue",
                "consent:lower",
                "consent:a",
                "consent:b",
                "reject"
            ])
        );
        assert_eq!(f["recommended"], 0);
        assert_eq!(f["choiceLabels"]["reject"], "Stop here");
        let p = ask_prompt(&s, 7);
        assert!(p.starts_with("budget reached Choose one:"), "{p}");
        assert!(p.contains("Map 12 more (recommended)") && p.ends_with("is kept."));
    }

    #[test]
    fn an_ask_with_no_choice_recommends_nothing_and_says_approve_runs_it_again() {
        let s = parse_signal(r#"wicked-repeat {"next":"ask","line":"pause"}"#).unwrap();
        let f = event_fields(&s);
        assert_eq!(f["choices"], serde_json::json!(["reject"]));
        assert!(
            f.get("recommended").is_none(),
            "stop is never the recommendation"
        );
        assert!(ask_prompt(&s, 3).contains("Approve: the step runs again."));
    }

    #[test]
    fn a_long_line_is_clipped_on_a_char_boundary() {
        let long = "\u{e9}".repeat(MAX_LINE_CHARS + 50);
        let out = format!("wicked-repeat {{\"next\":\"continue\",\"line\":\"{long}\"}}");
        let s = parse_signal(&out).unwrap();
        assert_eq!(s.line.chars().count(), MAX_LINE_CHARS);
        assert!(s.line.ends_with('\u{2026}'));
    }
}
