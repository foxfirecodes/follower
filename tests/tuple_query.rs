mod support;

use code_flow::{
    Project,
    queries::Conclusion,
    query::{
        QueryBoundaryKind, QueryCallPathKind, QueryCallsiteStatus, QueryGapAssessment,
        QueryGapTarget, QueryReverseImporterEvaluation, QueryScope, QueryValue, Reachability,
        load_query,
    },
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
fn configured_entry_is_indexed_without_matching_the_text_filter() {
    let fixture = TestProject::new(&[
        ("src/Host.tsx", "export function Host() { return null; }"),
        ("src/Unrelated.ts", "export const unrelated = true;"),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['makeCallback']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n",
    );
    let sources = Project::load(fixture.root.join("flow.toml"))
        .unwrap()
        .discover_sources()
        .unwrap();
    assert!(sources.iter().any(|path| path.ends_with("Host.tsx")));
    assert!(sources.iter().any(|path| path.ends_with("factory.ts")));
    assert!(!sources.iter().any(|path| path.ends_with("Unrelated.ts")));
}

#[test]
fn backward_use_walk_finds_filtered_component_chain_to_entry() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; function useLocal() { const [, apply] = useItemSelection(['alpha']); apply('close'); } export function Leaf() { useLocal(); return null; }",
        ),
        (
            "src/Middle.tsx",
            "import { Leaf } from './Leaf'; export function Middle() { return <Leaf />; }",
        ),
        (
            "src/App.tsx",
            "import { Middle } from './Middle'; export function App() { return <Middle />; }",
        ),
        ("src/Noise.tsx", "export function Noise() { return null; }"),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(
        report.coverage.processed_files, 4,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
    assert_eq!(report.creations[0].invocations.len(), 1);
    assert_eq!(
        report.creations[0].invocations[0].arguments["action"],
        QueryValue::String {
            value: "close".to_owned()
        }
    );
}

#[test]
fn filtered_root_path_links_components_through_star_export_barrels() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/App.tsx",
            "import Session from './session'; import { Frame, Panel } from './ui'; import { Leaf } from './Leaf'; export function App() { return <Session.Provider value={null}><Frame><Panel><Leaf /></Panel></Frame></Session.Provider>; }",
        ),
        (
            "src/session.ts",
            "import { createContext } from 'react'; export default createContext(null);",
        ),
        // The type-only import records no path for the same specifier as the value re-export.
        (
            "src/ui/index.ts",
            "import type * as frame from './Frame'; export * from './Frame'; export * from './Panel'; export * from './Unrelated'; export * from 'helpers';",
        ),
        (
            "src/ui/Frame.tsx",
            "export function Frame({ children }: { children: unknown }) { return <section>{children}</section>; }",
        ),
        (
            "src/ui/Panel.tsx",
            "export function Panel({ children }: { children: unknown }) { return <div>{children}</div>; }",
        ),
        (
            "src/ui/Unrelated.tsx",
            "export function Unrelated() { return null; }",
        ),
        (
            "node_modules/helpers/package.json",
            "{\"name\": \"helpers\", \"main\": \"index.js\"}",
        ),
        (
            "node_modules/helpers/index.js",
            "var react = require('react'); exports.format = function () { return react; };",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.creations[0].invocations.len(), 1);
    // The unrelated star target and the CommonJS package cannot export the names, so neither is
    // parsed: the hook, leaf, entry, context, barrel, frame, and panel are.
    assert_eq!(
        report.coverage.processed_files, 7,
        "{:?}",
        report.coverage.gaps
    );
}

const TUPLE_QUERY: &str = "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n";

const HOOK: (&str, &str) = (
    "src/hook.ts",
    "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
);

const LEAF: (&str, &str) = (
    "src/Leaf.tsx",
    "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
);

