use std::path::PathBuf;

use code_flow::{
    Analyzer, Project,
    evidence::RelationKind,
    queries::Conclusion,
    query::{QueryValue, load_query},
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/query-mutation/{name}"))
}

#[test]
fn preserves_the_candidate_but_marks_aliased_record_mutation_unresolved() {
    let analyzer = Analyzer::new(Project::load(fixture_path("flow.toml")).expect("load project"));
    let (query, hash) = load_query(&fixture_path("query.toml")).expect("load query");
    let report = analyzer.query(&query, &hash).expect("run query");

    assert_eq!(report.schema_version, 2);
    assert_eq!(report.creations.len(), 1);
    assert!(!report.coverage.complete);
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("record aliasing around mutation"))
    );

    let creation = &report.creations[0];
    assert_eq!(creation.conclusion, Conclusion::CandidateInvocation);
    assert_eq!(creation.invocations.len(), 1);
    assert!(!creation.unresolved.is_empty());
    assert_eq!(
        creation.invocations[0].arguments["hide_kind"],
        QueryValue::String {
            value: "timeout".to_owned()
        }
    );
    assert!(
        report
            .evidence
            .iter()
            .any(|evidence| evidence.relation == RelationKind::Mutation)
    );

    let factory_location = creation
        .factory_location
        .as_ref()
        .expect("factory source location");
    assert_eq!(factory_location.path, "src/Host.tsx");
    assert_eq!(factory_location.start_line, 12);
    assert!(creation.invocations[0].location.is_some());
}
