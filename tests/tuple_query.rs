mod support;

use code_flow::{
    Project,
    queries::Conclusion,
    query::{QueryCallsiteStatus, QueryGapAssessment, QueryGapTarget, QueryValue, load_query},
};
use support::TestProject;

fn collect_enum_members(value: &QueryValue, members: &mut Vec<String>) {
    match value {
        QueryValue::EnumMember { member_name, .. } => members.push(member_name.clone()),
        QueryValue::Array { elements } => {
            for element in elements {
                collect_enum_members(element, members);
            }
        }
        QueryValue::Alternatives { values } => {
            for value in values {
                collect_enum_members(value, members);
            }
        }
        _ => {}
    }
}

fn contains_unknown(value: &QueryValue) -> bool {
    match value {
        QueryValue::Unknown { .. } => true,
        QueryValue::Array { elements } => elements.iter().any(contains_unknown),
        QueryValue::Alternatives { values } => values.iter().any(contains_unknown),
        _ => false,
    }
}

#[test]
fn text_prefilter_keeps_matching_directory_files_and_explicit_support_files() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection() { return [null, () => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; export function Host() { const [, apply] = useItemSelection(); apply('close'); }",
        ),
        ("src/Unrelated.ts", "export const unrelated = true;"),
        ("src/Skipped.ts", "export const skipped = true;"),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src', 'src/Unrelated.ts']\nsource_contains_any = ['useItemSelection']\n",
    );
    let project = Project::load(fixture.root.join("flow.toml")).expect("load project");
    let sources = project.discover_sources().expect("discover sources");
    assert_eq!(sources.len(), 3);
    assert!(sources.iter().any(|path| path.ends_with("Unrelated.ts")));
    assert!(!sources.iter().any(|path| path.ends_with("Skipped.ts")));
    let snapshot = fixture.analyzer().index().expect("index selected sources");
    assert_eq!(snapshot.files.len(), 3);
}

#[test]
fn filtered_query_follows_callback_bearing_imports_across_multiple_files() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; import { Bridge } from './Bridge'; export function Host() { const [, apply] = useItemSelection(['alpha']); return <Bridge onDone={apply} />; }",
        ),
        (
            "src/Bridge.tsx",
            "import { Deep } from './Deep'; export function Bridge({onDone}: {onDone: (action: string) => void}) { return <Deep finish={onDone} />; }",
        ),
        (
            "src/Deep.tsx",
            "export function Deep({finish}: {finish: (action: string) => void}) { finish('submit'); return null; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].invocations.len(), 1);
    assert_eq!(
        report.creations[0].invocations[0].arguments["action"],
        QueryValue::String {
            value: "submit".to_owned()
        }
    );
}

#[test]
fn filtered_query_follows_reverse_importers_of_wrapper_returning_capability() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_items: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/wrapper.ts",
            "import { useItemSelection } from './hook'; export function useWidget() { const [, apply] = useItemSelection(['alpha']); return {apply}; }",
        ),
        (
            "src/Host.ts",
            "import { useWidget } from './wrapper'; export function Host() { const {apply} = useWidget(); apply('close'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 3,
        "{:?}",
        report.coverage.gaps
    );
    assert!(
        report
            .creations
            .iter()
            .any(|creation| creation.invocations.iter().any(
                |invocation| invocation.arguments["action"]
                    == QueryValue::String {
                        value: "close".to_owned()
                    }
            ))
    );
}

#[test]
fn filtered_query_follows_reverse_importers_of_exported_arrow_wrapper() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_items: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/wrapper.ts",
            "import { useItemSelection } from './hook'; export const useWidget = () => { const [, apply] = useItemSelection(['alpha']); return {apply}; };",
        ),
        (
            "src/Host.ts",
            "import { useWidget } from './wrapper'; export function Host() { const {apply} = useWidget(); apply('close'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 3,
        "{:?}",
        report.coverage.gaps
    );
    assert!(
        report
            .creations
            .iter()
            .any(|creation| creation.invocations.iter().any(
                |invocation| invocation.arguments["action"]
                    == QueryValue::String {
                        value: "close".to_owned()
                    }
            ))
    );
}