#[test]
fn contract_on_a_package_export_applies_through_re_exporting_barrels() {
    let fixture = TestProject::new(&[
        HOOK,
        LEAF,
        (
            "src/App.tsx",
            "import { Frame } from './ui'; import { Leaf } from './Leaf'; export function App() { return <Frame><Leaf /></Frame>; }",
        ),
        ("src/ui/index.ts", "export { Frame } from './frame';"),
        ("src/ui/frame.ts", "export { Frame } from 'external-frame';"),
    ]);
    fixture.write("query.toml", TUPLE_QUERY);
    let config = "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n";
    fixture.write("flow.toml", config);
    assert_eq!(
        fixture.report().creations[0].reachability,
        Reachability::Possible
    );
    fixture.write(
        "flow.toml",
        &format!(
            "{config}[[component_consumers]]\nmodule = 'external-frame'\nexport = 'Frame'\nforward_children = true\n"
        ),
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
}

#[test]
fn a_single_function_child_is_callable_and_text_children_are_strings() {
    let fixture = TestProject::new(&[
        HOOK,
        LEAF,
        (
            "src/Theme.tsx",
            "function Inner({ children }) { return <section>{children}</section>; } export function Theme({ children }) { return <Inner>{children('dark')}</Inner>; } export function Label({ children }) { return typeof children === 'string' ? <span>{children}</span> : null; }",
        ),
        (
            "src/App.tsx",
            "import { Theme, Label } from './Theme'; import { Leaf } from './Leaf'; export function App() {\n  return (\n    <Theme>\n      {(className) => <div className={className}><Label>  title\n  </Label><Leaf /></div>}\n    </Theme>\n  );\n}",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
}

#[test]
fn root_path_parses_the_higher_order_component_it_renders() {
    let fixture = TestProject::new(&[
        HOOK,
        LEAF,
        (
            "src/Panel.tsx",
            "import { Leaf } from './Leaf'; import { withBox } from './withBox'; function Panel() { return <Leaf />; } export const BoxedPanel = withBox(Panel);",
        ),
        (
            "src/withBox.tsx",
            "import * as React from 'react'; export function withBox(Component) { return React.forwardRef(function Boxed(props, ref) { return <div ref={ref}><Component {...props} /></div>; }); }",
        ),
        (
            "src/App.tsx",
            "import { BoxedPanel } from './Panel'; export function App() { return <BoxedPanel />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection', 'BoxedPanel']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
}

fn exact_actions(files: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let mut all = vec![
        HOOK,
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
    ];
    all.extend_from_slice(files);
    let fixture = TestProject::new(&all);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    let mut exact = Vec::new();
    let mut other = Vec::new();
    for creation in &report.creations {
        for invocation in &creation.invocations {
            let action = match &invocation.arguments["action"] {
                QueryValue::String { value } => value.clone(),
                value => format!("{value:?}"),
            };
            if creation.reachability == Reachability::Reachable {
                exact.push(action);
            } else {
                other.push(action);
            }
        }
    }
    exact.sort();
    exact.dedup();
    other.sort();
    (exact, other)
}

#[test]
fn missing_props_are_undefined_and_destructuring_defaults_apply() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import { Leaf } from './Leaf'; function label(text = 'unlabeled') { return text; } function Panel({ mode = 'compact', onClose }) { if (onClose != null) { return <Leaf action='closable' />; } return <div><Leaf action={mode} /><Leaf action={label()} /></div>; } export function App() { return <div><Panel /><Panel mode='wide' /></div>; }",
    )]);
    assert_eq!(exact, ["compact", "unlabeled", "wide"]);
    assert!(other.is_empty(), "{other:?}");
}

#[test]
fn object_spread_keeps_known_properties_of_each_alternative() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import { Leaf } from './Leaf'; import { extra } from 'external-values'; function Base({ children, hover }) { const base = { kind: 'base', ...(hover ? { kind: 'hovered' } : {}) }; return children({ ...base, size: 'md' }); } function Open() { const merged = { action: 'listed', ...extra }; return <Leaf action={merged.action} />; } export function App() { return <div><Base hover={false}>{(props) => <Leaf action={props.kind} />}</Base><Base hover>{(props) => <Leaf action={props.size} />}</Base><Open /></div>; }",
    )]);
    // A spread of an unknown value may overwrite properties listed before it, so that read is a
    // joined unknown; the other spreads keep exact values.
    assert_eq!(
        exact,
        ["Unknown { reason: \"joined_alternatives\" }", "base", "md"],
        "{other:?}"
    );
}

