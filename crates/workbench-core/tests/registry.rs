//! Phase 1 gate: the deterministic plan validator rejects every class of bad plan
//! the blueprint names, and accepts a well-formed one.

use workbench_core::engine::schemas::{FileKind, InputFile, Plan, TaskStep};
use workbench_core::planner::{normalize_task_name, validate_plan};

fn step(id: &str, task: &str, deps: &[&str]) -> TaskStep {
    TaskStep {
        id: id.into(),
        task: task.into(),
        args: serde_json::Value::Null,
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn rejects_unknown_task() {
    let plan = Plan {
        steps: vec![step("s1", "frobnicate", &[])],
    };
    let errs = validate_plan(&plan, &[]);
    assert!(
        errs.iter().any(|e| e.contains("unknown task")),
        "expected an unknown-task error, got {errs:?}"
    );
}

#[test]
fn rejects_analysis_before_extraction() {
    let plan = Plan {
        steps: vec![step("s1", "summarize", &[])],
    };
    let errs = validate_plan(&plan, &[]);
    assert!(
        errs.iter().any(|e| e.contains("no preceding extraction")),
        "expected a stage-order error, got {errs:?}"
    );
}

#[test]
fn rejects_missing_required_file() {
    let plan = Plan {
        steps: vec![step("s1", "parse_pdf", &[]), step("s2", "summarize", &["s1"])],
    };
    let errs = validate_plan(&plan, &[]); // no PDF attached
    assert!(
        errs.iter().any(|e| e.contains("needs an attached Pdf file")),
        "expected a missing-file error, got {errs:?}"
    );
}

#[test]
fn rejects_file_not_on_disk() {
    let plan = Plan {
        steps: vec![step("s1", "parse_pdf", &[]), step("s2", "summarize", &["s1"])],
    };
    let uploads = vec![InputFile {
        path: "/no/such/file.pdf".into(),
        kind: FileKind::Pdf,
        original_name: "x.pdf".into(),
    }];
    let errs = validate_plan(&plan, &uploads);
    assert!(
        errs.iter().any(|e| e.contains("was not found")),
        "expected a not-found error, got {errs:?}"
    );
}

#[test]
fn rejects_dependency_listed_after_dependent() {
    let plan = Plan {
        steps: vec![
            step("s1", "summarize", &["s2"]),
            step("s2", "search_knowledge", &[]),
        ],
    };
    let errs = validate_plan(&plan, &[]);
    assert!(
        errs.iter().any(|e| e.contains("listed before its dependency")),
        "expected a topological-order error, got {errs:?}"
    );
}

#[test]
fn tolerates_stage_label_and_quotes_on_task_name() {
    assert_eq!(normalize_task_name("parse_pdf [Extract]"), "parse_pdf");
    assert_eq!(normalize_task_name("`search_knowledge`"), "search_knowledge");
    assert_eq!(normalize_task_name("  \"summarize\"  "), "summarize");
    assert_eq!(normalize_task_name("ocr_image"), "ocr_image");

    // A plan whose task names carry the prompt's stage label still validates.
    let plan = Plan {
        steps: vec![
            step("s1", "search_knowledge [Retrieve]", &[]),
            step("s2", "summarize [Analyze]", &["s1"]),
        ],
    };
    let errs = validate_plan(&plan, &[]);
    assert!(
        !errs.iter().any(|e| e.contains("unknown task")),
        "stage-labelled task names should still resolve, got {errs:?}"
    );
}

#[test]
fn accepts_well_formed_plan() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("insp.pdf");
    std::fs::write(&pdf, b"%PDF-1.4 minimal").unwrap();

    let uploads = vec![InputFile {
        path: pdf.to_string_lossy().into_owned(),
        kind: FileKind::Pdf,
        original_name: "insp.pdf".into(),
    }];
    let plan = Plan {
        steps: vec![
            step("s1", "parse_pdf", &[]),
            step("s2", "search_knowledge", &[]),
            step("s3", "compare_to_sop", &["s1", "s2"]),
        ],
    };
    let errs = validate_plan(&plan, &uploads);
    assert!(errs.is_empty(), "expected a clean plan, got {errs:?}");
}

#[test]
fn unsupported_file_type_is_no_longer_a_validation_error() {
    // Stage 3: an `.mov` (FileKind::Unknown) is screened out with a warning
    // *before* validation now, so validate_plan itself no longer rejects it —
    // one bad attachment must not block a plan over the good ones.
    let dir = tempfile::tempdir().unwrap();
    let mov = dir.path().join("clip.mov");
    std::fs::write(&mov, b"not really a movie").unwrap();
    let pdf = dir.path().join("insp.pdf");
    std::fs::write(&pdf, b"%PDF-1.4").unwrap();

    let uploads = vec![
        InputFile {
            path: pdf.to_string_lossy().into_owned(),
            kind: FileKind::Pdf,
            original_name: "insp.pdf".into(),
        },
        InputFile {
            path: mov.to_string_lossy().into_owned(),
            kind: FileKind::Unknown,
            original_name: "clip.mov".into(),
        },
    ];
    let plan = Plan {
        steps: vec![step("s1", "parse_pdf", &[]), step("s2", "summarize", &["s1"])],
    };
    let errs = validate_plan(&plan, &uploads);
    assert!(
        !errs.iter().any(|e| e.contains("unsupported")),
        "unsupported-type should be a pre-screen warning, not a plan error: {errs:?}"
    );
}
