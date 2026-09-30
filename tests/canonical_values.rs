mod support;

use code_flow::{
    link::{SymbolLinker, ValueResolution},
    queries::Conclusion,
    query::{QueryScope, QueryValue},
};
use support::TestProject;

fn string(value: &QueryValue) -> &str {
    let QueryValue::String { value } = value else {
        panic!("expected a string: {value:?}");
    };
    value
}

#[test]
fn enum_and_constant_aliases_keep_their_defining_module_across_namespace_barrels() {
    let fixture = TestProject::new(&[
        (
            "src/values.ts",
            "export enum Kind { Value = 'right' }\nexport const reason = 'clicked';\nexport const { selected } = { selected: Kind.Value };",
        ),
        (
            "src/renamed.ts",
            "import { Kind as LocalKind, reason as localReason, selected } from './values';\nexport { LocalKind as RenamedKind, localReason as renamedReason, selected };",
        ),
        (
            "src/public.ts",
            "export * as values from './renamed';\nexport * as factories from './factory';",
        ),
        (
            "src/alias.ts",
            "import * as Public from './public'; export { Public as api };",
        ),
        (
            "src/Host.tsx",
            "import { api } from './alias';\nexport function Host() { const { callback } = api.factories.makeCallback(api.values.RenamedKind.Value); callback(api.values.renamedReason); }",
        ),
    ]);
    for decoy in ["wrong-one", "wrong-two"] {
        fixture.write("src/zz-decoy.ts", &format!("export enum Kind {{ Value = '{decoy}' }}\nexport const reason = '{decoy}'; export const selected = '{decoy}';"));
        let report = fixture.report();
        assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
        assert!(report.diagnostics.is_empty());
        assert_eq!(report.creations.len(), 1);
        let creation = &report.creations[0];
        assert_eq!(string(&creation.factory_arguments["created"]), "right");
        assert_eq!(
            string(&creation.invocations[0].arguments["invoked"]),
            "clicked"
        );
    }
    fixture.write("src/Host.tsx", "import { selected as choice } from './values'; import { makeCallback } from './factory'; export function Host() { makeCallback(choice).callback('direct'); }");
    let report = fixture.report();
    assert!(report.coverage.complete);
    assert_eq!(
        string(&report.creations[0].factory_arguments["created"]),
        "right"
    );
}

#[test]
fn module_initializers_resolve_dependencies_without_relying_on_file_sort_order() {
    let fixture = TestProject::new(&[
        (
            "src/a-derived.ts",
            "import { seed as input } from './z-seed'; export const projected = input.value;",
        ),
        (
            "src/z-seed.ts",
            "export const seed = { value: 'dependency' };",
        ),
        (
            "src/Host.tsx",
            "import { projected } from './a-derived'; import { makeCallback } from './factory'; export function Host() { makeCallback(projected).callback(projected); }",
        ),
    ]);
    let report = fixture.report();
    assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
    assert_eq!(
        string(&report.creations[0].factory_arguments["created"]),
        "dependency"
    );
}

#[test]
fn unimported_module_initializers_do_not_create_reachable_query_rows() {
    let fixture = TestProject::new(&[
        (
            "src/unused.ts",
            "import { makeCallback } from './factory'; export const unused = makeCallback('unreachable');",
        ),
        (
            "src/Host.tsx",
            "import { makeCallback } from './factory'; export function Host() { makeCallback('reachable').callback('clicked'); }",
        ),
    ]);
    let report = fixture.report();
    assert!(report.coverage.complete);
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        string(&report.creations[0].factory_arguments["created"]),
        "reachable"
    );
    let mut query = fixture.query();
    query.scope = QueryScope::AllCreations;
    let report = fixture
        .analyzer()
        .query(&query, "test")
        .expect("all creations");
    assert_eq!(report.creations.len(), 2);
    assert!(!report.coverage.complete);
    let unused = report
        .creations
        .iter()
        .find(|creation| creation.reachability == code_flow::query::Reachability::Unknown)
        .expect("unreachable creation");
    assert!(!unused.unresolved.is_empty());
}

#[test]
fn an_exported_arrow_factory_is_instrumented_through_a_local_alias() {
    let fixture = TestProject::new(&[
        (
            "src/factory.ts",
            "export const makeCallback = (_value: string) => { throw new Error('modeled factory'); };",
        ),
        (
            "src/Host.tsx",
            "import { makeCallback as factory } from './factory'; export function Host() { const localFactory = factory; localFactory('created').callback('clicked'); }",
        ),
    ]);
    let report = fixture.report();
    assert!(report.coverage.complete);
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].conclusion,
        Conclusion::CandidateInvocation
    );
}

