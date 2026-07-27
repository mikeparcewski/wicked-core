//! Structured assumptions — the external-transform convention.
//!
//! When an agent's work relies on a THIRD-PARTY library or service that TRANSFORMS a
//! payload (normalization, enrichment, format conversion — e.g. an address service
//! returning a corrected address), that transformation is business logic living outside
//! the codebase. The convention makes agents record it, and record HONESTLY when they do
//! not know its exact semantics:
//!
//! ```text
//! ASSUMPTION[external-transform] library=<name> transform=<what changes> confidence=<known|needs-research> :: <detail>
//! ```
//!
//! `confidence=needs-research` is the placeholder path: the detail explains what is
//! known ("uses libpostal for address normalization") and what still needs human
//! research. The engine parses these markers from completed unit output and emits one
//! [`crate::event::CoreEvent::AssumptionRecorded`] per marker so the studio's
//! Assumptions panel can badge needs-research entries for review, and evidence bundles
//! can carry them.

/// One parsed external-transform assumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalTransform {
    /// The third-party library/service (a single token, e.g. `libpostal`).
    pub library: String,
    /// What the transformation does to the payload (free text).
    pub transform: String,
    /// `true` = the agent captured the logic; `false` = placeholder, needs human review.
    pub known: bool,
    /// Captured logic, or the needs-research placeholder text.
    pub detail: String,
}

/// The marker prefix agents emit (also the parse anchor).
pub const MARKER: &str = "ASSUMPTION[external-transform]";

/// Caps: a hostile/rambling transcript cannot flood the event stream.
const MAX_MARKERS: usize = 16;
const MAX_FIELD: usize = 400;

/// The standing instruction appended to real work-unit prompts (never to engine-internal
/// judge/triage prompts — see `skill_prompt`). SINGLE-LINE by contract: PTY session
/// runners write prompts line-based, so an embedded newline would split the turn.
pub const PROMPT_CONVENTION: &str = " ||| CONVENTION (external transformations): if this \
     work relies on a third-party library or service that transforms a payload \
     (normalization, enrichment, format conversion - e.g. an address service returning a \
     corrected address), record EACH one on its own output line, exactly: \
     ASSUMPTION[external-transform] library=<name> transform=<what changes> \
     confidence=<known|needs-research> :: <the transformation logic as you understand it; \
     when unsure, use confidence=needs-research and state what needs human research>";

/// Parse every external-transform marker in `output` (bounded). Lines that carry the
/// marker but not the full grammar are captured as needs-research placeholders rather
/// than dropped — a half-written assumption is still a signal a human should see.
pub fn parse(output: &str) -> Vec<ExternalTransform> {
    let mut found = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Tolerate leading list markers / quote gutters agents love to add.
        let Some(ix) = line.find(MARKER) else {
            continue;
        };
        let rest = line[ix + MARKER.len()..].trim();

        let (fields, detail) = match rest.split_once("::") {
            Some((f, d)) => (f.trim(), d.trim()),
            None => (rest, ""),
        };

        let library = field_token(fields, "library=").unwrap_or_default();
        let confidence = field_token(fields, "confidence=").unwrap_or_default();
        // `transform=` runs from its key to the next known key (or the end of fields).
        let transform = field_span(fields, "transform=", &["confidence=", "library="]);

        let well_formed = !library.is_empty()
            && !transform.is_empty()
            && matches!(confidence.as_str(), "known" | "needs-research");

        let entry = if well_formed {
            ExternalTransform {
                library: clip(&library),
                transform: clip(&transform),
                known: confidence == "known",
                detail: clip(if detail.is_empty() {
                    "(no detail provided)"
                } else {
                    detail
                }),
            }
        } else {
            // Malformed marker → needs-research placeholder carrying the raw line, so
            // the signal survives even when the agent fumbled the grammar.
            ExternalTransform {
                library: if library.is_empty() {
                    "(unspecified)".to_string()
                } else {
                    clip(&library)
                },
                transform: if transform.is_empty() {
                    "(unspecified transformation)".to_string()
                } else {
                    clip(&transform)
                },
                known: false,
                detail: clip(&format!("malformed marker, review the source line: {line}")),
            }
        };
        found.push(entry);
        if found.len() >= MAX_MARKERS {
            break;
        }
    }
    found
}

/// The single whitespace-delimited token following `key`, if present.
fn field_token(fields: &str, key: &str) -> Option<String> {
    let ix = fields.find(key)?;
    fields[ix + key.len()..]
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// The span from after `key` up to the earliest of `stops` (or end), trimmed.
fn field_span(fields: &str, key: &str, stops: &[&str]) -> String {
    let Some(ix) = fields.find(key) else {
        return String::new();
    };
    let after = &fields[ix + key.len()..];
    let end = stops
        .iter()
        .filter_map(|s| after.find(s))
        .min()
        .unwrap_or(after.len());
    after[..end].trim().to_string()
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_FIELD).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_needs_research_markers_parse() {
        let out = "did the work\n\
            ASSUMPTION[external-transform] library=libpostal transform=address normalization to canonical form confidence=known :: expands abbreviations, reorders components per locale\n\
            more text\n\
            - ASSUMPTION[external-transform] library=stripe-tax transform=tax line enrichment confidence=needs-research :: uses stripe-tax for jurisdiction resolution; exact rounding rules unverified\n";
        let got = parse(out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].library, "libpostal");
        assert_eq!(got[0].transform, "address normalization to canonical form");
        assert!(got[0].known);
        assert!(got[0].detail.contains("expands abbreviations"));
        assert_eq!(got[1].library, "stripe-tax");
        assert!(!got[1].known);
        assert!(got[1].detail.contains("unverified"));
    }

    #[test]
    fn malformed_markers_become_review_placeholders_not_silence() {
        let got = parse("ASSUMPTION[external-transform] libpostal does address stuff");
        assert_eq!(got.len(), 1);
        assert!(!got[0].known, "malformed → needs review");
        assert!(got[0].detail.contains("malformed marker"));

        // Wrong confidence word is not silently accepted as known.
        let got =
            parse("ASSUMPTION[external-transform] library=x transform=y confidence=certain :: z");
        assert_eq!(got.len(), 1);
        assert!(!got[0].known);
    }

    #[test]
    fn no_markers_no_output_and_flood_is_capped() {
        assert!(parse("plain output with no markers").is_empty());
        let flood = "ASSUMPTION[external-transform] library=a transform=b confidence=known :: c\n"
            .repeat(50);
        assert_eq!(parse(&flood).len(), MAX_MARKERS);
    }
}
