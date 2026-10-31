//! Screen role C's `memory_proposals` before the user ever sees them.
//!
//! Role C's prompt says: at most three short facility facts — plant, unit,
//! equipment tag, governing standard — stated in the request or evidence; never a
//! finding. Structured output enforces the *shape* (`[{key, value}]`) but not
//! the meaning, and a 1.5B model ignores the prose: the first real turn proposed
//! `{"key": "Pump P-101 Inspection Report", "value": "Critical Coating Loss
//! Detected"}` and SOP section titles. Asking the user to "remember" a finding is
//! worse than proposing nothing.
//!
//! So this is the deterministic gate, in the same spirit as the plan validator:
//! the model suggests, code decides. Every rule is a plain predicate, no model
//! call, no added latency.

use crate::engine::schemas::{MemoryFact, ToolResult};

/// The only keys a cross-chat fact may have. Facility identity, not content.
const ALLOWED: &[&str] = &["plant", "site", "unit", "area", "equipment", "standard"];

/// Words in a model-written key that map onto an allowed one.
const SYNONYMS: &[(&str, &str)] = &[
    ("facility", "plant"),
    ("refinery", "plant"),
    ("location", "site"),
    ("section", "area"),
    ("asset", "equipment"),
    ("tag", "equipment"),
    ("code", "standard"),
];

const PLACEHOLDERS: &[&str] = &[
    "not provided", "not specified", "not available", "not mentioned", "unknown",
    "n/a", "na", "none", "null", "tbd", "-",
];

const MAX_VALUE_CHARS: usize = 40;
const MAX_VALUE_WORDS: usize = 6;
const MAX_PROPOSALS: usize = 3;

/// Keep only proposals that are plausibly durable facility facts, grounded in
/// what this turn actually saw.
///
/// **Inputs:** the raw proposals, the user's prompt, and the tool results the
/// report was written from. **Output:** at most three facts with a normalised
/// key, one per key.
pub fn screen(proposals: Vec<MemoryFact>, prompt: &str, results: &[ToolResult]) -> Vec<MemoryFact> {
    // Everything this turn read: the grounding haystack. A value that appears
    // nowhere in it was invented by the model. Built from the *text* in the
    // tool data (every JSON string leaf), not its serialisation — that would add
    // field names ("source", "text") as matchable words and escape a wrapped
    // name into `MRPL\nPhase-3`, which a value could then never match.
    let mut text = String::from(prompt);
    let mut kb_sources: Vec<String> = Vec::new();
    for r in results.iter().filter(|r| r.ok) {
        collect_strings(&r.data, &mut text);
        if let Some(chunks) = r.data.get("chunks").and_then(|c| c.as_array()) {
            for c in chunks {
                if let Some(s) = c.get("source").and_then(|s| s.as_str()) {
                    kb_sources.push(normalise(s));
                }
            }
        }
    }
    let haystack = normalise(&text);

    let mut kept: Vec<MemoryFact> = Vec::new();
    for p in proposals {
        let Some(key) = normalise_key(&p.key) else {
            tracing::debug!(key = %p.key, "memory proposal dropped: key not a facility field");
            continue;
        };
        let value = p.value.trim();
        if let Some(why) = reject_value(key, value, &haystack, &kb_sources) {
            tracing::debug!(key, value, why, "memory proposal dropped");
            continue;
        }
        if kept.iter().any(|k| k.key == key) {
            continue;
        }
        kept.push(MemoryFact {
            key: key.to_string(),
            value: value.to_string(),
        });
        if kept.len() == MAX_PROPOSALS {
            break;
        }
    }
    kept
}

/// Append every string leaf of `v` to `out`, space-separated.
fn collect_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            out.push(' ');
            out.push_str(s);
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

/// Lower-case and collapse every whitespace run to one space. `parse_pdf` runs
/// with `layout: true`, which pads columns with runs of spaces, so a name
/// printed as `MRPL   Phase-3` must still match the model's `MRPL Phase-3`.
fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// `"Plant name"` → `plant`; `"Equipment tag"` → `equipment`; a title like
/// `"Pump P-101 Inspection Report"` → `None`. Only short keys qualify: a key
/// that is a sentence is a sign the model wrote content into it.
fn normalise_key(raw: &str) -> Option<&'static str> {
    let k = raw.trim().to_lowercase();
    if k.split_whitespace().count() > 3 {
        return None;
    }
    // A field name has no digits. "Unit-2 Pump P-101" or "safety_note_1" is the
    // model writing content (or a numbered list) into the key.
    if k.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    for word in k.split(|c: char| !c.is_alphanumeric()) {
        if let Some(a) = ALLOWED.iter().find(|a| **a == word) {
            return Some(a);
        }
        if let Some((_, a)) = SYNONYMS.iter().find(|(s, _)| *s == word) {
            return Some(a);
        }
    }
    None
}

