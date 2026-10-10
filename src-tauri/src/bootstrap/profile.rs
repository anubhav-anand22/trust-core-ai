//! The user's profile: what they do, what they'll feed the app, what they value.
//!
//! Asked once, on the first screen, and stored as plain JSON (`profile.json` in
//! the app data dir — nothing secret in it). It never goes through a model: the
//! recommender runs before any model is installed, so the profile is mapped to
//! concrete needs ([`ProfileNeeds`]) by a fixed table here. That keeps the choice
//! reproducible and explainable ("you said photos → a vision model is included").

use serde::{Deserialize, Serialize};
use tauri::Manager;

/// The answers as the profile screen stores them. Every field may be empty —
/// "Not sure" is always an option, and an empty answer maps to safe defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserProfile {
    /// One key, e.g. `"energy"`, `"finance"`. Empty = not sure.
    #[serde(default)]
    pub industry: String,
    /// Kinds of work, e.g. `["reports", "inspection"]`.
    #[serde(default)]
    pub work: Vec<String>,
    /// What they will attach: `"documents"`, `"photos"`, `"voice"`, `"code"`.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// `"fast"`, `"balanced"` or `"quality"`. Empty = balanced.
    #[serde(default)]
    pub priority: String,
    /// `"english"`, `"hinglish"` or `"multilingual"`. Empty = English.
    #[serde(default)]
    pub language: String,
}

/// What the recommender and the runtime actually need from a profile.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProfileNeeds {
    /// Use-case weights, 0..=1. Multiply into each model's strengths.
    pub docs: f32,
    pub reasoning: f32,
    pub code: f32,
    pub multilingual: f32,
    /// Photos expected → a vision path is needed. No photos → don't download a
    /// vision model at all.
    pub needs_vision: bool,
    pub needs_audio: bool,
    /// Decode speed (tokens/s, CPU terms) below which a model feels sluggish to
    /// this user. "Fast" users get smaller, quicker models.
    pub min_tok_s: f32,
    /// Context window this user benefits from, if the memory budget allows.
    pub prefer_ctx: u64,
    /// One short line for the system prompts (≤ ~40 tokens: every token is
    /// prefill time on a CPU). Empty when the profile says nothing useful.
    pub persona: String,
}

impl UserProfile {
    fn has_input(&self, k: &str) -> bool {
        self.inputs.iter().any(|i| i == k)
    }
    fn does(&self, k: &str) -> bool {
        self.work.iter().any(|w| w == k)
    }

    /// The fixed profile → needs table. Deterministic on purpose.
    pub fn needs(&self) -> ProfileNeeds {
        let unsure_inputs = self.inputs.is_empty();
        let docs = if self.has_input("documents")
            || ["analysis", "compliance", "reports"].iter().any(|w| self.does(w))
            || unsure_inputs
        {
            1.0
        } else {
            0.5
        };
        let reasoning = if ["analysis", "planning", "compliance", "design"]
            .iter()
            .any(|w| self.does(w))
        {
            1.0
        } else {
            0.6
        };
        let code = if self.has_input("code") || self.does("coding") { 1.0 } else { 0.0 };
        let multilingual = match self.language.as_str() {
            "hinglish" | "multilingual" => 1.0,
            _ => 0.0,
        };
        // Unsure keeps today's behaviour (vision included) rather than silently
        // dropping a capability the user may need.
        let needs_vision = unsure_inputs || self.has_input("photos") || self.does("inspection");
        let (min_tok_s, prefer_ctx) = match self.priority.as_str() {
            "fast" => (10.0, 4096),
            "quality" => (3.0, 8192),
            _ => (6.0, if docs >= 1.0 { 8192 } else { 4096 }),
        };

        ProfileNeeds {
            docs,
            reasoning,
            code,
            multilingual,
            needs_vision,
            needs_audio: self.has_input("voice"),
            min_tok_s,
            prefer_ctx,
            persona: self.persona(),
        }
    }

    fn persona(&self) -> String {
        let industry = industry_label(&self.industry);
        let work: Vec<&str> = self.work.iter().filter_map(|w| work_label(w)).collect();
        let mut out = String::new();
        if let Some(ind) = industry {
            out.push_str(&format!("The user works in {ind}"));
            if !work.is_empty() {
                out.push_str(&format!(", mostly on {}", work.join(", ")));
            }
            out.push('.');
        } else if !work.is_empty() {
            out.push_str(&format!("The user mostly works on {}.", work.join(", ")));
        }
        if matches!(self.language.as_str(), "hinglish" | "multilingual") {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("Reply in the language the user writes in.");
        }
        out
    }
}

