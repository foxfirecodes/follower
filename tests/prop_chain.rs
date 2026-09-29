use std::{collections::BTreeSet, path::PathBuf};

use code_flow::{
    Analyzer, Project,
    evidence::RelationKind,
    ids::EvidenceId,
    queries::{AuditReport, Conclusion},
};

fn fixture_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/prop-chain/flow.toml")
}

fn evidence_id(reference: &str) -> EvidenceId {
    EvidenceId(
        reference
            .strip_prefix('E')
            .expect("evidence reference prefix")
            .parse()
            .expect("numeric evidence reference"),
    )
}

fn collect_ancestors(report: &AuditReport, root: EvidenceId) -> BTreeSet<EvidenceId> {
    let mut pending = vec![root];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let evidence = &report.evidence[id.0 as usize];
        pending.extend(evidence.parents.iter().copied());
    }
    visited
}

#[test]
fn follows_two_callbacks_through_three_prop_forwarding_components() {
    let analyzer = Analyzer::new(Project::load(fixture_config()).expect("load fixture"));
    let report = analyzer
        .audit("prop-chain-callback")
        .expect("audit fixture");

    assert_eq!(report.findings.len(), 2);
    for expected_key in ["alpha", "beta"] {
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.key == expected_key)
            .expect("finding for configured key");

        assert_eq!(finding.choice, expected_key);
        assert_eq!(finding.conclusion, Conclusion::CandidateInvocation);
        assert_eq!(finding.registrations.len(), 1);
        assert_eq!(finding.invocations.len(), 1);
        assert!(finding.unresolved.is_empty());

        let ancestors = collect_ancestors(&report, evidence_id(&finding.invocations[0].finding_id));
        let prop_bindings = ancestors
            .iter()
            .filter(|id| report.evidence[id.0 as usize].relation == RelationKind::RenderPropBinding)
            .count();
        assert!(
            prop_bindings >= 6,
            "expected the invocation evidence to retain the three-component prop path"
        );
    }
}