#[test]
fn default_props_fill_missing_class_and_function_component_props() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import * as React from 'react'; import { Leaf } from './Leaf'; class Card extends React.Component { static defaultProps = { action: 'class-default' }; render() { return <Leaf action={this.props.action} />; } } function Tile({ action }) { return <Leaf action={action} />; } Tile.defaultProps = { action: 'function-default' }; export function App() { return <div><Card /><Card action='explicit' /><Tile /></div>; }",
    )]);
    assert_eq!(
        exact,
        ["class-default", "explicit", "function-default"],
        "{other:?}"
    );
}

#[test]
fn component_boundaries_suggest_contracts_that_make_creations_reachable() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        ("src/ui.ts", "export { Frame } from 'external-frame';"),
        (
            "src/App.tsx",
            "import { Frame } from './ui'; import { withTracking } from 'external-tracking'; import { Leaf } from './Leaf'; function Panel({ action }) { return <section><Leaf action={action} /></section>; } const TrackedPanel = withTracking(Panel); export function App() { return <div><Frame><Leaf action='framed' /></Frame><TrackedPanel action='tracked' /></div>; }",
        ),
    ]);
    let config = "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n";
    fixture.write("flow.toml", config);
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    assert_eq!(report.schema_version, 9);
    assert!(
        report
            .creations
            .iter()
            .all(|creation| creation.reachability == Reachability::Possible)
    );
    let frame = report
        .component_boundaries
        .iter()
        .find(|boundary| boundary.component == "Frame")
        .expect("frame boundary");
    assert_eq!(frame.kind, QueryBoundaryKind::ExternalPackage);
    assert_eq!(frame.module.as_deref(), Some("external-frame"));
    assert!(frame.entered_from_reachable);
    assert_eq!(frame.sole_blocker_creations, 1);
    let tracked = report
        .component_boundaries
        .iter()
        .find(|boundary| boundary.component == "TrackedPanel")
        .expect("wrapper boundary");
    assert_eq!(tracked.kind, QueryBoundaryKind::DynamicValue);
    assert_eq!(
        tracked.suggested_contract.as_deref(),
        Some(
            "[[component_wrappers]]\nmodule = \"external-tracking\"\nexport = \"withTracking\"\ncomponent_argument = 0\n"
        )
    );
    let contracts = report
        .component_boundaries
        .iter()
        .filter_map(|boundary| boundary.suggested_contract.clone())
        .collect::<Vec<_>>()
        .join("\n");
    fixture.write("flow.toml", &format!("{config}\n{contracts}"));
    let report = fixture.report();
    assert_eq!(report.creations.len(), 2, "{:?}", report.coverage.gaps);
    assert!(
        report
            .creations
            .iter()
            .all(|creation| creation.reachability == Reachability::Reachable),
        "{contracts}"
    );
    assert!(report.component_boundaries.is_empty());
}

