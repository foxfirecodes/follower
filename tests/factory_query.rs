use std::path::PathBuf;

use code_flow::{
    Analyzer, Project,
    queries::Conclusion,
    query::{QueryValue, load_query},
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/factory-query/{name}"))
}

fn strings(value: &QueryValue) -> Vec<&str> {
    let QueryValue::Array { elements } = value else {
        panic!("expected array query value");
    };
    elements
        .iter()
        .map(|element| {
            let QueryValue::String { value } = element else {
                panic!("expected string query value");
            };
            value.as_str()
        })
        .collect()
}

#[test]
fn correlates_array_factory_arguments_with_forwarded_invocation_arguments() {
    let analyzer = Analyzer::new(Project::load(fixture_path("flow.toml")).expect("load project"));
    let (query, hash) = load_query(&fixture_path("query.toml")).expect("load query");
    let report = analyzer.query(&query, &hash).expect("run query");

    assert_eq!(report.creations.len(), 3);
    let welcome = report
        .creations
        .iter()
        .find(|creation| creation.choice == "welcome")
        .expect("welcome creation");
    assert_eq!(
        strings(&welcome.factory_arguments["thing_types"]),
        ["banner", "modal"]
    );
    assert_eq!(welcome.conclusion, Conclusion::CandidateInvocation);
    assert_eq!(welcome.invocations.len(), 1);
    assert_eq!(
        welcome.invocations[0].arguments["hide_kind"],
        QueryValue::String {
            value: "close_button".to_owned()
        }
    );

    let upgrade = report
        .creations
        .iter()
        .find(|creation| creation.choice == "upgrade")
        .expect("upgrade creation");
    assert_eq!(
        strings(&upgrade.factory_arguments["thing_types"]),
        ["upgrade"]
    );
    assert_eq!(upgrade.invocations.len(), 1);
    assert_eq!(
        upgrade.invocations[0].arguments["hide_kind"],
        QueryValue::String {
            value: "timeout".to_owned()
        }
    );

    let dormant = report
        .creations
        .iter()
        .find(|creation| creation.choice == "<unreached>")
        .expect("unreached creation");
    assert_eq!(
        dormant.reachability,
        code_flow::query::Reachability::Unknown
    );
    assert_eq!(
        strings(&dormant.factory_arguments["thing_types"]),
        ["modal"]
    );
    assert_eq!(dormant.invocations.len(), 1);
    assert_eq!(dormant.conclusion, Conclusion::CandidateInvocation);
    assert!(!dormant.unresolved.is_empty());
    assert!(!report.coverage.complete);
}