#[test]
fn module_closures_follow_callbacks_declared_later_in_the_module() {
    let fixture = TestProject::new(&[
        (
            "src/callbacks.ts",
            "import { makeCallback } from './factory'; export const invoke = () => callback('clicked'); const callback = makeCallback('module').callback;",
        ),
        (
            "src/Host.tsx",
            "import { invoke } from './callbacks'; export function Host() { return <button onClick={invoke} />; }",
        ),
    ]);
    let report = fixture.report();
    assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].conclusion,
        Conclusion::CandidateInvocation
    );
    assert!(!report.creations[0].registrations.is_empty());
}

#[test]
fn branch_local_enums_and_constants_do_not_replace_outer_bindings() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import { makeCallback } from './factory'; enum Kind { Value = 'outer' } export function Host() { const reason = 'outer-reason'; if (true) { enum Kind { Value = 'inner' } const reason = 'inner-reason'; } makeCallback(Kind.Value).callback(reason); }",
    )]);
    let report = fixture.report();
    assert!(report.coverage.complete);
    assert_eq!(
        string(&report.creations[0].factory_arguments["created"]),
        "outer"
    );
    assert_eq!(
        string(&report.creations[0].invocations[0].arguments["invoked"]),
        "outer-reason"
    );
}

#[test]
fn star_conflicts_are_ambiguous_but_explicit_exports_and_diamond_paths_resolve() {
    let fixture = TestProject::new(&[
        (
            "src/other.ts",
            "export function makeCallback(_value: string) {}",
        ),
        (
            "src/conflict.ts",
            "export * from './factory'; export * from './other';",
        ),
        ("src/one.ts", "export * from './factory';"),
        ("src/two.ts", "export { makeCallback } from './factory';"),
        (
            "src/cycle-a.ts",
            "export * from './cycle-b'; export * from './factory';",
        ),
        ("src/cycle-b.ts", "export * from './cycle-a';"),
        (
            "src/Host.tsx",
            "import { makeCallback } from './barrel'; export function Host() { makeCallback('created').callback('invoked'); }",
        ),
    ]);
    for barrel in [
        "export * from './factory'; export * from './other';",
        "export * from './other'; export * from './factory';",
        "export { makeCallback } from './conflict';",
        "import { makeCallback as local } from './conflict'; export { local as makeCallback };",
    ] {
        fixture.write("src/barrel.ts", barrel);
        let analyzer = fixture.analyzer();
        let snapshot = analyzer.index().expect("index fixture");
        let host = snapshot
            .files
            .iter()
            .find(|file| file.path.ends_with("Host.tsx"))
            .expect("host");
        assert_eq!(
            SymbolLinker::new(analyzer.project(), &snapshot)
                .resolve_binding(host.file_id, "makeCallback"),
            ValueResolution::Ambiguous
        );
        let report = fixture.report();
        assert!(report.creations.is_empty());
        assert!(!report.coverage.complete);
        assert!(
            report
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.contains("ambiguous value linkage"))
        );
    }
    for barrel in [
        "export * from './other'; export { makeCallback } from './factory';",
        "export { makeCallback } from './factory'; export * from './other';",
        "export * from './one'; export * from './two';",
        "export * from './cycle-a'; export * from './cycle-b';",
        "import { makeCallback as local } from './factory'; export { local as makeCallback };",
    ] {
        fixture.write("src/barrel.ts", barrel);
        let report = fixture.report();
        assert!(
            report.coverage.complete,
            "{barrel}: {:?}",
            report.coverage.gaps
        );
        assert_eq!(report.creations.len(), 1);
        assert_eq!(
            report.creations[0].conclusion,
            Conclusion::CandidateInvocation
        );
    }
}

