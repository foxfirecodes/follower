use std::{collections::BTreeSet, path::PathBuf};

use code_flow::{
    Analyzer, Project,
    queries::Conclusion,
    query::{QueryValue, load_query},
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/query-control-flow/{name}"))
}

fn string(value: &QueryValue) -> &str {
    let QueryValue::String { value } = value else {
        panic!("expected string query value");
    };
    value
}

fn only_array_string(value: &QueryValue) -> &str {
    let QueryValue::Array { elements } = value else {
        panic!("expected array query value");
    };
    assert_eq!(elements.len(), 1);
    string(&elements[0])
}

#[test]
fn preserves_rows_through_finite_branch_literal_map_and_if_statement() {
    let analyzer = Analyzer::new(Project::load(fixture_path("flow.toml")).expect("load project"));
    let (query, hash) = load_query(&fixture_path("query.toml")).expect("load query");
    let report = analyzer.query(&query, &hash).expect("run query");

    assert_eq!(report.creations.len(), 3);
    assert!(report.coverage.complete);
    assert!(report.coverage.gaps.is_empty());
    assert!(
        report
            .creations
            .iter()
            .all(|creation| creation.conclusion == Conclusion::CandidateInvocation)
    );

    let triples = report
        .creations
        .iter()
        .map(|creation| {
            (
                creation.choice.as_str(),
                only_array_string(&creation.factory_arguments["thing_types"]),
                string(&creation.invocations[0].arguments["hide_kind"]),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        triples,
        BTreeSet::from([
            ("alpha", "banner", "close_button"),
            ("alpha", "modal", "timeout"),
            ("beta", "upgrade", "close_button"),
        ])
    );
}