#[test]
fn filtered_query_follows_reverse_importers_through_reexport_barrels() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_items: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/wrapper.ts",
            "import { useItemSelection } from './hook'; export function useWidget() { const [, apply] = useItemSelection(['alpha']); return {apply}; }",
        ),
        ("src/barrel.ts", "export { useWidget } from './wrapper';"),
        (
            "src/Host.ts",
            "import { useWidget } from './barrel'; export function Host() { const {apply} = useWidget(); apply('close'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    assert!(
        report
            .creations
            .iter()
            .any(|creation| creation.invocations.iter().any(
                |invocation| invocation.arguments["action"]
                    == QueryValue::String {
                        value: "close".to_owned()
                    }
            ))
    );
}

#[test]
fn filtered_query_keeps_candidate_calls_inside_callbacks_passed_to_known_consumers() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection() { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; import { consume } from './consume'; export function Host() { const [, apply] = useItemSelection(); consume(() => apply('close')); return null; }",
        ),
        (
            "src/consume.ts",
            "export function consume(_callback: () => void) { return undefined; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.coverage.processed_files, 3);
    assert_eq!(report.creations[0].invocations.len(), 1);
    assert_eq!(
        report.creations[0].invocations[0].arguments["action"],
        QueryValue::String {
            value: "close".to_owned()
        }
    );
    assert!(!report.coverage.complete);
}