#[test]
fn all_creations_explores_configured_roots_when_expansion_budget_stops() {
    let mut files = vec![
        (
            "src/hook.ts".to_owned(),
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }".to_owned(),
        ),
        (
            "src/App.tsx".to_owned(),
            "import { Host0 } from './Host0'; export function App() { return <Host0 />; }"
                .to_owned(),
        ),
    ];
    // Each host's factory argument lives in an unfiltered file, so expansion requests more files
    // than its budget allows in the first round.
    for index in 0..260 {
        files.push((
            format!("src/Host{index}.tsx"),
            format!(
                "import {{ useItemSelection }} from './hook'; import {{ values }} from './values{index}'; export function Host{index}() {{ const [, apply] = useItemSelection(values); apply('close'); return null; }}"
            ),
        ));
        files.push((
            format!("src/values{index}.ts"),
            "export const values = ['alpha'];".to_owned(),
        ));
    }
    let files = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let fixture = TestProject::new(&files);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert!(
        report
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.starts_with("capability import expansion stopped")),
        "{:?}",
        report.coverage.gaps
    );
    let rendered = report
        .creations
        .iter()
        .filter(|creation| {
            creation
                .factory_location
                .as_ref()
                .is_some_and(|location| location.path.ends_with("Host0.tsx"))
        })
        .collect::<Vec<_>>();
    assert_eq!(rendered.len(), 1);
    assert_eq!(rendered[0].reachability, Reachability::Reachable);
    assert_eq!(rendered[0].invocations.len(), 1);
}

#[test]
fn backward_import_without_use_does_not_claim_reachability() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/Middle.tsx",
            "import { Leaf } from './Leaf'; export function Middle() { return <Leaf />; }",
        ),
        (
            "src/App.tsx",
            "import { Middle } from './Middle'; export function App() { return null; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].reachability, Reachability::Unknown);
    assert_eq!(report.coverage.processed_files, 3);
}

#[test]
fn backward_use_walk_crosses_filtered_reexport() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        ("src/barrel.ts", "export { Leaf as Feature } from './Leaf';"),
        (
            "src/App.tsx",
            "import { Feature } from './barrel'; export function App() { return <Feature />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
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
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
}

#[test]
fn backward_use_walk_crosses_class_render_and_helper_method() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/Panel.tsx",
            "import { Leaf } from './Leaf'; export class Panel { renderBody() { return <Leaf />; } render() { return this.renderBody(); } }",
        ),
        (
            "src/App.tsx",
            "import { Panel } from './Panel'; export function App() { return <Panel />; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.coverage.processed_files, 4);
    let path = &report.creations[0].invocations[0].call_path;
    assert_eq!(path.first().unwrap().kind, QueryCallPathKind::Entry);
    assert_eq!(path.last().unwrap().kind, QueryCallPathKind::Invocation);
    assert!(
        path.iter()
            .any(|step| step.kind == QueryCallPathKind::Factory)
    );
    let files = path
        .iter()
        .map(|step| step.location.path.as_str())
        .collect::<Vec<_>>();
    let app = files
        .iter()
        .position(|path| path.ends_with("src/App.tsx"))
        .unwrap();
    let panel = files
        .iter()
        .position(|path| path.ends_with("src/Panel.tsx"))
        .unwrap();
    let leaf = files
        .iter()
        .position(|path| path.ends_with("src/Leaf.tsx"))
        .unwrap();
    assert!(app < panel && panel < leaf, "{files:?}");
}

#[test]
fn configured_lazy_factory_connects_literal_dynamic_import_without_widening_parse() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/Page.tsx",
            "import { Leaf } from './Leaf'; export default function Page() { return <Leaf />; }",
        ),
        (
            "src/lazy.ts",
            "export function loadComponent(_options: unknown) { return null; }",
        ),
        (
            "src/App.tsx",
            "import { loadComponent } from './lazy'; const LazyPage = loadComponent({ load: () => import('./Page') }); export function App() { return <LazyPage />; }",
        ),
        ("src/Noise.tsx", "export function Noise() { return null; }"),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[lazy_component_factories]]\nmodule = './lazy'\nexport = 'loadComponent'\npromise_property = 'load'\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.coverage.processed_files, 4);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    let unmodeled = fixture.report();
    assert_eq!(unmodeled.creations[0].reachability, Reachability::Unknown);
}

