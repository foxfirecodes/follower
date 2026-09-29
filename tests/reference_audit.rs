use std::path::PathBuf;

use code_flow::{Analyzer, Project, queries::Conclusion};

fn fixture_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/reference/flow.toml")
}

#[test]
fn keeps_registry_component_and_callback_choices_correlated() {
    let analyzer = Analyzer::new(Project::load(fixture_config()).expect("load fixture"));
    let report = analyzer.audit("notice-dismiss-v1").expect("audit fixture");

    assert_eq!(report.findings.len(), 2);
    let welcome = report
        .findings
        .iter()
        .find(|finding| finding.key == "welcome")
        .expect("welcome finding");
    let upgrade = report
        .findings
        .iter()
        .find(|finding| finding.key == "upgrade")
        .expect("upgrade finding");

    assert_eq!(welcome.choice, "welcome");
    assert_eq!(welcome.conclusion, Conclusion::CandidateInvocation);
    assert_eq!(welcome.registrations.len(), 1);
    assert_eq!(welcome.invocations.len(), 1);

    assert_eq!(upgrade.choice, "upgrade");
    assert_eq!(upgrade.conclusion, Conclusion::AbsentWithinModel);
    assert_eq!(upgrade.registrations.len(), 1);
    assert!(upgrade.invocations.is_empty());
    assert!(upgrade.unresolved.is_empty());

    assert!(report.coverage.complete);
}