#[test]
fn resolves_project_aliases_and_correlates_tuple_callback_with_branch_built_numeric_enums() {
    let fixture = TestProject::new(&[
        (
            "src/values.ts",
            "export enum ItemKind { A = 1, B = 2 } export enum Action { CONFIRM = 'confirm' }",
        ),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: ItemKind[]) { return [null, (_action: Action) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { ItemKind, Action } from '@sample/values'; import { useItemSelection } from '@sample/hook'; export default function Host({flag}: {flag: boolean}) { const types: ItemKind[] = []; if (flag) { types.push(ItemKind.A); } else { types.push(ItemKind.B); } const [visible, applyAction] = useItemSelection(types); applyAction(Action.CONFIRM); return visible; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src/../src']\n[import_aliases]\n'@sample/*' = 'src/*'\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'default'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'contents'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let (query, _) = load_query(&fixture.root.join("query.toml")).expect("load tuple query");
    assert_eq!(query.capability.returned_index, Some(1));
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    let creation = &report.creations[0];
    assert_eq!(creation.conclusion, Conclusion::CandidateInvocation);
    assert_eq!(creation.invocations.len(), 1);
    let mut members = Vec::new();
    collect_enum_members(&creation.factory_arguments["contents"], &mut members);
    members.sort();
    assert_eq!(members, ["A", "B"]);
    assert_eq!(
        creation.invocations[0].arguments["action"],
        QueryValue::String {
            value: "confirm".to_owned()
        }
    );
}

#[test]
fn configured_imported_selector_invokes_its_callback_argument() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum ItemKind { A = 1 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: ItemKind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { selectFromStore } from '@sample/state'; import { ItemKind } from './values'; import { useItemSelection } from './hook'; export function Host() { const items = selectFromStore([], () => [ItemKind.A]); const [, apply] = useItemSelection(items); apply('submit'); }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[callback_selector_imports]]\nmodule = '@sample/state'\nexport = 'selectFromStore'\ncallback_argument = 1\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A"]);
    assert_eq!(report.creations[0].invocations.len(), 1);
}

#[test]
fn finite_filter_then_map_preserves_only_selected_enum_values() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1, B = 2 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { useItemSelection } from './hook'; export function Host() { const rows = [{enabled: true, kind: Kind.A}, {enabled: false, kind: Kind.B}]; const items = rows.filter(row => row.enabled).map(row => row.kind); const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A"]);
}

#[test]
fn finite_map_receives_the_array_index() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1, B = 2 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { useItemSelection } from './hook'; export function Host() { const items = [Kind.A, Kind.A].map((kind, index) => index === 0 ? kind : Kind.B); const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A", "B"]);
}

#[test]
fn filtered_query_resolves_imported_predicate_for_dynamic_array() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1 }"),
        (
            "src/predicate.ts",
            "export function isPresent<T>(value: T | null | undefined): value is T { return value != null; }",
        ),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { isPresent } from './predicate'; import { useItemSelection } from './hook'; export function Host(flag: boolean) { const items = [flag ? Kind.A : undefined, undefined].filter(isPresent); const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A"]);
    assert!(
        report
            .creations
            .iter()
            .all(|creation| !contains_unknown(&creation.factory_arguments["items"]))
    );
}

#[test]
fn guarded_record_property_can_feed_a_dynamic_array() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { useItemSelection } from './hook'; function getConfig(code: number) { switch (code) { case 1: return {kind: Kind.A}; default: return null; } } export function Host(code: number, ready: boolean) { const config = getConfig(code); const items: Kind[] = []; if (config != null && ready) items.push(config.kind); const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    assert!(
        report
            .creations
            .iter()
            .all(|creation| !contains_unknown(&creation.factory_arguments["items"]))
    );
    assert!(report.creations.iter().any(|creation| {
        let mut members = Vec::new();
        collect_enum_members(&creation.factory_arguments["items"], &mut members);
        members.contains(&"A".to_owned())
    }));
}

#[test]
fn object_values_and_array_spreads_preserve_finite_candidates() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { useItemSelection } from './hook'; export function Host() { const rows = {first: {enabled: true, kind: Kind.A}, second: {enabled: false, kind: Kind.B}}; const selected = Object.values(rows).filter(row => row.enabled).map(row => row.kind); const items = [...selected, Kind.C]; const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A", "C"]);
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("Object.values record key order is not modeled"))
    );
}

#[test]
fn filtered_query_follows_imported_data_used_to_build_factory_array() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1, B = 2 }"),
        (
            "src/data.ts",
            "import { Kind } from './values'; export const rows = [{enabled: true, kind: Kind.A}, {enabled: false, kind: Kind.B}];",
        ),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { rows } from './data'; import { useItemSelection } from './hook'; export function Host() { const items = rows.filter(row => row.enabled).map(row => row.kind); const [, apply] = useItemSelection(items); apply('close'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A"]);
}

#[test]
fn nullish_logical_and_loose_null_guards_keep_candidate_values() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { A = 1 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_items: Kind[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './values'; import { useItemSelection } from './hook'; export function Host() { const missing = null; const items = missing ?? [Kind.A]; const [, apply] = useItemSelection(items); if (!false && missing == null) apply('close'); return null; }",
        ),
    ]);
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].invocations.len(), 1);
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["items"],
        &mut members,
    );
    assert_eq!(members, ["A"]);
}

#[test]
fn unresolved_factory_import_does_not_claim_complete_all_creations_coverage() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, () => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from '@sample/hook'; export function Host() { const [, apply] = useItemSelection([]); apply('close'); }",
        ),
    ]);
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n",
    );
    let report = fixture.report();
    assert!(report.creations.is_empty());
    assert!(!report.coverage.complete);
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("possible factory call has unresolved import"))
    );
}

#[test]
fn optional_body_scan_finds_actions_inside_opaque_callbacks_and_jsx_props() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, () => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; export function Host() { const [, apply] = useItemSelection(['alpha']); opaque(() => [{onClick: () => apply('submit')}]); return <Popover onRequestClose={() => apply('confirm')} />; }",
        ),
    ]);
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let creation = &report.creations[0];
    let mut actions = creation
        .invocations
        .iter()
        .map(|invocation| invocation.arguments["action"].clone())
        .collect::<Vec<_>>();
    actions.sort_by_key(|action| format!("{action:?}"));
    assert_eq!(
        actions,
        [
            QueryValue::String {
                value: "confirm".to_owned()
            },
            QueryValue::String {
                value: "submit".to_owned()
            },
        ]
    );
    assert!(!report.coverage.complete);
}

#[test]
fn optional_body_scan_follows_nested_effect_cleanup() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_types: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import * as React from 'react'; import { useItemSelection } from './hook'; export function Host() { const [, apply] = useItemSelection(['alpha']); const [ready] = React.useState(false); React.useEffect(() => { return () => { if (ready) apply('close'); }; }, [ready, apply]); return null; }",
        ),
    ]);
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].invocations.len(),
        1,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(
        report.creations[0].invocations[0].arguments["action"],
        QueryValue::String {
            value: "close".to_owned()
        }
    );
}

#[test]
fn all_creations_explores_memo_function_expressions_filtered_arrays_and_switch_callbacks() {
    let fixture = TestProject::new(&[
        ("src/values.ts", "export enum Kind { PRIMARY = 7 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, () => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import * as React from 'react'; import { Kind } from './values'; import { useItemSelection } from './hook'; export const Host = React.memo(function Host() { const types = [Kind.PRIMARY, undefined].filter((value) => value != null); const [, apply] = useItemSelection(types); opaque(() => { switch (Kind.PRIMARY) { case Kind.PRIMARY: apply('close'); break; default: break; } }); return null; });",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'contents'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    let creation = &report.creations[0];
    let mut members = Vec::new();
    collect_enum_members(&creation.factory_arguments["contents"], &mut members);
    assert!(members.contains(&"PRIMARY".to_owned()));
    assert_eq!(creation.invocations.len(), 1);
    assert_eq!(
        creation.invocations[0].arguments["action"],
        QueryValue::String {
            value: "close".to_owned()
        }
    );
    assert!(!report.coverage.complete);
}

#[test]
fn filtered_query_traces_finite_props_from_imported_component_callers() {
    let fixture = TestProject::new(&[
        ("src/kinds.ts", "export enum Kind { FIRST = 1, SECOND = 2 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; export function Host({types}: {types: unknown[]}) { const [, apply] = useItemSelection(types); apply('close'); return null; }",
        ),
        ("src/barrel.ts", "export { Host as Panel } from './Host';"),
        (
            "src/Parent.tsx",
            "import { Kind } from './kinds'; import { Panel } from './barrel'; export function Parent() { return <Panel types={[Kind.FIRST, Kind.SECOND]} />; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write(
        "src/Parent.tsx",
        &format!("{}{}", "import { Kind } from './kinds'; import { Panel } from './barrel'; export function Parent() { const types = [Kind.FIRST]; types.push(Kind.SECOND); return <Panel types={types} />; }", " ".repeat(20_100)),
    );
    fixture.write(
        "src/ParentCall.ts",
        &format!("{}{}", "import { Kind } from './kinds'; import { Host } from './Host'; export function ParentCall() { return Host({types: [Kind.SECOND]}); }", " ".repeat(20_100)),
    );
    fixture.write(
        "src/ParentBranch.tsx",
        &format!("{}{}", "import { Kind } from './kinds'; import { Panel } from './barrel'; export function ParentBranch({flag}: {flag: boolean}) { const types = [Kind.FIRST]; if (flag) types.push(Kind.SECOND); return <Panel types={types} />; }", " ".repeat(20_100)),
    );
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'types'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 7,
        "{:?}",
        report.coverage.gaps
    );
    assert!(
        report.creations.iter().any(|creation| {
            let mut members = Vec::new();
            collect_enum_members(&creation.factory_arguments["types"], &mut members);
            members == ["FIRST", "SECOND"]
                && creation.invocations.iter().any(|invocation| {
                    invocation.arguments["action"]
                        == QueryValue::String {
                            value: "close".to_owned(),
                        }
                })
        }),
        "{:?}",
        report
            .creations
            .iter()
            .map(|creation| &creation.factory_arguments)
            .collect::<Vec<_>>()
    );
    assert!(report.creations.iter().any(|creation| {
        let mut members = Vec::new();
        collect_enum_members(&creation.factory_arguments["types"], &mut members);
        members == ["SECOND"]
    }));
    assert!(report.creations.iter().any(|creation| {
        let QueryValue::Alternatives { values } = &creation.factory_arguments["types"] else {
            return false;
        };
        let members = values
            .iter()
            .map(|value| {
                let mut members = Vec::new();
                collect_enum_members(value, &mut members);
                members
            })
            .collect::<Vec<_>>();
        members.contains(&vec!["FIRST".to_owned()])
            && members.contains(&vec!["FIRST".to_owned(), "SECOND".to_owned()])
    }));
}

#[test]
fn filtered_query_binds_props_into_exported_arrow_components() {
    let fixture = TestProject::new(&[
        ("src/kinds.ts", "export enum Kind { FIRST = 1 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; export const Host = ({types}: {types: unknown[]}) => { const [, apply] = useItemSelection(types); apply('close'); return null; };",
        ),
        (
            "src/Parent.tsx",
            "import { Kind } from './kinds'; import { Host } from './Host'; export function Parent() { return <Host types={[Kind.FIRST]} />; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'types'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    assert!(report.creations.iter().any(|creation| {
        let mut members = Vec::new();
        collect_enum_members(&creation.factory_arguments["types"], &mut members);
        members == ["FIRST"]
    }));
}

#[test]
fn helper_built_arrays_and_record_alias_mutations_reach_factory_arguments() {
    let fixture = TestProject::new(&[
        ("src/kinds.ts", "export enum Kind { FIRST = 1, SECOND = 2 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/helper.ts",
            "import { Kind } from './kinds'; export function buildTypes() { return [Kind.FIRST]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './kinds'; import { buildTypes } from './helper'; import { useItemSelection } from './hook'; export function Host() { const state = {types: buildTypes()}; const alias = state; const arrayAlias = alias.types; arrayAlias.push(Kind.SECOND); const [, apply] = useItemSelection(state.types); apply('close'); return null; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'types'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["types"],
        &mut members,
    );
    assert_eq!(members, ["FIRST", "SECOND"]);
    assert_eq!(report.creations[0].invocations.len(), 1);
}

#[test]
fn mutations_through_helper_parameters_update_shared_record_and_array_values() {
    let fixture = TestProject::new(&[
        ("src/kinds.ts", "export enum Kind { FIRST = 1, SECOND = 2 }"),
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { Kind } from './kinds'; import { useItemSelection } from './hook'; function replace(state: {types: unknown[]}) { const alias = state; alias.types = [Kind.SECOND]; } function append(items: unknown[]) { const alias = items; alias.push(Kind.FIRST); } export function Host() { const state = {types: []}; replace(state); append(state.types); const [, apply] = useItemSelection(state.types); apply('close'); return null; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'types'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    let mut members = Vec::new();
    collect_enum_members(
        &report.creations[0].factory_arguments["types"],
        &mut members,
    );
    assert_eq!(members, ["SECOND", "FIRST"]);
}

#[test]
fn callsite_inventory_marks_analyzed_filtered_and_unresolved_candidates() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, () => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; // this-trace\nexport function Host() { useItemSelection(['one']); return null; }",
        ),
        (
            "src/Filtered.ts",
            "import { useItemSelection } from './hook'; export function filtered() { useItemSelection(['two']); }",
        ),
        (
            "src/barrel.ts",
            "export { useItemSelection as select } from './hook';",
        ),
        (
            "src/FilteredAlias.ts",
            "import { select } from './barrel'; export function filteredAlias() { select(['three']); }",
        ),
        (
            "src/Unrelated.ts",
            "// this-trace\nfunction useItemSelection() {} export function unrelated() { useItemSelection(); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src', 'src/hook.ts']\nsource_contains_any = ['this-trace']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n");
    let report = fixture.report();
    let inventory = &report.callsite_inventory;
    assert_eq!(inventory.configured_files, 7);
    assert!(
        inventory
            .callsites
            .iter()
            .any(|site| site.location.path == "src/Host.tsx"
                && site.status == QueryCallsiteStatus::Analyzed)
    );
    assert!(
        inventory
            .callsites
            .iter()
            .any(|site| site.location.path == "src/Filtered.ts"
                && site.status == QueryCallsiteStatus::Filtered)
    );
    assert!(
        inventory
            .callsites
            .iter()
            .any(|site| site.location.path == "src/FilteredAlias.ts"
                && site.status == QueryCallsiteStatus::Filtered)
    );
    assert!(
        inventory
            .callsites
            .iter()
            .any(|site| site.location.path == "src/Unrelated.ts"
                && site.status == QueryCallsiteStatus::Unresolved)
    );
    let filtered_gap = report
        .gaps
        .iter()
        .find(|gap| {
            gap.kind == "filtered_callsite"
                && gap
                    .location
                    .as_ref()
                    .is_some_and(|location| location.path == "src/FilteredAlias.ts")
        })
        .expect("filtered callsite gap");
    assert_eq!(filtered_gap.assessment, QueryGapAssessment::MayAffect);
    assert!(filtered_gap.links.iter().any(|link| {
        link.target == QueryGapTarget::Callsite
            && link.callsite_index.is_some_and(|index| {
                inventory.callsites[index].location.path == "src/FilteredAlias.ts"
            })
    }));
    assert!(
        !report
            .gaps
            .iter()
            .any(|gap| gap.kind == "unresolved_callsite")
    );
}

#[test]
fn callback_property_mutation_is_visible_through_record_alias() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_types: unknown[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; export function Host() { const [, apply] = useItemSelection(['one']); const state = {run: () => {}}; const alias = state; alias.run = apply; state.run('close'); return null; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].invocations[0].arguments["action"],
        QueryValue::String {
            value: "close".to_owned()
        }
    );
}