/// Why `value` is not a fact worth remembering, or `None` if it passes.
fn reject_value(key: &str, value: &str, haystack: &str, kb_sources: &[String]) -> Option<&'static str> {
    // One character ("2") is trivially "grounded" somewhere in any document.
    if value.chars().count() < 2 {
        return Some("too short to identify anything");
    }
    if value.chars().count() > MAX_VALUE_CHARS || value.split_whitespace().count() > MAX_VALUE_WORDS {
        return Some("too long to be a name or tag");
    }
    if value.ends_with('.') || value.contains(": ") {
        return Some("reads like a sentence, not a name");
    }
    let v = normalise(value);
    // Placeholders a small model fills a field with when it has nothing — and
    // which a document may well contain, so grounding alone won't catch them.
    if PLACEHOLDERS.contains(&v.as_str()) {
        return Some("a placeholder, not a value");
    }
    if !haystack.contains(&v) {
        return Some("not stated in the request or evidence");
    }
    // The knowledge base is already searchable; remembering one of its
    // documents as a "facility fact" adds nothing.
    if kb_sources.iter().any(|s| s.contains(&v) || v.contains(s.as_str())) {
        return Some("a knowledge-base document, not a facility fact");
    }
    // Equipment is identified by tags (P-101, V-12A, TK-3); a value with no
    // digit there is a description or a finding.
    if key == "equipment" && !value.chars().any(|c| c.is_ascii_digit()) {
        return Some("equipment without a tag number");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(k: &str, v: &str) -> MemoryFact {
        MemoryFact { key: k.into(), value: v.into() }
    }

    fn evidence() -> Vec<ToolResult> {
        vec![
            ToolResult {
                step_id: "x1".into(),
                task: "parse_pdf".into(),
                ok: true,
                data: serde_json::json!({
                    "text": "Unit-2, MRPL Phase-3. Pump P-101 casing: critical coating loss detected.",
                    "source": "inspection.pdf"
                }),
                error: None,
                warning: None,
                elapsed_ms: 0,
            },
            ToolResult {
                step_id: "kb".into(),
                task: "search_knowledge".into(),
                ok: true,
                data: serde_json::json!({ "chunks": [
                    { "text": "Corrosion grades and actions …", "source": "SOP-COR-014 External Corrosion Inspection" }
                ]}),
                error: None,
                warning: None,
                elapsed_ms: 0,
            },
        ]
    }

    /// The exact junk the first real turn (qwen2.5:1.5b) produced.
    #[test]
    fn drops_the_first_real_turns_junk() {
        let raw = vec![
            fact("Pump P-101 Inspection Report", "Critical Coating Loss Detected"),
            fact("SOP-COR-014 External Corrosion Inspection", "Corrosion Grades and Actions"),
        ];
        assert!(screen(raw, "Assess pump P-101", &evidence()).is_empty());
    }

    /// The junk from the six warm runs (2026-10-11, qwen2.5:1.5b), each one
    /// placed in the evidence so grounding can't be what catches it — every
    /// one must fall to a structural rule.
    #[test]
    fn drops_the_warm_runs_junk_even_when_grounded() {
        let raw = vec![
            fact("Plant Name", "Not Provided"),
            fact("inspection_sop_14", "External Corrosion Inspection"),
            fact("safety_note_1", "Isolate the pump before inspection"),
            fact("Unit-2 Pump P-101", "Pump P-101, identified as Unit-2"),
        ];
        let mut ev = evidence();
        ev[0].data = serde_json::json!({ "text":
            "Plant: not provided. External Corrosion Inspection. Isolate the pump before inspection. \
             Pump P-101, identified as Unit-2." });
        let kept = screen(raw, "", &ev);
        assert!(kept.is_empty(), "{kept:?}");
    }

    #[test]
    fn keeps_grounded_short_facility_facts_with_normalised_keys() {
        let raw = vec![
            fact("Plant name", "MRPL Phase-3"),
            fact("Equipment tag", "P-101"),
            fact("unit", "Unit-2"),
        ];
        let kept = screen(raw, "Assess pump P-101", &evidence());
        assert_eq!(
            kept,
            vec![fact("plant", "MRPL Phase-3"), fact("equipment", "P-101"), fact("unit", "Unit-2")]
        );
    }

    #[test]
    fn drops_values_the_turn_never_saw() {
        let kept = screen(vec![fact("plant", "Jamnagar Refinery")], "Assess P-101", &evidence());
        assert!(kept.is_empty(), "invented by the model, not in the evidence");
    }

    #[test]
    fn equipment_needs_a_tag() {
        let kept = screen(vec![fact("equipment", "pump p-101 casing")], "", &evidence());
        assert_eq!(kept.len(), 1, "has a digit");
        let kept = screen(vec![fact("equipment", "casing")], "", &evidence());
        assert!(kept.is_empty(), "a part name, not an identifier");
    }

    #[test]
    fn knowledge_base_documents_are_not_facility_facts() {
        let kept = screen(vec![fact("standard", "SOP-COR-014")], "", &evidence());
        assert!(kept.is_empty());
    }

    /// `layout: true` PDF text pads columns, and a name can wrap a line. Both
    /// must still ground the model's single-spaced value.
    #[test]
    fn column_padding_and_line_wraps_still_ground_a_value() {
        let mut ev = evidence();
        ev[0].data = serde_json::json!({ "text": "Site:   MRPL\n   Phase-3   (Unit-2)" });
        let kept = screen(vec![fact("plant", "MRPL Phase-3")], "", &ev);
        assert_eq!(kept, vec![fact("plant", "MRPL Phase-3")]);
    }

    /// JSON field names are not evidence: "source" must not ground a value.
    #[test]
    fn field_names_are_not_grounding() {
        let kept = screen(vec![fact("site", "source")], "", &evidence());
        assert!(kept.is_empty(), "{kept:?}");
    }

    #[test]
    fn a_one_character_value_is_too_short() {
        assert!(screen(vec![fact("unit", "2")], "", &evidence()).is_empty());
    }

    #[test]
    fn one_per_key_and_at_most_three() {
        let raw = vec![
            fact("plant", "MRPL Phase-3"),
            fact("facility", "Unit-2"),
            fact("unit", "Unit-2"),
            fact("equipment", "P-101"),
            fact("area", "Unit-2"),
        ];
        let kept = screen(raw, "", &evidence());
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert_eq!(kept[0], fact("plant", "MRPL Phase-3"), "first plant wins, `facility` dup dropped");
    }
}