#[test]
fn configured_route_render_prop_and_wrapper_connect_a_class_route_table() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/Chat.tsx",
            "import { Leaf } from './Leaf'; import * as React from 'react'; function Chat() { return <Leaf />; } export default React.memo(Chat);",
        ),
        (
            "src/wrap.tsx",
            "export function protect(Component: any) { return function Protected() { return <Component />; }; }",
        ),
        (
            "src/router.tsx",
            "export function Route(_props: any) { return null; }",
        ),
        (
            "src/View.tsx",
            "import Chat from './Chat'; import { protect } from './wrap'; import { Route } from './router'; const routed = protect(Chat); const routes = [{render: routed}]; export class View { render() { return <Route render={routes[0].render} />; } }",
        ),
        (
            "src/App.tsx",
            "import { View } from './View'; export function App() { return <View />; }",
        ),
        (
            "src/Unused.tsx",
            "export function Unused() { return null; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[component_wrappers]]\nmodule = './wrap'\nexport = 'protect'\ncomponent_argument = 0\n[[component_consumers]]\nmodule = './router'\nexport = 'Route'\nrender_props = ['render']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.coverage.gaps
    );
    assert_eq!(report.coverage.processed_files, 5);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[component_wrappers]]\nmodule = './wrap'\nexport = 'protect'\ncomponent_argument = 0\n[[component_consumers]]\nmodule = './other-router'\nexport = 'Route'\nrender_props = ['render']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    // Without a contract the route file is parsed. Its body is fully modeled and returns null,
    // so the route renders nothing and the leaf is not reached from the root.
    let unmodeled_route = fixture.report();
    assert_eq!(
        unmodeled_route.creations[0].reachability,
        Reachability::Unknown
    );
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[component_wrappers]]\nmodule = './wrap'\nexport = 'protect'\ncomponent_argument = 0\n[[component_consumers]]\nmodule = './router'\nexport = 'Route'\nrender_props = ['render']\nrender_callback_names = ['Elsewhere']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    let filtered_route = fixture.report();
    assert_eq!(
        filtered_route.creations[0].reachability,
        Reachability::Unknown
    );
    assert!(
        filtered_route
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("configured render callback filter"))
    );
}

#[test]
fn unmodeled_component_renders_children_and_jsx_props_as_possible_reachability() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/Page.tsx",
            "import { Leaf } from './Leaf'; export function Page() { return <Leaf action='component' />; }",
        ),
        (
            "src/App.tsx",
            "import { Frame } from 'external-frame'; import { useItemSelection } from './hook'; import { Leaf } from './Leaf'; import { Page } from './Page'; export function App() { const [, select] = useItemSelection(['beta']); return <div><Leaf action='direct' /><Frame title='x'><Leaf action='child' /></Frame><Frame>{() => <Leaf action='function-child' />}</Frame><Frame header={<Leaf action='element' />} render={() => <Leaf action='render' />} component={Page} onSelect={() => select('select')} /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let mut by_action = report
        .creations
        .iter()
        .flat_map(|creation| {
            creation.invocations.iter().map(move |invocation| {
                let QueryValue::String { value } = &invocation.arguments["action"] else {
                    panic!("action is not a string: {invocation:?}");
                };
                (value.clone(), creation, invocation)
            })
        })
        .collect::<Vec<_>>();
    by_action.sort_by(|left, right| left.0.cmp(&right.0));
    let actions = by_action
        .iter()
        .map(|(action, creation, _)| (action.as_str(), creation.reachability))
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            ("child", Reachability::Possible),
            ("component", Reachability::Possible),
            ("direct", Reachability::Reachable),
            ("element", Reachability::Possible),
            ("function-child", Reachability::Possible),
            ("render", Reachability::Possible),
        ],
        "{:?}",
        report.coverage.gaps
    );
    for (_, creation, invocation) in by_action
        .iter()
        .filter(|(_, creation, _)| creation.reachability == Reachability::Possible)
    {
        assert!(
            invocation
                .call_path
                .iter()
                .any(|step| step.kind == QueryCallPathKind::AssumedRender)
        );
        assert_ne!(creation.conclusion, Conclusion::AbsentWithinModel);
        assert!(report.gaps.iter().any(|gap| {
            gap.kind == "render_assumed_through_unmodeled_component"
                && gap.assessment == QueryGapAssessment::Direct
                && gap
                    .links
                    .iter()
                    .any(|link| link.creation_id.as_deref() == Some(creation.creation_id.as_str()))
        }));
    }
    let select = report
        .creations
        .iter()
        .find(|creation| {
            creation.reachability == Reachability::Reachable && creation.invocations.is_empty()
        })
        .expect("callback passed to the unmodeled component is not invoked");
    assert_eq!(select.conclusion, Conclusion::Unresolved);
}