fn industry_label(k: &str) -> Option<&'static str> {
    Some(match k {
        "energy" => "manufacturing, oil & gas or energy",
        "construction" => "construction and engineering",
        "finance" => "finance and banking",
        "legal" => "legal and compliance",
        "healthcare" => "healthcare",
        "logistics" => "logistics and supply chain",
        "it" => "IT and software",
        "government" => "the public sector",
        "education" => "education",
        _ => return None,
    })
}

fn work_label(k: &str) -> Option<&'static str> {
    Some(match k {
        "reports" => "reports and management summaries",
        "analysis" => "document and data analysis",
        "inspection" => "field inspection",
        "planning" => "planning and operations",
        "compliance" => "compliance and audit",
        "writing" => "writing and communication",
        "coding" => "coding",
        "design" => "design review",
        _ => return None,
    })
}

fn profile_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("profile.json"))
}

/// Read the stored profile, or `None` on first launch (or an unreadable file —
/// asking again beats crashing).
pub fn load(app: &tauri::AppHandle) -> Option<UserProfile> {
    let raw = std::fs::read_to_string(profile_path(app).ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The stored profile, or `None` → the UI shows the profile screen.
#[tauri::command]
pub fn get_profile(app: tauri::AppHandle) -> Option<UserProfile> {
    load(&app)
}

/// Store the profile the user just confirmed.
#[tauri::command]
pub fn save_profile(app: tauri::AppHandle, profile: UserProfile) -> Result<(), String> {
    let path = profile_path(&app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&profile).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    tracing::info!(
        industry = %profile.industry,
        work = ?profile.work,
        inputs = ?profile.inputs,
        priority = %profile.priority,
        language = %profile.language,
        "profile saved"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(industry: &str, work: &[&str], inputs: &[&str], priority: &str, lang: &str) -> UserProfile {
        UserProfile {
            industry: industry.into(),
            work: work.iter().map(|s| s.to_string()).collect(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            priority: priority.into(),
            language: lang.into(),
        }
    }

    /// "Not sure" everywhere must reproduce today's behaviour: vision included,
    /// balanced speed, document-oriented.
    #[test]
    fn an_empty_profile_keeps_safe_defaults() {
        let n = UserProfile::default().needs();
        assert!(n.needs_vision);
        assert_eq!(n.min_tok_s, 6.0);
        assert_eq!(n.docs, 1.0);
        assert_eq!(n.code, 0.0);
        assert!(n.persona.is_empty());
    }

    #[test]
    fn no_photos_means_no_vision_model() {
        let n = p("finance", &["reports"], &["documents"], "", "english").needs();
        assert!(!n.needs_vision, "a finance user feeding PDFs needs no vision model");
    }

    #[test]
    fn inspection_work_implies_vision_even_without_ticking_photos() {
        assert!(p("energy", &["inspection"], &["documents"], "", "").needs().needs_vision);
    }

    #[test]
    fn priority_sets_speed_target_and_context() {
        assert_eq!(p("", &[], &["documents"], "fast", "").needs().min_tok_s, 10.0);
        assert_eq!(p("", &[], &["documents"], "fast", "").needs().prefer_ctx, 4096);
        assert_eq!(p("", &[], &["documents"], "quality", "").needs().min_tok_s, 3.0);
    }

    #[test]
    fn persona_is_short_and_specific() {
        let n = p("energy", &["inspection", "compliance"], &["photos"], "", "hinglish").needs();
        assert!(n.persona.contains("oil & gas"), "{}", n.persona);
        assert!(n.persona.contains("field inspection"), "{}", n.persona);
        assert!(n.persona.contains("language the user writes in"), "{}", n.persona);
        // ~40 tokens ≈ 160 characters. Every token is CPU prefill time.
        assert!(n.persona.len() < 200, "{} chars", n.persona.len());
        assert_eq!(n.multilingual, 1.0);
    }

    #[test]
    fn coding_raises_the_code_weight() {
        assert_eq!(p("it", &["coding"], &["code"], "", "").needs().code, 1.0);
    }
}
