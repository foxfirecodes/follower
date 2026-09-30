mod support;

use code_flow::{queries::Conclusion, query::QueryValue};
use support::TestProject;

#[test]
fn callback_assignments_in_unknown_branches_block_absence_claims() {
    for statement in [
        "if (flag) { selected = callback; }",
        "if (flag === 'yes') { selected = callback; }",
        "if (flag !== 'yes') { selected = callback; }",
        "if (flag) { selected = callback; } else { selected = () => {}; }",
    ] {
        let source = format!(
            "import {{ makeCallback }} from './factory'; export function Host({{ flag }}) {{ const {{ callback }} = makeCallback('branch'); let selected = () => {{}}; {statement} selected('clicked'); }}"
        );
        let fixture = TestProject::new(&[("src/Host.tsx", &source)]);
        let report = fixture.report();
        assert_eq!(report.creations.len(), 1);
        assert_eq!(
            report.creations[0].conclusion,
            Conclusion::Unresolved,
            "{statement}"
        );
        assert!(!report.coverage.complete);
        assert!(!report.creations[0].unresolved.is_empty());
    }
}

#[test]
fn callback_created_inside_a_branch_retains_its_identity_after_the_branch() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import { makeCallback } from './factory'; export function Host({ flag }) { let selected = () => {}; if (flag) { selected = makeCallback('inner').callback; } selected('clicked'); }",
    )]);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].conclusion, Conclusion::Unresolved);
    assert!(!report.coverage.complete);
}

#[test]
fn partial_returns_preserve_callback_invocations_on_the_continuing_path() {
    for branch in [
        "if (flag) { return 'done'; }",
        "if (flag) {} else { return 'done'; }",
    ] {
        let source = format!(
            "import {{ makeCallback }} from './factory'; export function Host({{ flag }}) {{ const {{ callback }} = makeCallback('continuation'); {branch} callback('later'); }}"
        );
        let fixture = TestProject::new(&[("src/Host.tsx", &source)]);
        let report = fixture.report();
        assert_eq!(report.creations.len(), 1);
        assert_eq!(
            report.creations[0].conclusion,
            Conclusion::CandidateInvocation
        );
        assert_eq!(
            report.creations[0].invocations[0].arguments["invoked"],
            QueryValue::String {
                value: "later".into()
            }
        );
    }
}

#[test]
fn joined_data_becomes_an_explicit_unknown_projection_and_coverage_gap() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import { makeCallback } from './factory'; export function Host({ flag }) { let value = 'alpha'; if (flag) { value = 'beta'; } makeCallback(value).callback('clicked'); }",
    )]);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert!(matches!(
        report.creations[0].factory_arguments["created"],
        QueryValue::Unknown { .. }
    ));
    assert!(!report.coverage.complete);
    assert!(!report.creations[0].unresolved.is_empty());
}

#[test]
fn opaque_callback_calls_have_incomplete_coverage_but_unrelated_branches_do_not() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import { makeCallback } from './factory'; export function Host({ flag }) { let irrelevant = 'alpha'; if (flag) { irrelevant = 'beta'; } const { callback } = makeCallback('opaque'); opaqueConsumer(callback); }",
    )]);
    let report = fixture.report();
    assert_eq!(report.creations[0].conclusion, Conclusion::Unresolved);
    assert!(!report.coverage.complete);
    fixture.write("src/Host.tsx", "import { makeCallback } from './factory'; export function Host({ flag }) { let irrelevant = 'alpha'; if (flag) { irrelevant = 'beta'; } const { callback } = makeCallback('known'); callback('clicked'); }");
    let report = fixture.report();
    assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
    assert!(report.creations[0].unresolved.is_empty());
}

#[test]
fn reassigned_captures_and_writes_through_closures_are_unresolved() {
    for body in [
        "let target = () => {}; const handler = () => target('clicked'); target = callback; return <button onClick={handler} />;",
        "let target = () => {}; const setter = () => { target = callback; }; setter(); target('clicked');",
    ] {
        let source = format!(
            "import {{ makeCallback }} from './factory'; export function Host() {{ const {{ callback }} = makeCallback('capture'); {body} }}"
        );
        let fixture = TestProject::new(&[("src/Host.tsx", &source)]);
        let report = fixture.report();
        assert_eq!(report.creations.len(), 1);
        assert_eq!(report.creations[0].conclusion, Conclusion::Unresolved);
        assert!(!report.coverage.complete);
        assert!(
            report
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.contains("live closure cells"))
        );
    }
}

#[test]
fn opaque_namespace_escapes_preserve_module_callback_dependencies() {
    let fixture = TestProject::new(&[
        (
            "src/callbacks.ts",
            "import { makeCallback } from './factory'; export const callback = makeCallback('module').callback;",
        ),
        (
            "src/Host.tsx",
            "import * as callbacks from './callbacks'; export function Host() { opaqueConsumer(callbacks); }",
        ),
    ]);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].conclusion, Conclusion::Unresolved);
    assert!(!report.coverage.complete);
}

#[test]
fn unrelated_unknown_iteration_does_not_capture_a_live_callback() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import { makeCallback } from './factory'; export function Host({ rows }) { const { callback } = makeCallback('known'); rows.map(row => row); callback('clicked'); }",
    )]);
    let report = fixture.report();
    assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].conclusion,
        Conclusion::CandidateInvocation
    );
    assert!(report.creations[0].unresolved.is_empty());
}

#[test]
fn fail_on_unresolved_rejects_ambiguous_empty_results_and_candidates_with_gaps() {
    let fixture = TestProject::new(&[
        ("src/other.ts", "export function makeCallback() {}"),
        (
            "src/barrel.ts",
            "export * from './factory'; export * from './other';",
        ),
        (
            "src/Host.tsx",
            "import { makeCallback } from './barrel'; export function Host() { makeCallback('ambiguous'); }",
        ),
    ]);
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_follower"))
            .arg("query")
            .arg("--project")
            .arg(fixture.root.join("flow.toml"))
            .arg("--query")
            .arg(fixture.root.join("query.toml"))
            .args(["--format", "json", "--fail-on-unresolved"])
            .output()
            .expect("run CLI")
    };
    let output = run();
    assert!(!output.status.success());
    let report: code_flow::query::QueryReport =
        serde_json::from_slice(&output.stdout).expect("JSON report");
    assert!(report.creations.is_empty());
    assert!(!report.coverage.complete);
    fixture.write("src/Host.tsx", "import { makeCallback } from './factory'; export function Host() { const { callback } = makeCallback('candidate'); callback('clicked'); opaqueConsumer(callback); }");
    let output = run();
    assert!(!output.status.success());
    let report: code_flow::query::QueryReport =
        serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(
        report.creations[0].conclusion,
        Conclusion::CandidateInvocation
    );
    assert!(!report.creations[0].unresolved.is_empty());
    fixture.write("src/Host.tsx", "import { makeCallback } from './factory'; export function Host() { makeCallback('known').callback('clicked'); }");
    assert!(run().status.success());
}