#[test]
fn unmodeled_higher_order_component_renders_wrapped_component_as_possible() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/Panel.tsx",
            "import { withSize, connect } from 'external-hoc'; import { Leaf } from './Leaf'; function Panel({ action }: { action: string }) { return <Leaf action={action} />; } export const SizedPanel = withSize(Panel); export const ConnectedPanel = connect((_state: unknown) => ({}))(Panel);",
        ),
        (
            "src/App.tsx",
            "import { SizedPanel, ConnectedPanel } from './Panel'; export function App() { return <div><SizedPanel action='sized' /><ConnectedPanel action='connected' /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let mut actions = report
        .creations
        .iter()
        .flat_map(|creation| {
            creation.invocations.iter().map(move |invocation| {
                (
                    invocation.arguments["action"].clone(),
                    creation.reachability,
                )
            })
        })
        .collect::<Vec<_>>();
    actions.sort_by_key(|(value, _)| format!("{value:?}"));
    assert_eq!(
        actions,
        [
            (
                QueryValue::String {
                    value: "connected".to_owned()
                },
                Reachability::Possible
            ),
            (
                QueryValue::String {
                    value: "sized".to_owned()
                },
                Reachability::Possible
            ),
        ],
        "{:?}",
        report.coverage.gaps
    );
    assert!(report.gaps.iter().any(
        |gap| gap.kind == "render_assumed_through_unmodeled_component"
            && gap.assessment == QueryGapAssessment::Direct
    ));
}

#[test]
fn parsed_wrapper_that_hands_children_to_an_unknown_call_keeps_a_possible_path() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/Layer.tsx",
            "import { createPortal } from 'external-dom'; export function Layer({ children }: { children: unknown }) { return createPortal(children, null); } export function Empty(_props: { children: unknown }) { return null; }",
        ),
        (
            "src/App.tsx",
            "import { Layer, Empty } from './Layer'; import { Leaf } from './Leaf'; export function App() { return <div><Layer><Leaf action='portal' /></Layer><Empty><Leaf action='dropped' /></Empty></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let actions = report
        .creations
        .iter()
        .flat_map(|creation| {
            creation.invocations.iter().map(move |invocation| {
                (
                    format!("{:?}", invocation.arguments["action"]),
                    creation.reachability,
                )
            })
        })
        .collect::<Vec<_>>();
    // The portal's children leave the model through an unknown call, so they are possible.
    // A wrapper that returns null is fully modeled, so its children are not rendered at all.
    assert_eq!(
        actions,
        [(
            format!(
                "{:?}",
                QueryValue::String {
                    value: "portal".to_owned()
                }
            ),
            Reachability::Possible
        )],
        "{:?}",
        report.coverage.gaps
    );
}

#[test]
fn conditional_component_renders_children_under_an_assumption() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/Panel.tsx",
            "import { Tab } from 'external-tabs'; import { Leaf } from './Leaf'; export function Panel({ tabbed }: { tabbed: boolean }) { const Box = tabbed ? Tab : 'section'; return <Box><Leaf action='boxed' /></Box>; }",
        ),
        (
            "src/App.tsx",
            "import { Frame } from 'external-frame'; import { Panel } from './Panel'; export function App({ tabbed }: { tabbed: boolean }) { return <Frame><Panel tabbed={tabbed} /><Panel tabbed={tabbed} /></Frame>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert!(
        report.creations.iter().any(|creation| {
            creation.reachability == Reachability::Possible && creation.invocations.len() == 1
        }),
        "{:?}",
        report.coverage.gaps
    );
}

