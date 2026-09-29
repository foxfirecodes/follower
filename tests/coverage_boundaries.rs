use std::path::PathBuf;

use code_flow::{Analyzer, Project, queries::Conclusion};

fn fixture_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/coverage/flow.toml")
}

#[test]
fn opaque_consumers_block_absence_but_discarded_jsx_does_not_register() {
    let analyzer = Analyzer::new(Project::load(fixture_config()).expect("load fixture"));
    let report = analyzer.audit("coverage-callback").expect("audit fixture");

    let opaque = report
        .findings
        .iter()
        .find(|finding| finding.key == "opaque")
        .expect("opaque finding");
    let discarded = report
        .findings
        .iter()
        .find(|finding| finding.key == "discarded")
        .expect("discarded finding");

    assert_eq!(opaque.conclusion, Conclusion::Unresolved);
    assert_eq!(opaque.unresolved.len(), 1);
    assert!(opaque.invocations.is_empty());

    assert_eq!(discarded.conclusion, Conclusion::AbsentWithinModel);
    assert!(discarded.registrations.is_empty());
    assert!(discarded.invocations.is_empty());
    assert!(discarded.unresolved.is_empty());
}