#[test]
fn namespace_conflicts_and_type_only_imports_do_not_match_value_declarations() {
    let fixture = TestProject::new(&[
        ("src/other.ts", "export const value = 'other';"),
        ("src/one.ts", "export * as api from './factory';"),
        ("src/two.ts", "export * as api from './other';"),
        (
            "src/barrel.ts",
            "export * from './one'; export * from './two';",
        ),
        (
            "src/Host.tsx",
            "import { api } from './barrel'; import type { makeCallback as TypeFactory } from './factory'; export function Host() { api.makeCallback('created'); }",
        ),
    ]);
    let analyzer = fixture.analyzer();
    let snapshot = analyzer.index().expect("index fixture");
    let host = snapshot
        .files
        .iter()
        .find(|file| file.path.ends_with("Host.tsx"))
        .expect("host");
    let linker = SymbolLinker::new(analyzer.project(), &snapshot);
    assert_eq!(
        linker.resolve_binding(host.file_id, "api"),
        ValueResolution::Ambiguous
    );
    assert_eq!(
        linker.resolve_binding(host.file_id, "TypeFactory"),
        ValueResolution::Missing
    );
    assert!(!fixture.report().coverage.complete);
    fixture.write("src/two.ts", "export * as api from './factory';");
    let report = fixture.report();
    assert!(report.coverage.complete);
    assert_eq!(report.creations.len(), 1);
}

#[test]
fn ambiguous_factory_matchers_fail_instead_of_reporting_an_empty_success() {
    let fixture = TestProject::new(&[
        ("src/other.ts", "export function makeCallback() {}"),
        (
            "src/barrel.ts",
            "export * from './factory'; export * from './other';",
        ),
        ("src/Host.tsx", "export function Host() {}"),
    ]);
    let mut query = fixture.query();
    query.factory.module = "src/barrel.ts".into();
    let error = fixture
        .analyzer()
        .query(&query, "test")
        .expect_err("ambiguous matcher");
    assert!(
        error
            .to_string()
            .contains("does not resolve to one value declaration")
    );
}

#[test]
fn local_bindings_shadow_namespace_imports_and_same_named_enums() {
    let fixture = TestProject::new(&[(
        "src/Host.tsx",
        "import * as api from './factory';\nimport { makeCallback } from './factory';\nenum Kind { Value = 'module' }\nfunction first() { enum Kind { Value = 'first' } return Kind.Value; }\nfunction second() { enum Kind { Value = 'second' } return Kind.Value; }\nexport function Host() { const api = { makeCallback: (value) => value }; api.makeCallback('decoy'); makeCallback(first()).callback(second()); }",
    )]);
    let report = fixture.report();
    assert!(report.coverage.complete, "{:?}", report.coverage.gaps);
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        string(&report.creations[0].factory_arguments["created"]),
        "first"
    );
    assert_eq!(
        string(&report.creations[0].invocations[0].arguments["invoked"]),
        "second"
    );
    let mut query = fixture.query();
    query.scope = QueryScope::AllCreations;
    let report = fixture
        .analyzer()
        .query(&query, "test")
        .expect("all creations");
    assert_eq!(
        report.creations.len(),
        1,
        "shadowed namespace call is not a factory creation"
    );
}

#[test]
fn unresolved_star_paths_and_cyclic_value_initialization_are_coverage_gaps() {
    let fixture = TestProject::new(&[
        (
            "src/barrel.ts",
            "export * from './factory'; export * from './missing';",
        ),
        (
            "src/Host.tsx",
            "import { makeCallback } from './barrel'; export function Host() { makeCallback('created'); }",
        ),
    ]);
    assert!(!fixture.report().coverage.complete);
    fixture.write("src/a.ts", "import { b } from './b'; export const a = b;");
    fixture.write("src/b.ts", "import { a } from './a'; export const b = a;");
    fixture.write("src/Host.tsx", "import { a } from './a'; import { makeCallback } from './factory'; export function Host() { makeCallback(a).callback('invoked'); }");
    let report = fixture.report();
    assert!(!report.coverage.complete);
    assert!(!report.creations[0].unresolved.is_empty());
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("cyclic module value initialization"))
    );
}

#[test]
fn cyclic_reexports_with_no_definition_terminate_as_missing() {
    let fixture = TestProject::new(&[
        ("src/a.ts", "export { absent } from './b';"),
        ("src/b.ts", "export { absent } from './a';"),
        (
            "src/Host.tsx",
            "import { absent } from './a'; export function Host() { absent(); }",
        ),
    ]);
    let analyzer = fixture.analyzer();
    let snapshot = analyzer.index().expect("index");
    let host = snapshot
        .files
        .iter()
        .find(|file| file.path.ends_with("Host.tsx"))
        .expect("host");
    assert_eq!(
        SymbolLinker::new(analyzer.project(), &snapshot).resolve_binding(host.file_id, "absent"),
        ValueResolution::Missing
    );
    assert!(!fixture.report().coverage.complete);
}