#[test]
fn shared_provider_renders_children_at_every_use() {
    // One provider element site is shared by every use of the wrapper component. Its visit
    // budget must not stop later uses from rendering their own children.
    let uses = (0..20)
        .map(|index| format!("<Shell><Leaf action='use-{index}' /></Shell>"))
        .collect::<Vec<_>>()
        .concat();
    let app = format!(
        "import {{ Shell }} from './shell'; import {{ Leaf }} from './Leaf'; export function App() {{ return <div>{uses}</div>; }}"
    );
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/shell.tsx",
            "import * as React from 'react'; const ShellContext = React.createContext(null); export function Shell({ children }: { children: unknown }) { return <ShellContext.Provider value={null}>{children}</ShellContext.Provider>; }",
        ),
        ("src/App.tsx", app.as_str()),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let actions = report
        .creations
        .iter()
        .filter(|creation| creation.reachability == Reachability::Reachable)
        .flat_map(|creation| &creation.invocations)
        .map(|invocation| format!("{:?}", invocation.arguments["action"]))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actions.len(), 20, "{:?}", report.coverage.gaps);
}

#[test]
fn provider_component_forwards_children_through_typed_context() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('open'); return null; }",
        ),
        (
            "src/location.tsx",
            "import * as React from 'react'; type Locations = string[]; export const LocationContext = React.createContext<Locations>([]); export function LocationProvider({children, value}: {children: React.ReactNode; value: Locations}) { return <LocationContext.Provider value={value}>{children}</LocationContext.Provider>; }",
        ),
        (
            "src/App.tsx",
            "import { LocationProvider } from './location'; import { Leaf } from './Leaf'; export function App({ flag }: { flag: boolean }) { return <LocationProvider value={['a']}>{flag ? <span /> : null}<div><Leaf /></div></LocationProvider>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
    assert_eq!(report.creations[0].invocations.len(), 1);
}

#[test]
fn react_context_provider_and_consumer_render_children_without_contracts() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf({ action }: { action: string }) { const [, apply] = useItemSelection(['alpha']); apply(action); return null; }",
        ),
        (
            "src/contexts.ts",
            "import * as React from 'react'; import { createContext } from 'react'; export const Theme = createContext('light'); const Session = React.createContext(null); Session.displayName = 'Session'; export default Session;",
        ),
        (
            "src/App.tsx",
            "import Session, { Theme } from './contexts'; import { Leaf } from './Leaf'; export function App() { return <Theme.Provider value='dark'><Leaf action='open' /><Session.Consumer>{(_session: unknown) => <Leaf action='close' />}</Session.Consumer></Theme.Provider>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let mut actions = report
        .creations
        .iter()
        .inspect(|creation| assert_eq!(creation.reachability, Reachability::Reachable))
        .flat_map(|creation| &creation.invocations)
        .map(|invocation| invocation.arguments["action"].clone())
        .collect::<Vec<_>>();
    actions.sort_by_key(|value| format!("{value:?}"));
    assert_eq!(
        actions,
        [
            QueryValue::String {
                value: "close".to_owned()
            },
            QueryValue::String {
                value: "open".to_owned()
            },
        ],
        "{:?}",
        report.coverage.gaps
    );
}

#[test]
fn provider_fragment_object_rest_and_function_child_reach_leaf() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values: string[]) { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Leaf.tsx",
            "import { useItemSelection } from './hook'; export function Leaf() { const [, apply] = useItemSelection(['alpha']); apply('close'); return null; }",
        ),
        (
            "src/context.tsx",
            "const Context = {Provider: (_props: unknown) => null}; export default Context;",
        ),
        (
            "src/overlay.tsx",
            "export default function Overlay(_props: unknown) { return null; }",
        ),
        (
            "src/Wrapper.tsx",
            "export function Wrapper({label, ...rest}: {label: string; children: unknown}) { return <div {...rest} />; }",
        ),
        (
            "src/App.tsx",
            "import * as React from 'react'; import Context from './context'; import Overlay from './overlay'; import { Wrapper } from './Wrapper'; import { Leaf } from './Leaf'; export class App { render() { return <React.Fragment><Context.Provider><Wrapper label='x'><Overlay>{() => <Leaf />}</Overlay></Wrapper></Context.Provider></React.Fragment>; } }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[component_consumers]]\nmodule = './context'\nexport = 'default.Provider'\nforward_children = true\n[[component_consumers]]\nmodule = './overlay'\nexport = 'default'\ninvoke_children = true\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
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
    let seed = report
        .creations
        .iter()
        .find_map(|creation| {
            creation.reverse_importer.as_ref().filter(|seed| {
                seed.location
                    .as_ref()
                    .is_some_and(|location| location.path == "src/Host.ts")
            })
        })
        .expect("reverse importer provenance");
    assert_eq!(seed.symbol, "Host");
    assert_eq!(seed.matched_imports, ["useWidget"]);
    assert_eq!(seed.evaluation, QueryReverseImporterEvaluation::Function);
    assert_eq!(seed.location.as_ref().unwrap().start_line, 1);
}

#[test]
fn reverse_importer_report_names_a_large_file_direct_use() {
    let padding = "x".repeat(20_100);
    let host = format!(
        "import {{ useWidget }} from './wrapper'; /*{padding}*/ export function Host() {{ useWidget(); }}"
    );
    let fixture = TestProject::new(&[
        ("src/Host.ts", &host),
        (
            "src/wrapper.ts",
            "import { makeCallback } from './factory'; export function useWidget() { return makeCallback('alpha'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['makeCallback']\n");
    let mut query = fixture.query();
    query.scope = QueryScope::AllCreations;
    let report = fixture.analyzer().query(&query, "test-query").unwrap();
    let seed = report
        .creations
        .iter()
        .filter_map(|creation| creation.reverse_importer.as_ref())
        .find(|seed| seed.evaluation == QueryReverseImporterEvaluation::DirectImportUse)
        .expect("large-file direct import seed");
    assert_eq!(seed.symbol, "Host");
    assert_eq!(seed.matched_imports, ["useWidget"]);
    assert_eq!(seed.location.as_ref().unwrap().path, "src/Host.ts");
}

#[test]
fn reverse_importer_report_names_module_binding() {
    let fixture = TestProject::new(&[
        (
            "src/Host.ts",
            "import { useWidget } from './wrapper'; export const installed = useWidget();",
        ),
        (
            "src/wrapper.ts",
            "import { makeCallback } from './factory'; export function useWidget() { return makeCallback('alpha'); }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['makeCallback']\n");
    let mut query = fixture.query();
    query.scope = QueryScope::AllCreations;
    let report = fixture.analyzer().query(&query, "test-query").unwrap();
    let seed = report
        .creations
        .iter()
        .filter_map(|creation| creation.reverse_importer.as_ref())
        .find(|seed| seed.evaluation == QueryReverseImporterEvaluation::ModuleBinding)
        .expect("module binding seed");
    assert_eq!(seed.symbol, "installed");
    assert_eq!(seed.matched_imports, ["useWidget"]);
    assert_eq!(seed.location.as_ref().unwrap().path, "src/Host.ts");
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
fn unreached_host_renders_child_that_receives_its_capability() {
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection() { return [null, (_action: string) => {}]; }",
        ),
        (
            "src/Host.tsx",
            "import { useItemSelection } from './hook'; function Child({apply}: {apply: (action: string) => void}) { apply('close'); return null; } export function Host() { const [, apply] = useItemSelection(); return <Child apply={apply} />; }",
        ),
    ]);
    fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n");
    fixture.write("query.toml", "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n");
    let report = fixture.report();
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
