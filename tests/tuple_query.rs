mod support;

use code_flow::{
    Project,
    queries::Conclusion,
    query::{
        QueryBoundaryKind, QueryCallPathKind, QueryCallsiteStatus, QueryCapabilityStatus,
        QueryGapAssessment, QueryGapTarget, QueryReverseImporterEvaluation, QueryScope,
        QueryUnreachedReason, QueryValue, Reachability, load_query,
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
    // A spread of an unknown value may overwrite properties listed before it, so that read is
    // the listed value or an unknown; the other spreads keep exact values.
    assert_eq!(
        exact,
        [
            "Alternatives { values: [String { value: \"listed\" }, Unknown { reason: \"object_spread_of_unknown_value\" }] }",
            "base",
            "md"
        ],
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
fn assumed_render_steps_bound_possible_exploration() {
    let fixture = TestProject::new(&[
        HOOK,
        LEAF,
        (
            "src/App.tsx",
            "import { Frame } from 'external-frame'; import { Leaf } from './Leaf'; export function App() { return <Frame><Leaf /></Frame>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].reachability, Reachability::Possible);
    fixture.write(
        "query.toml",
        &format!("assumed_render_steps = 1\n{TUPLE_QUERY}"),
    );
    let report = fixture.report();
    assert!(report.creations.is_empty(), "{:?}", report.creations);
    assert!(
        report
            .gaps
            .iter()
            .any(|gap| gap.kind == "assumed_render_budget_exhausted")
    );
}

#[test]
fn blocks_loops_try_and_throw_keep_paths_exact() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import { Leaf } from './Leaf'; import { strict } from 'external-flags'; function Guarded({ item }) { let action = item; if (strict) { action = 'thrown'; throw new Error(action); } return <Leaf action={action} />; } function Counted({ items }) { for (let index = 0; index < items.length; index += 1) { if (index > 1) { break; } return <Leaf action='counted' />; } return null; } function Waiting({ ready }) { while (!ready) { return <Leaf action='waited' />; } return null; } function Labeled() { scan: for (const key in { only: 1 }) { continue scan; } return <Leaf action='labeled' />; } function Scoped() { { const label = 'scoped'; return <Leaf action={label} />; } } function Attempt({ load }) { try { load(); return <Leaf action='tried' />; } catch (error) { return <Leaf action='caught' />; } finally { load(); } } export function App() { return <div><Guarded item='guarded' /><Counted items={[]} /><Waiting ready={strict} /><Labeled /><Scoped /><Attempt load={strict} /></div>; }",
    )]);
    // The thrown branch ends its path, so the value it assigned never reaches the render.
    assert_eq!(
        exact,
        [
            "caught", "counted", "guarded", "labeled", "scoped", "tried", "waited"
        ],
        "{other:?}"
    );
    assert!(other.is_empty(), "{other:?}");
}

#[test]
fn recursion_over_unknown_data_stops_at_the_recursion_budget() {
    let app = (
        "src/App.tsx",
        "import { Leaf } from './Leaf'; import { tree } from 'external-tree'; function walk(node, depth) { if (node.child != null) { return walk(node.child, [...depth, node.key]); } return <Leaf action='leaf' />; } function count(node) { if (node.next != null) { return count(node.next); } return <Leaf action={node.label} />; } export function App() { return <div>{walk(tree, [])}{count({ next: { next: { next: { label: 'deep' } } } })}</div>; }",
    );
    let (exact, other) = exact_actions(&[app]);
    // Known data recurses as deep as it goes; unknown data stops at the recursion budget, and
    // the path that returns before recursing is still explored at every level.
    assert_eq!(exact, ["deep", "leaf"], "{other:?}");
    let fixture = TestProject::new(&[HOOK, LEAF, app]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    let stops = report
        .gaps
        .iter()
        .filter(|gap| gap.kind == "recursion_depth_budget_exhausted")
        .count();
    assert_eq!(stops, 1, "{:?}", report.coverage.gaps);
}

#[test]
fn uncalled_callbacks_carrying_the_result_run_as_possible_handlers() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/App.tsx",
            "import { useItemSelection } from './hook'; function Notice() { const [, apply] = useItemSelection(['alpha']); return <div onMouseLeave={() => apply('left')} onClick={() => apply('clicked')} />; } export function App() { return <Notice />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        &TUPLE_QUERY.replace(
            "scope = 'reachable'\n",
            "scope = 'reachable'\nscan_callback_bodies = true\n",
        ),
    );
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1);
    assert_eq!(report.creations[0].reachability, Reachability::Reachable);
    let mut actions = report.creations[0]
        .invocations
        .iter()
        .map(|invocation| {
            let QueryValue::String { value } = &invocation.arguments["action"] else {
                panic!("{:?}", invocation.arguments);
            };
            let uncalled = invocation
                .call_path
                .iter()
                .any(|step| step.kind == QueryCallPathKind::UncalledCallback);
            (value.clone(), uncalled)
        })
        .collect::<Vec<_>>();
    actions.sort();
    // The click handler is modeled; nothing in the model calls the other handler, so it runs as
    // one that may, and its path says so.
    assert_eq!(
        actions,
        [("clicked".to_owned(), false), ("left".to_owned(), true)]
    );
}

#[test]
fn optional_chains_read_and_call_like_their_plain_forms() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import { Leaf } from './Leaf'; function Picker({ config, render }) { return <div><Leaf action={config?.label} />{render?.('rendered')}</div>; } export function App() { return <Picker config={{ label: 'chained' }} render={(action) => <Leaf action={action} />} />; }",
    )]);
    assert_eq!(exact, ["chained", "rendered"], "{other:?}");
}

#[test]
fn callbacks_run_without_known_arguments_do_not_invent_undefined() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/App.tsx",
            "import { useItemSelection } from './hook'; import { open } from 'external-open'; function Notice() { const [, apply] = useItemSelection(['alpha']); open({ onChoice: (choice) => apply(choice) }); return <button onClick={(event) => apply(event)} />; } export function App() { return <Notice />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        &TUPLE_QUERY.replace(
            "scope = 'reachable'\n",
            "scope = 'reachable'\nscan_callback_bodies = true\n",
        ),
    );
    let report = fixture.report();
    let actions = report
        .creations
        .iter()
        .flat_map(|creation| &creation.invocations)
        .map(|invocation| invocation.arguments["action"].clone())
        .collect::<Vec<_>>();
    // The opaque consumer's argument and the click event are unknown, never a missing argument.
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, QueryValue::Unknown { .. })),
        "{actions:?}"
    );
}

#[test]
fn class_property_methods_are_bound_to_instance_props() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import * as React from 'react'; import { Leaf } from './Leaf'; function Wrapper({ render }) { return render('bound'); } class Panel extends React.Component { handleRender = (label) => <Leaf action={label} />; renderBody() { return <Leaf action={this.props.mode} />; } render() { return <div><Wrapper render={this.handleRender} />{this.renderBody()}</div>; } } export function App() { return <Panel mode='method' />; }",
    )]);
    assert_eq!(exact, ["bound", "method"], "{other:?}");
    assert!(other.is_empty(), "{other:?}");
}

#[test]
fn react_element_and_children_apis_keep_paths_exact() {
    let (exact, other) = exact_actions(&[(
        "src/App.tsx",
        "import * as React from 'react'; import { createPortal } from 'react-dom'; import { Leaf } from './Leaf'; function Only({ children }) { return React.Children.only(children); } function Portal({ children }) { return createPortal(children, null); } function Each({ children }) { return <div>{React.Children.map(children, (child) => React.cloneElement(child, { action: 'mapped' }))}</div>; } function Base(props) { return <Leaf {...props} />; } const Themed = Object.assign(Base, { Overlay: Base }); export function App() { return <React.Suspense fallback={null}><Only><Leaf action='only' /></Only><Portal><Leaf action='portal' /></Portal><Each><Leaf action='original' /></Each>{React.createElement(Leaf, { action: 'created' })}<Themed action='assigned' /></React.Suspense>; }",
    )]);
    assert_eq!(
        exact,
        ["assigned", "created", "mapped", "only", "portal"],
        "{other:?}"
    );
}

#[test]
fn root_path_parses_the_factory_that_creates_a_rendered_component() {
    let fixture = TestProject::new(&[
        HOOK,
        LEAF,
        (
            "src/panelFactory.tsx",
            "export function createPanel(className) { return function Panel({ children }) { return <section className={className}>{children}</section>; }; }",
        ),
        (
            "src/design.tsx",
            "import { createPanel } from './panelFactory'; export const Panel = createPanel('panel');",
        ),
        (
            "src/App.tsx",
            "import { Panel } from './design'; import { Leaf } from './Leaf'; export function App() { return <Panel><Leaf /></Panel>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{:?}",
        report.component_boundaries
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
    assert_eq!(report.schema_version, 11);
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
    // Both paths call the factory with the same values at one callsite, so they are one
    // creation with an invocation from each.
    assert_eq!(report.creations.len(), 1, "{:?}", report.coverage.gaps);
    assert_eq!(
        report.creations[0].reachability,
        Reachability::Reachable,
        "{contracts}"
    );
    assert_eq!(report.creations[0].invocations.len(), 2);
    assert!(report.component_boundaries.is_empty());
}

#[test]
fn unreached_callsites_name_the_explored_use_that_stopped_exploration() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/leaves.tsx",
            "import { useItemSelection } from './hook'; export function MenuLeaf() { const [, apply] = useItemSelection(['menu']); apply('menu'); return null; } export function ToastLeaf() { const [, apply] = useItemSelection(['toast']); apply('toast'); return null; } export function UnusedLeaf() { const [, apply] = useItemSelection(['unused']); apply('unused'); return null; }",
        ),
        (
            "src/Page.tsx",
            "import { useItemSelection } from './hook'; export default function Page() { const [, apply] = useItemSelection(['page']); apply('page'); return null; }",
        ),
        (
            "src/Unused.tsx",
            "import { UnusedLeaf } from './leaves'; export function Unused() { return <UnusedLeaf />; }",
        ),
        (
            "src/App.tsx",
            "import { load } from 'external-loader'; import { notify } from 'external-toasts'; import { MenuLeaf, ToastLeaf } from './leaves'; const Page = load({ promise: () => import('./Page') }); function Menu({ renderItem }) { return <ul />; } function Item() { return <MenuLeaf />; } export function App() { notify(() => <ToastLeaf />); return <div><Menu renderItem={() => <Item />} /><Page /></div>; }",
        ),
    ]);
    let config = "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n";
    fixture.write("flow.toml", config);
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    let explained = report
        .unreached_callsites
        .iter()
        .map(|callsite| {
            let enclosing = callsite.enclosing.as_deref().unwrap_or_default();
            let name = enclosing.split(' ').next().unwrap_or_default().to_owned();
            (name, callsite)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        explained.keys().map(String::as_str).collect::<Vec<_>>(),
        ["MenuLeaf", "Page", "ToastLeaf", "UnusedLeaf"]
    );
    let menu = explained["MenuLeaf"];
    assert_eq!(menu.reason, QueryUnreachedReason::PropCallback);
    assert!(menu.detail.contains("renderItem"), "{}", menu.detail);
    assert!(
        menu.explored_ancestor
            .as_deref()
            .unwrap()
            .starts_with("App ")
    );
    assert_eq!(menu.chain.len(), 2, "{:?}", menu.chain);
    let toast = explained["ToastLeaf"];
    assert_eq!(toast.reason, QueryUnreachedReason::Callback);
    assert!(toast.detail.contains("notify"), "{}", toast.detail);
    assert_eq!(
        explained["UnusedLeaf"].reason,
        QueryUnreachedReason::NoExploredAncestor
    );
    assert!(explained["UnusedLeaf"].detail.contains("ends at Unused"));
    let page = explained["Page"];
    assert_eq!(page.reason, QueryUnreachedReason::ComponentNotFollowed);
    let contract = page.suggested_contract.as_deref().expect("lazy contract");
    assert_eq!(
        contract,
        "[[lazy_component_factories]]\nmodule = \"external-loader\"\nexport = \"load\"\npromise_property = \"promise\"\n"
    );
    fixture.write("flow.toml", &format!("{config}\n{contract}"));
    let report = fixture.report();
    assert!(
        report.creations.iter().any(|creation| {
            creation.reachability == Reachability::Reachable
                && creation
                    .factory_location
                    .as_ref()
                    .is_some_and(|location| location.path.ends_with("Page.tsx"))
        }),
        "{:?}",
        report.unreached_callsites
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn callsite_values_follow_the_result_to_every_call_in_the_source() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { CLOSE = 1, OPEN = 2 }"),
        (
            "src/Lazy.tsx",
            "export default function LazyChild({ onDone }) { return <button onClick={() => onDone('lazy')} />; }",
        ),
        (
            "src/App.tsx",
            "import * as React from 'react'; import { lazy } from 'react'; import { Kind } from './kinds'; import { useItemSelection } from './hook'; import { report } from 'external-report'; const LazyChild = lazy(() => import('./Lazy')); function Child({ onClose }) { return <button onMouseEnter={() => onClose?.('child')} />; } class Legacy extends React.Component { handle() { this.props.onClose('class'); } render() { return <button onClick={() => this.handle()} />; } } function Quiet({ onClose }) { return null; } function Direct({ enabled = false }) { const [, apply] = useItemSelection(['direct']); apply('now'); if (enabled) { apply(Kind.OPEN); } return null; } function Props() { const [, apply] = useItemSelection(['props']); const wrapped = React.useCallback(apply, []); const handlers = React.useMemo(() => ({ close: apply }), [apply]); return <div><Child onClose={wrapped} /><Legacy onClose={handlers.close} /><LazyChild onDone={apply} /></div>; } function useNotice() { const [visible, apply] = useItemSelection(['notice']); return [visible, apply]; } function Banner() { const [, close] = useNotice(); return <button onClick={() => close(Kind.CLOSE)} />; } export function useOrphan() { return useItemSelection(['orphan']); } function Unused() { const [visible] = useItemSelection(['unused']); const [, _ignored] = useItemSelection(['ignored']); return visible; } function Ignored() { const [, apply] = useItemSelection(['ignoring']); return <Quiet onClose={apply} />; } function Escaped() { const [, apply] = useItemSelection(['escaped']); report(apply); return null; } export function App() { return <div><Direct /><Props /><Banner /><Unused /><Ignored /><Escaped /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let by_items = report
        .callsites
        .iter()
        .map(|callsite| {
            let QueryValue::Array { elements } = &callsite.factory_arguments["items"][0] else {
                panic!("{:?}", callsite.factory_arguments);
            };
            let QueryValue::String { value } = &elements[0] else {
                panic!("{elements:?}");
            };
            (value.clone(), callsite)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let calls = |items: &str| {
        let mut calls = by_items[items]
            .capability
            .calls
            .iter()
            .map(|call| {
                let action = call.arguments["action"]
                    .iter()
                    .map(|value| match value {
                        QueryValue::String { value } => value.clone(),
                        QueryValue::EnumMember { member_name, .. } => member_name.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|");
                (action, call.explored, call.via.clone())
            })
            .collect::<Vec<_>>();
        calls.sort();
        calls
    };
    // A guarded call no explored path runs is still found, with its context-free argument.
    assert_eq!(
        calls("direct"),
        [
            ("OPEN".to_owned(), false, vec![]),
            ("now".to_owned(), true, vec![])
        ]
    );
    assert_eq!(
        by_items["direct"].capability.status,
        QueryCapabilityStatus::Called
    );
    let props = calls("props");
    assert_eq!(
        props
            .iter()
            .map(|(action, _, via)| (action.as_str(), via.join(" / ")))
            .collect::<Vec<_>>(),
        [
            ("child", "prop onClose of <Child>".to_owned()),
            ("class", "prop onClose of <Legacy>".to_owned()),
            ("lazy", "prop onDone of <LazyChild>".to_owned()),
        ]
    );
    assert_eq!(
        calls("notice"),
        [(
            "CLOSE".to_owned(),
            true,
            vec!["returned by useNotice to Banner".to_owned()]
        )]
    );
    assert_eq!(
        by_items["orphan"].capability.status,
        QueryCapabilityStatus::Escapes
    );
    assert!(
        by_items["orphan"].capability.escapes[0]
            .detail
            .contains("callers are not in the parsed files")
    );
    assert_eq!(
        by_items["unused"].capability.status,
        QueryCapabilityStatus::Unused
    );
    assert_eq!(
        by_items["ignored"].capability.status,
        QueryCapabilityStatus::Unused
    );
    assert_eq!(
        by_items["ignoring"].capability.status,
        QueryCapabilityStatus::NotCalled
    );
    assert_eq!(
        by_items["escaped"].capability.status,
        QueryCapabilityStatus::Escapes
    );
    assert!(
        by_items["escaped"].capability.escapes[0]
            .detail
            .contains("passed to report")
    );
    assert!(
        by_items
            .values()
            .all(|callsite| callsite.factory_arguments_resolved)
    );
}

#[test]
fn many_possible_arrays_are_summarized_by_their_elements() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { enabled } from 'external-flags'; import { useItemSelection } from './hook'; const ALL = [Kind.A, Kind.B, Kind.C]; function Picker() { const [, apply] = useItemSelection(ALL.filter((kind) => enabled(kind))); apply('pick'); return null; } export function App() { return <Picker />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("{:?}", report.callsites);
    };
    // An unknown predicate keeps every subset; the summary names the elements they hold.
    let mut names = Vec::new();
    for value in &callsite.possible_elements["items"] {
        collect_enum_members(value, &mut names);
    }
    assert_eq!(names, ["A", "B", "C"]);
    assert!(callsite.factory_arguments_resolved);
}

#[test]
fn set_and_array_membership_decide_filters_with_unknown_conditions() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4 }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { check } from 'external-check'; import { useItemSelection } from './hook'; const ALL = [{ kind: Kind.A }, { kind: Kind.B }, { kind: Kind.C }, { kind: Kind.D }]; const EXCLUDED = new Set([Kind.A]); const RETIRED = [Kind.D]; function Picker() { const types = ALL.filter(({ kind }) => check(kind) === true && !EXCLUDED.has(kind) && !RETIRED.includes(kind)).map((entry) => entry.kind); const [, apply] = useItemSelection(types); apply('pick'); return null; } export function App() { return <Picker />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("{:?}", report.callsites);
    };
    // An unknown condition joined by `&&` with a decided exclusion is decided for that element.
    let mut names = Vec::new();
    for value in &callsite.possible_elements["items"] {
        collect_enum_members(value, &mut names);
    }
    assert_eq!(names, ["B", "C"]);
}

const ITEMS_QUERY: &str = "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n";

/// The enum members a callsite may request, from its explored values or what the source pushes,
/// and whether any of them is unknown.
fn requested_members(report: &code_flow::query::QueryReport) -> (Vec<String>, bool) {
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let values = callsite
        .possible_elements
        .get("items")
        .unwrap_or(&callsite.factory_arguments["items"]);
    let mut names = Vec::new();
    for value in values {
        collect_enum_members(value, &mut names);
    }
    names.sort();
    names.dedup();
    (names, values.iter().any(contains_unknown))
}

#[test]
fn a_choice_between_arrays_holds_what_each_pushes() {
    // Independent pushes make more arrays than exploration keeps, so the source says what the
    // list may hold, through the choice between it and an empty list.
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4, E = 5, F = 6, G = 7 }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { flag } from 'external-flags'; import { useItemSelection } from './hook'; function Picker() { const kinds: Kind[] = []; if (flag('a')) { kinds.push(Kind.A); } if (flag('b')) { if (flag('c')) { kinds.push(Kind.B); } else if (flag('d')) { kinds.push(Kind.C); } } if (flag('e')) { kinds.push(Kind.D); } if (flag('f')) { kinds.push(Kind.E); } if (flag('g')) { kinds.push(Kind.F); } if (flag('h')) { kinds.push(Kind.G); } const [, apply] = useItemSelection(flag('hidden') ? [] : kinds); apply('pick'); return null; } export function App() { return <Picker />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", ITEMS_QUERY);
    let (names, unknown) = requested_members(&fixture.report());
    assert_eq!(names, ["A", "B", "C", "D", "E", "F", "G"]);
    assert!(!unknown);
}

#[test]
fn tables_with_computed_keys_give_any_entry_for_an_unknown_key() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4, E = 5 }",
        ),
        (
            "src/names.ts",
            "export const Names = { ONE: 'one', TWO: 'two', THREE: 'three' } as const;",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { Names } from './names'; import { pick } from 'external-pick'; import { useItemSelection } from './hook'; const TABLE = { [Names.ONE]: Kind.A, [Names.TWO]: Kind.B, [Kind.C]: Kind.D }; function Picker({ name }) { const kind = TABLE[name]; if (kind != null) { const [, apply] = useItemSelection([kind, TABLE[Names.THREE] ?? Kind.E]); apply('pick'); } return null; } export function App() { return <Picker name={pick()} />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", ITEMS_QUERY);
    // The unknown key reads one of the table's entries; the known key reads a missing entry.
    let (names, unknown) = requested_members(&fixture.report());
    assert_eq!(names, ["A", "B", "D", "E"]);
    assert!(!unknown);
}

#[test]
fn callsite_arguments_parse_the_modules_their_values_come_from() {
    // Neither the config, the hook that builds the other entry, nor the enum holds the filter
    // term, so only the values that reach the callsite's argument ask for them.
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/config.ts",
            "import { Kind } from './kinds'; const Current = { content: Kind.B, label: 'current' }; export default Current;",
        ),
        (
            "src/useConfig.ts",
            "import * as React from 'react'; import { Kind } from './kinds'; import { flag } from 'external-flags'; function build(source) { if (source == null) { return null; } return { content: Kind.C, title: source.title }; } export function useConfig() { const show = flag('show'); const source = flag('source'); const config = React.useMemo(() => (show ? build(source) : null), [show, source]); return { show, config }; }",
        ),
        (
            "src/App.tsx",
            "import Current from './config'; import { useConfig } from './useConfig'; import { flag } from 'external-flags'; import { useItemSelection } from './hook'; function Picker() { const { show, config } = useConfig(); const items = []; if (flag('current') && Current.content != null) { items.push(Current.content); } if (show && config != null) { items.push(config.content); } const [, apply] = useItemSelection(items); apply('pick'); return null; } export function App() { return <Picker />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", ITEMS_QUERY);
    let (names, unknown) = requested_members(&fixture.report());
    assert_eq!(names, ["B", "C"]);
    assert!(!unknown);
}

#[test]
fn an_unexplored_instance_requests_the_arrays_its_caller_builds() {
    // The explored caller passes `B`, so the wrapper's items are known; the other callers are never
    // rendered, and their calls apply to what they build or are given, not to the explored
    // caller's items.
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/Tagged.tsx",
            "import { Kind } from './kinds'; import Sel from './Sel'; function Tagged({ kind }) { return <Sel kinds={[kind]}>{({ apply }) => <button onClick={() => apply('c')} />}</Sel>; } export function Uses() { return <Tagged kind={Kind.C} />; }",
        ),
        (
            "src/Sel.tsx",
            "import { useItemSelection } from './hook'; export default function Sel({ kinds, children }) { const [visible, apply] = useItemSelection(kinds); return children({ visible, apply }); }",
        ),
        (
            "src/Unrendered.tsx",
            "import { Kind } from './kinds'; import Sel from './Sel'; export function Unrendered({ disabled }) { const kinds = disabled ? [] : [Kind.A]; return <Sel kinds={kinds}>{({ visible, apply }) => (visible === Kind.A ? <button onClick={() => apply('a')} /> : null)}</Sel>; }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import Sel from './Sel'; export function App() { return <Sel kinds={[Kind.B]}>{({ visible, apply }) => (visible === Kind.B ? <button onClick={() => apply('b')} /> : null)}</Sel>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", ITEMS_QUERY);
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut applied = callsite
        .capability
        .calls
        .iter()
        .map(|call| {
            let mut names = Vec::new();
            for value in &call.elements["items"] {
                collect_enum_members(value, &mut names);
            }
            (format!("{:?}", call.arguments["action"]), names)
        })
        .collect::<Vec<_>>();
    applied.sort();
    assert_eq!(
        applied,
        [
            ("[String { value: \"a\" }]".to_owned(), vec!["A".to_owned()]),
            ("[String { value: \"b\" }]".to_owned(), vec!["B".to_owned()]),
            ("[String { value: \"c\" }]".to_owned(), vec!["C".to_owned()]),
        ],
        "{:?}",
        callsite.capability.excluded_calls
    );
}

/// Each call's action and the enum members its items name, sorted.
fn applied_items(callsite: &code_flow::query::QueryCallsiteValues) -> Vec<(String, Vec<String>)> {
    let mut applied = callsite
        .capability
        .calls
        .iter()
        .map(|call| {
            let mut names = Vec::new();
            for value in &call.elements["items"] {
                collect_enum_members(value, &mut names);
            }
            let action = match call.arguments["action"].as_slice() {
                [QueryValue::String { value }] => value.clone(),
                values => format!("{values:?}"),
            };
            (action, names)
        })
        .collect::<Vec<_>>();
    applied.sort();
    applied
}

#[test]
fn implicit_invocations_are_calls_at_each_callsite_that_does_not_opt_out() {
    // The hook calls its result itself unless its third argument is truthy or the item is one it
    // skips; a wrapper's instances decide for themselves, including through a bare attribute.
    let fixture = TestProject::new(&[
        (
            "src/hook.ts",
            "export function useItemSelection(_values, _group, _bypass = false) { return [null, (_action) => {}]; }",
        ),
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4, E = 5 }",
        ),
        (
            "src/skip.ts",
            "import { Kind } from './kinds'; export const SKIP = new Set([Kind.C]);",
        ),
        (
            "src/Sel.tsx",
            "import { useItemSelection } from './hook'; export default function Sel({ kinds, bypass, children }) { const [visible, apply] = useItemSelection(kinds, undefined, bypass); return children({ visible, apply }); }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import Sel from './Sel'; import { useItemSelection } from './hook'; function Shown() { const [visible] = useItemSelection([Kind.A]); return visible; } function Bypassed() { const [visible] = useItemSelection([Kind.B], undefined, true); return visible; } function Skipped() { const [visible] = useItemSelection([Kind.C]); return visible; } export function App() { return <div><Shown /><Bypassed /><Skipped /><Sel kinds={[Kind.D]}>{({ apply }) => <button onClick={() => apply('d')} />}</Sel><Sel kinds={[Kind.E]} bypass>{({ apply }) => <button onClick={() => apply('e')} />}</Sel></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        &format!(
            "{ITEMS_QUERY}[[capability.implicit_invocations]]\narguments = {{ action = 'auto' }}\nunless_argument = 2\nexcept_items = {{ module = 'src/skip.ts', export = 'SKIP' }}\ndescription = 'called when the component unmounts'\n"
        ),
    );
    let report = fixture.report();
    let sel = report
        .callsites
        .iter()
        .find(|callsite| {
            callsite
                .location
                .as_ref()
                .is_some_and(|location| location.path == "src/Sel.tsx")
        })
        .expect("wrapper callsite");
    // Only the instance that does not pass `bypass` gets the hook's own call.
    assert_eq!(
        applied_items(sel),
        [
            ("auto".to_owned(), vec!["D".to_owned()]),
            ("d".to_owned(), vec!["D".to_owned()]),
            ("e".to_owned(), vec!["E".to_owned()]),
        ]
    );
    let app = report
        .callsites
        .iter()
        .filter(|callsite| {
            callsite
                .location
                .as_ref()
                .is_some_and(|location| location.path == "src/App.tsx")
        })
        .collect::<Vec<_>>();
    let [shown, bypassed, skipped] = app.as_slice() else {
        panic!("{app:?}");
    };
    assert_eq!(
        applied_items(shown),
        [("auto".to_owned(), vec!["A".to_owned()])]
    );
    assert!(bypassed.capability.calls.is_empty());
    assert!(skipped.capability.calls.is_empty());
    assert_eq!(skipped.capability.excluded_calls.len(), 1);
    assert!(
        shown.capability.calls[0]
            .via
            .contains(&"called when the component unmounts".to_owned())
    );
}

#[test]
fn calls_that_are_the_invocation_report_their_own_arguments() {
    // A function that acts when called: each call is the invocation, an options property gives
    // the action, a missing one takes the default, and a helper's callers say its items.
    let fixture = TestProject::new(&[
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4 }",
        ),
        (
            "src/record.ts",
            "export async function record(_kind, _config = {}) {}",
        ),
        (
            "src/internal/forward.ts",
            "import { record } from '../record'; export function forward(kind) { return record(kind, { action: 'forwarded' }); }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { record } from './record'; import { forward } from './internal/forward'; function recordFor(kind) { void record(kind); } export function App() { return <div><button onClick={() => record(Kind.A)} /><button onClick={() => record(Kind.B, { action: 'take' })} /><button onClick={() => recordFor(Kind.C)} /><button onClick={() => forward(Kind.D)} /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'direct'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nexclude_callsites = ['**/internal/**']\n[factory]\nproject = 'acceptance'\nmodule = 'src/record.ts'\nexport = 'record'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\nitems = true\n[capability]\ncall_is_invocation = true\n[[capability.invocation_arguments]]\nindex = 1\npath = ['action']\ndefault = 'unknown'\nlabel = 'action'\n",
    );
    let report = fixture.report();
    // The excluded helper's call is not a callsite.
    assert_eq!(report.callsites.len(), 3, "{:?}", report.callsites);
    let mut applied = report
        .callsites
        .iter()
        .flat_map(applied_items)
        .collect::<Vec<_>>();
    applied.sort();
    assert_eq!(
        applied,
        [
            ("take".to_owned(), vec!["B".to_owned()]),
            ("unknown".to_owned(), vec!["A".to_owned()]),
            ("unknown".to_owned(), vec!["C".to_owned()]),
        ]
    );
}

#[test]
fn callsite_walk_follows_function_children_refs_loaders_and_aliases() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Selected.tsx",
            "import { useItemSelection } from './hook'; export default function Selected({ children }) { const [visible, apply] = useItemSelection(['selected']); return <>{children({ visible, apply })}</>; }",
        ),
        (
            "src/Sheet.tsx",
            "export default function Sheet({ onDone }) { return <button onClick={() => onDone('sheet')} />; }",
        ),
        (
            "src/Modal.tsx",
            "export default function Modal({ onClose }) { return <button onClick={() => onClose('modal')} />; }",
        ),
        (
            "src/Panel.tsx",
            "export default function Panel({ onClose }) { return <button onClick={() => onClose('panel')} />; }",
        ),
        (
            "src/platform.web.tsx",
            "import { useItemSelection } from './hook'; export function usePlatform() { return useItemSelection(['platform']); }",
        ),
        (
            "src/platform.tsx",
            "import * as web from './platform.web'; export const { usePlatform } = web;",
        ),
        (
            "src/shared.tsx",
            "import { useItemSelection } from './hook'; export default function useShared({ open }) { const [, apply] = useItemSelection(['shared']); open({ onClose: (kind) => apply(kind) }); }",
        ),
        (
            "src/App.tsx",
            "import * as React from 'react'; import Selected from './Selected'; import { usePlatform } from './platform'; import useSharedBase from './shared'; import { openSheet, openModal } from 'external-open'; import { useItemSelection } from './hook'; function Child() { return <Selected>{({ apply }) => <button onClick={() => apply('child')} />}</Selected>; } function Platform() { const [, apply] = usePlatform(); apply('platform_call'); return null; } function Refs() { const [, apply] = useItemSelection(['refs']); const ref = React.useRef(apply); React.useEffect(() => { ref.current = apply; }); return <button onClick={() => ref.current('ref')} />; } function Lazy() { const [, apply] = useItemSelection(['lazy']); openModal(async () => { const mod = await import('./Modal'); return () => <mod.default onClose={apply} />; }); openSheet(import('./Sheet'), 'key', { onDone: apply }); return null; } function Destructured() { const [, apply] = useItemSelection(['destructured']); openModal(async () => { const { default: Panel } = await import('./Panel'); return (props) => <Panel {...props} onClose={apply} />; }); return null; } function Shared() { const open = React.useCallback(({ onClose }) => onClose('shared_call'), []); useSharedBase({ open }); return null; } export function App() { return <div><Child /><Platform /><Refs /><Lazy /><Destructured /><Shared /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let actions = |items: &str| {
        let callsite = report
            .callsites
            .iter()
            .find(|callsite| {
                callsite.factory_arguments["items"]
                    == [QueryValue::Array {
                        elements: vec![QueryValue::String {
                            value: items.to_owned(),
                        }],
                    }]
            })
            .unwrap_or_else(|| panic!("callsite for {items}"));
        let mut actions = callsite
            .capability
            .calls
            .iter()
            .flat_map(|call| &call.arguments["action"])
            .map(|value| match value {
                QueryValue::String { value } => value.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        actions.sort();
        actions
    };
    // A call of the `children` parameter is followed into the function child its caller writes.
    assert_eq!(actions("selected"), ["child"]);
    // `export const { usePlatform } = web` re-exports the declaration in `platform.web`.
    assert_eq!(actions("platform"), ["platform_call"]);
    assert_eq!(actions("refs"), ["ref"]);
    // `await import()` names a module, and props passed next to an import are its component's.
    assert_eq!(actions("lazy"), ["modal", "sheet"]);
    // A name destructured from `await import()` is that module's export.
    assert_eq!(actions("destructured"), ["panel"]);
    // A default import under another name still matches its callers, and an injected function
    // bound through `useCallback` is followed.
    assert_eq!(actions("shared"), ["shared_call"]);
}

#[test]
fn callsite_walk_parses_the_files_the_text_filter_skipped() {
    // A chain of hooks that outlasts discovery's rounds, so the hook's last caller is parsed in
    // the root phase, which follows only what root paths and the walk ask for. No file past the
    // hook holds the filter term.
    let mut files = vec![
        (
            "src/hook.ts".to_owned(),
            HOOK.1.to_owned(),
        ),
        (
            "src/useL0.ts".to_owned(),
            "import { useItemSelection } from './hook'; export function useL0() { const [, apply] = useItemSelection(['shown']); return apply; }".to_owned(),
        ),
    ];
    for level in 1..=7 {
        files.push((
            format!("src/useL{level}.ts"),
            format!(
                "import {{ useL{0} }} from './useL{0}'; export function useL{level}() {{ return useL{0}(); }}",
                level - 1
            ),
        ));
    }
    files.extend(
        [
            (
                "src/Menu.tsx",
                "import Child from './Child'; import { useOpen } from './barrel'; import { useL7 } from './useL7'; export function Menu() { const close = useL7(); useOpen({ close }); return <Child onDone={close} />; }",
            ),
            (
                "src/Child.tsx",
                "export default function Child({ onDone }) { return <button onClick={() => onDone('child')} />; }",
            ),
            ("src/barrel.ts", "export { useOpen } from './useOpen';"),
            (
                "src/useOpen.ts",
                "export function useOpen({ close }) { close('opened'); }",
            ),
            ("src/App.tsx", "export function App() { return null; }"),
        ]
        .map(|(path, source)| (path.to_owned(), source.to_owned())),
    );
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
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut actions = callsite
        .capability
        .calls
        .iter()
        .flat_map(|call| &call.arguments["action"])
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>();
    actions.sort();
    assert_eq!(
        actions,
        [
            "String { value: \"child\" }",
            "String { value: \"opened\" }"
        ],
        "{:?}",
        callsite.capability.escapes
    );
    assert!(callsite.capability.escapes.is_empty());

    // Without root-phase files, the hook's last caller is never parsed.
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[limits]\nroot_phase_files = 0\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    assert!(callsite.capability.calls.is_empty());
    assert!(!callsite.capability.escapes.is_empty());
}

#[test]
fn callsite_walk_follows_configured_component_wrappers() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Panel.tsx",
            "export default function Panel({ onDone }) { return <button onClick={() => onDone('panel')} />; }",
        ),
        (
            "src/Card.tsx",
            "export default function Card({ onDone }) { return <button onClick={() => onDone('card')} />; }",
        ),
        (
            "src/Connected.tsx",
            "import { connect } from 'external-store'; import Panel from './Panel'; export default connect([], () => ({}))(Panel);",
        ),
        (
            "src/Themed.tsx",
            "import { withTheme, connect } from 'external-store'; import Card from './Card'; export const Themed = withTheme(connect([])(Card));",
        ),
        (
            "src/App.tsx",
            "import Connected from './Connected'; import { Themed } from './Themed'; import { useItemSelection } from './hook'; function Shown() { const [, apply] = useItemSelection(['shown']); return <div><Connected onDone={apply} /><Themed onDone={apply} /></div>; } export function App() { return <Shown />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[component_wrappers]]\nmodule = 'external-store'\nexport = 'connect'\ncomponent_argument = 0\ncurried = true\n[[component_wrappers]]\nmodule = 'external-store'\nexport = 'withTheme'\ncomponent_argument = 0\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut actions = callsite
        .capability
        .calls
        .iter()
        .flat_map(|call| &call.arguments["action"])
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>();
    actions.sort();
    assert_eq!(
        actions,
        ["String { value: \"card\" }", "String { value: \"panel\" }"]
    );
    // The props of a wrapped component are followed into the component, through a curried
    // wrapper and through one wrapper around another.
    assert!(
        callsite.capability.escapes.is_empty(),
        "{:?}",
        callsite.capability.escapes
    );
    // Exploration renders the configured wrappers' components too.
    assert!(callsite.capability.calls.iter().all(|call| call.explored));
}

#[test]
fn callsite_walk_follows_props_into_components_configured_openers_open() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Panel.tsx",
            "export default function Panel({ onDone }) { return <button onClick={() => onDone('panel')} />; }",
        ),
        (
            "src/Nested.tsx",
            "export default function Nested({ onDone }) { return <button onClick={() => onDone('nested')} />; }",
        ),
        (
            "src/SheetA.tsx",
            "export default function SheetA({ onDone }) { return <button onClick={() => onDone('sheet_a')} />; }",
        ),
        (
            "src/Rendered.tsx",
            "export default function Rendered({ onDone }) { return <button onClick={() => onDone('rendered')} />; }",
        ),
        (
            "src/SheetB.tsx",
            "export default function SheetB({ onDone }) { return <button onClick={() => onDone('sheet_b')} />; }",
        ),
        // Opens whichever sheet its caller's importer loads, with a wrapper around `onDone`.
        (
            "src/Opener.tsx",
            "import Sheets from 'external-ui'; export function Opener(props) { Sheets.openDeferred(props.importer(), 'key', { ...props, onDone: (kind) => props.onDone(kind) }); return null; }",
        ),
        (
            "src/App.tsx",
            "import { openThing, openWithProps, openRender } from 'external-ui'; import Panel from './Panel'; import Rendered from './Rendered'; import Nested from './Nested'; import { Opener } from './Opener'; import { useItemSelection } from './hook'; function loadA() { return import('./SheetA'); } function Direct() { const [, apply] = useItemSelection(['direct']); openThing(Panel, { onDone: apply }); openWithProps(Nested, { props: { onDone: apply } }); return null; } function A() { const [, apply] = useItemSelection(['a']); return <Opener importer={loadA} onDone={apply} />; } function B() { const [, apply] = useItemSelection(['b']); return <Opener importer={() => import('./SheetB')} onDone={apply} />; } function Render() { const [, apply] = useItemSelection(['render']); openRender(() => <Rendered onDone={apply} />); return null; } export function App() { return <div><Direct /><A /><B /><Render /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openThing'\ncomponent_argument = 0\nprops_argument = 1\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openWithProps'\ncomponent_argument = 0\nprops_argument = 1\nprops_path = ['props']\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'default.openDeferred'\ncomponent_argument = 0\nprops_argument = 2\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openRender'\nrender_argument = 0\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let callsite = |items: &str| {
        report
            .callsites
            .iter()
            .find(|callsite| {
                callsite.factory_arguments["items"]
                    == [QueryValue::Array {
                        elements: vec![QueryValue::String {
                            value: items.to_owned(),
                        }],
                    }]
            })
            .unwrap_or_else(|| panic!("callsite for {items}"))
    };
    let actions = |items: &str| {
        let mut actions = callsite(items)
            .capability
            .calls
            .iter()
            .flat_map(|call| &call.arguments["action"])
            .map(|value| match value {
                QueryValue::String { value } => value.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        actions.sort();
        actions
    };
    // A component and its props, with the props at the top of the argument or under a path.
    assert_eq!(actions("direct"), ["nested", "panel"]);
    // The opener's component comes from the importer each element passes, so each sheet's
    // calls belong only to the callsite whose element opened it.
    assert_eq!(actions("a"), ["sheet_a"]);
    assert_eq!(actions("b"), ["sheet_b"]);
    assert_eq!(actions("render"), ["rendered"]);
    for items in ["direct", "a", "b", "render"] {
        assert!(
            callsite(items).capability.escapes.is_empty(),
            "{items}: {:?}",
            callsite(items).capability.escapes
        );
        // Exploration renders what the openers open, so it runs each call.
        assert!(
            callsite(items)
                .capability
                .calls
                .iter()
                .all(|call| call.explored),
            "{items}: {:?}",
            callsite(items).capability.calls
        );
    }
    let via = &callsite("a").capability.calls[0].via;
    assert!(
        via.iter()
            .any(|step| step == "props of the component Sheets.openDeferred opens"),
        "{via:?}"
    );
}

#[test]
fn exploration_renders_what_render_function_openers_load() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Modal.tsx",
            "export default function Modal({ onClose }) { return <button onClick={() => onClose('modal')} />; }",
        ),
        (
            "src/Menu.tsx",
            "export default function Menu({ onDone }) { return <button onClick={() => onDone('menu')} />; }",
        ),
        (
            "src/Sheet.tsx",
            "export function Sheet({ onDone }) { return <button onClick={() => onDone('sheet')} />; }",
        ),
        (
            "src/Resolved.tsx",
            "export default function Resolved({ onDone }) { return <button onClick={() => onDone('resolved')} />; }",
        ),
        (
            "src/App.tsx",
            "import { openDialogDeferred, openPopupDeferred, openSheet } from 'external-ui'; import { useItemSelection } from './hook'; import Resolved from './Resolved'; function Shown() { const [, apply] = useItemSelection(['shown']); openDialogDeferred(async () => { const { default: Modal } = await import('./Modal'); return (props) => <Modal {...props} onClose={apply} />; }); openSheet(() => import('./Sheet').then((module) => module.Sheet), { onDone: apply }); openDialogDeferred(() => Promise.resolve((props) => <Resolved {...props} onDone={apply} />)); return <button onClick={(event) => openPopupDeferred(event, async () => { const module = await import('./Menu'); return (props) => <module.default {...props} onDone={apply} />; })} />; } export function App() { return <Shown />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openDialogDeferred'\nrender_argument = 0\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openPopupDeferred'\nrender_argument = 1\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openSheet'\ncomponent_argument = 0\nprops_argument = 1\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut explored = callsite
        .capability
        .calls
        .iter()
        .filter(|call| call.explored)
        .flat_map(|call| &call.arguments["action"])
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>();
    explored.sort();
    // An awaited `import()` is the module, so the render function's component is known; a
    // loader's `.then` callback receives the module too, and `Promise.resolve(value)` is the
    // value.
    assert_eq!(
        explored,
        [
            "String { value: \"menu\" }",
            "String { value: \"modal\" }",
            "String { value: \"resolved\" }",
            "String { value: \"sheet\" }"
        ],
        "{:?}",
        callsite.capability.calls
    );
    assert!(
        callsite.capability.escapes.is_empty(),
        "{:?}",
        callsite.capability.escapes
    );
}

#[test]
fn lazy_factories_follow_loaders_declared_by_name() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Panel.tsx",
            "export default function Panel({ onDone }) { return <button onClick={() => onDone('panel')} />; }",
        ),
        (
            "src/Card.tsx",
            "export default function Card({ onDone }) { return <button onClick={() => onDone('card')} />; }",
        ),
        (
            "src/App.tsx",
            "import { deferComponent } from 'external-lazy'; import { useItemSelection } from './hook'; function importPanel() { return import('./Panel'); } const loadCard = () => import('./Card'); const LazyPanel = deferComponent({ load: importPanel }); const LazyCard = deferComponent({ load: loadCard }); function Shown() { const [, apply] = useItemSelection(['shown']); return <div><LazyPanel onDone={apply} /><LazyCard onDone={apply} /></div>; } export function App() { return <Shown />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[lazy_component_factories]]\nmodule = 'external-lazy'\nexport = 'deferComponent'\npromise_property = 'load'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut actions = callsite
        .capability
        .calls
        .iter()
        .map(|call| (format!("{:?}", call.arguments["action"]), call.explored))
        .collect::<Vec<_>>();
    actions.sort();
    // A loader declared as a function or bound to a name is followed like one written inline,
    // by the walk and by exploration.
    assert_eq!(
        actions,
        [
            ("[String { value: \"card\" }]".to_owned(), true),
            ("[String { value: \"panel\" }]".to_owned(), true)
        ],
        "{:?}",
        callsite.capability.escapes
    );
    assert!(callsite.capability.escapes.is_empty());
}

#[test]
fn handlers_that_lead_toward_a_callsite_run_as_possible_callbacks() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/ShareModal.tsx",
            "import { useItemSelection } from './hook'; export function ShareModal() { const [, apply] = useItemSelection(['invite']); return <button onClick={() => apply('invited')} />; }",
        ),
        // Nothing in the model calls the handler: it goes to an unmodeled button.
        (
            "src/App.tsx",
            "import * as React from 'react'; import { Button, openDialogDeferred } from 'external-ui'; import { ShareModal } from './ShareModal'; function openShare() { openDialogDeferred(() => Promise.resolve((props) => <ShareModal {...props} />)); } export function App() { const showModal = React.useCallback(() => { openShare(); }, []); return <Button onClick={showModal} />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openDialogDeferred'\nrender_argument = 0\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    // The handler renders no JSX itself, but it calls `openShare`, which is on the use chain
    // toward the callsite, so it runs as a possible handler and the modal it opens renders.
    assert_eq!(
        callsite.reachability,
        Reachability::Possible,
        "{:?}",
        report.unreached_callsites
    );
    assert!(callsite.capability.calls.iter().all(|call| call.explored));
}

#[test]
fn platform_extensions_resolve_imports_to_one_platform() {
    let files = [
        HOOK,
        (
            "src/Panel.web.tsx",
            "import { useItemSelection } from './hook'; export function Panel() { const [, apply] = useItemSelection(['web']); apply('web_call'); return null; }",
        ),
        (
            "src/Panel.native.tsx",
            "import { useItemSelection } from './hook'; export function Panel() { const [, apply] = useItemSelection(['native']); apply('native_call'); return null; }",
        ),
        (
            "src/web/Only.tsx",
            "import { useItemSelection } from '../hook'; export function Only() { const [, apply] = useItemSelection(['web_dir']); apply('web_dir_call'); return null; }",
        ),
        (
            "src/App.tsx",
            "import { Panel } from './Panel'; export function App() { return <Panel />; }",
        ),
    ];
    let query = "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n";
    let run = |platform: &str, excludes: &str| {
        let fixture = TestProject::new(&files);
        fixture.write(
            "flow.toml",
            &format!(
                "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nplatform_extensions = [{platform}]\nsource_excludes = [{excludes}]\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n"
            ),
        );
        fixture.write("query.toml", query);
        let mut reached = fixture
            .report()
            .callsites
            .iter()
            .map(|callsite| {
                (
                    format!("{:?}", callsite.factory_arguments["items"]),
                    callsite.reachability,
                )
            })
            .collect::<Vec<_>>();
        reached.sort_by(|left, right| left.0.cmp(&right.0));
        reached
    };
    let items = |item: &str| format!("[Array {{ elements: [String {{ value: \"{item}\" }}] }}]");
    // `./Panel` resolves to the native file, and the excluded web files are not callsites.
    assert_eq!(
        run("'.ios', '.native'", "'**/web/**', '**/*.web.tsx'"),
        [(items("native"), Reachability::Reachable)]
    );
    assert_eq!(
        run("'.web'", "'**/*.native.tsx'"),
        [
            (items("web"), Reachability::Reachable),
            (items("web_dir"), Reachability::Unknown)
        ]
    );
}

#[test]
fn require_gives_the_module_like_an_awaited_import() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Panel.tsx",
            "export default function Panel({ onDone }) { return <button onClick={() => onDone('panel')} />; }",
        ),
        (
            "src/Card.tsx",
            "export function Card({ onDone }) { return <button onClick={() => onDone('card')} />; }",
        ),
        (
            "src/App.tsx",
            "import { useItemSelection } from './hook'; const Panel = require('./Panel').default; function Shown() { const [, apply] = useItemSelection(['shown']); const { Card } = require('./Card'); return <div><Panel onDone={apply} /><Card onDone={apply} /></div>; } export function App() { return <Shown />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut actions = callsite
        .capability
        .calls
        .iter()
        .map(|call| (format!("{:?}", call.arguments["action"]), call.explored))
        .collect::<Vec<_>>();
    actions.sort();
    // Exploration and the walk both take `require('./Panel')` as the module.
    assert_eq!(
        actions,
        [
            ("[String { value: \"card\" }]".to_owned(), true),
            ("[String { value: \"panel\" }]".to_owned(), true)
        ],
        "{:?}",
        callsite.capability.escapes
    );
    assert!(callsite.capability.escapes.is_empty());
}

#[test]
fn navigator_factory_members_render_their_screens() {
    let screen = |name: &str| {
        format!(
            "import {{ useItemSelection }} from './hook'; export default function {name}() {{ const [, apply] = useItemSelection(['{name}']); apply('{name}_call'); return <div />; }}"
        )
    };
    let tabs = screen("Tabs");
    let settings = screen("Settings");
    let inner = screen("Inner");
    let fixture = TestProject::new(&[
        HOOK,
        ("src/Tabs.tsx", &tabs),
        ("src/Settings.tsx", &settings),
        ("src/Inner.tsx", &inner),
        (
            "src/App.tsx",
            "import { createStack } from 'external-nav'; import { createNavigatorFactory } from 'external-nav-core'; import Settings from './Settings'; import Inner from './Inner'; const Root = createStack(); function PanelView() { return null; } const Panel = createNavigatorFactory(PanelView)({}); function getTabs() { return require('./Tabs').default; } export function App() { return <Root.Navigator><Root.Screen name=\"tabs\" getComponent={getTabs} /><Root.Group><Root.Screen name=\"settings\" component={Settings} /></Root.Group><Root.Screen name=\"main\">{() => <Panel.Navigator><Panel.Screen name=\"inner\" component={Inner} /></Panel.Navigator>}</Root.Screen></Root.Navigator>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n\
         [[component_consumers]]\nmodule = 'external-nav'\nexport = 'createStack'\nmember = 'Navigator'\nforward_children = true\n\
         [[component_consumers]]\nmodule = 'external-nav'\nexport = 'createStack'\nmember = 'Group'\nforward_children = true\n\
         [[component_consumers]]\nmodule = 'external-nav'\nexport = 'createStack'\nmember = 'Screen'\ncomponent_props = ['component']\nrender_props = ['getComponent']\ninvoke_children = true\n\
         [[component_consumers]]\nmodule = 'external-nav-core'\nexport = 'createNavigatorFactory'\nmember = 'Navigator'\ncurried = true\nforward_children = true\n\
         [[component_consumers]]\nmodule = 'external-nav-core'\nexport = 'createNavigatorFactory'\nmember = 'Screen'\ncurried = true\ncomponent_props = ['component']\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n",
    );
    let report = fixture.report();
    let mut reached = report
        .callsites
        .iter()
        .map(|callsite| {
            (
                format!("{:?}", callsite.factory_arguments["items"]),
                callsite.reachability,
            )
        })
        .collect::<Vec<_>>();
    reached.sort_by(|left, right| left.0.cmp(&right.0));
    let items = |item: &str| format!("[Array {{ elements: [String {{ value: \"{item}\" }}] }}]");
    // A screen's `component`, the component its `getComponent` returns through `require()`, and
    // a screen's function child holding another navigator, from a curried factory, all render.
    assert_eq!(
        reached,
        [
            (items("Inner"), Reachability::Reachable),
            (items("Settings"), Reachability::Reachable),
            (items("Tabs"), Reachability::Reachable)
        ]
    );
}

#[test]
fn declared_render_roots_and_calls_reach_what_no_entry_renders() {
    let fixture = TestProject::new(&[
        HOOK,
        // A modal nothing in the entry's tree opens.
        (
            "src/SettingsModal.tsx",
            "import { useItemSelection } from './hook'; export default function SettingsModal() { const [, apply] = useItemSelection(['modal']); return <button onClick={() => apply('modal_close')} />; }",
        ),
        // A function that opens a sheet, called only from code the model does not reach.
        (
            "src/Sheet.tsx",
            "import { useItemSelection } from './hook'; export default function Sheet() { const [, apply] = useItemSelection(['sheet']); return <button onClick={() => apply('sheet_close')} />; }",
        ),
        (
            "src/showSheet.tsx",
            "import { openSheet } from 'external-ui'; export function showSheet(props) { openSheet(import('./Sheet'), props); }",
        ),
        // A hook a settings registry stores for its renderer to call.
        (
            "src/useNotice.tsx",
            "import { useItemSelection } from './hook'; export function useNotice() { const [visible, apply] = useItemSelection(['notice']); return visible == null ? null : { onClose: () => apply('notice_closed') }; }",
        ),
        (
            "src/StatusCategory.tsx",
            "import { createSection } from 'external-settings'; import { useNotice } from './useNotice'; export const StatusCategory = createSection('status', { useStatusLine: useNotice });",
        ),
        // A frozen registry record whose fields hold a component.
        (
            "src/Banner.tsx",
            "import { useItemSelection } from './hook'; export function Banner() { const [, apply] = useItemSelection(['banner']); return <button onClick={() => apply('banner_close')} />; }",
        ),
        (
            "src/registry.tsx",
            "import { Banner } from './Banner'; export const REGISTRY = Object.freeze({ banner: { component: Banner } });",
        ),
        ("src/App.tsx", "export function App() { return null; }"),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n\
         [[render_roots]]\nmodule = 'src/SettingsModal.tsx'\nexport = 'default'\n\
         [[render_roots]]\nmodule = 'src/showSheet.tsx'\nexport = 'showSheet'\n\
         [[render_roots]]\nmodule = 'src/registry.tsx'\nexport = 'REGISTRY'\n\
         [[render_calls]]\nmodule = 'external-settings'\nexport = 'createSection'\narguments = [1]\n\
         [[component_openers]]\nmodule = 'external-ui'\nexport = 'openSheet'\ncomponent_argument = 0\nprops_argument = 1\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\nscan_callback_bodies = true\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let mut reached = report
        .callsites
        .iter()
        .map(|callsite| {
            (
                format!("{:?}", callsite.factory_arguments["items"]),
                callsite.reachability,
                callsite.capability.calls.iter().all(|call| call.explored),
            )
        })
        .collect::<Vec<_>>();
    reached.sort_by(|left, right| left.0.cmp(&right.0));
    let items = |item: &str| format!("[Array {{ elements: [String {{ value: \"{item}\" }}] }}]");
    // Each is reached only through what the project declares, and says so; exploration runs
    // their calls.
    assert_eq!(
        reached,
        [
            (items("banner"), Reachability::Declared, true),
            (items("modal"), Reachability::Declared, true),
            (items("notice"), Reachability::Declared, true),
            (items("sheet"), Reachability::Declared, true)
        ],
        "{:?}",
        report.unreached_callsites
    );
    assert!(report.unreached_callsites.is_empty());
}

#[test]
fn declared_render_roots_reach_callsites_off_the_entry_chain() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Inner.tsx",
            "import { useItemSelection } from './hook'; export function Inner() { const [, apply] = useItemSelection(['inner']); return <button onClick={() => apply('inner_close')} />; }",
        ),
        // No filter term, and nothing the entry renders uses it.
        (
            "src/Outer.tsx",
            "import { Inner } from './Inner'; export function Outer() { return <div><Inner /></div>; }",
        ),
        (
            "src/Other.tsx",
            "import { useItemSelection } from './hook'; export function Other() { const [, apply] = useItemSelection(['other']); return <button onClick={() => apply('other_close')} />; }",
        ),
        (
            "src/App.tsx",
            "import { Other } from './Other'; export function App() { return <Other />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\nsource_contains_any = ['useItemSelection']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[render_roots]]\nmodule = 'src/Outer.tsx'\nexport = 'Outer'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n",
    );
    let report = fixture.report();
    let mut reached = report
        .callsites
        .iter()
        .map(|callsite| {
            (
                format!("{:?}", callsite.factory_arguments["items"]),
                callsite.reachability,
            )
        })
        .collect::<Vec<_>>();
    reached.sort_by(|left, right| left.0.cmp(&right.0));
    let items = |item: &str| format!("[Array {{ elements: [String {{ value: \"{item}\" }}] }}]");
    // The entry's path explores only toward callsites the backward walk tied to it; a declared
    // root explores what statically leads to any callsite.
    assert_eq!(
        reached,
        [
            (items("inner"), Reachability::Declared),
            (items("other"), Reachability::Reachable)
        ]
    );
}

#[test]
fn a_render_root_that_does_not_resolve_is_an_error() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/App.tsx", "export function App() { return null; }"),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[render_roots]]\nmodule = 'src/App.tsx'\nexport = 'Missing'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n",
    );
    let error = fixture
        .analyzer()
        .query(&fixture.query(), "test-query")
        .expect_err("an unresolved render root fails the query");
    assert!(
        format!("{error:#}").contains("render root export Missing does not resolve"),
        "{error:#}"
    );
}

#[test]
fn callsite_walk_ignores_set_state_follows_chosen_components_and_nested_wrappers() {
    let fixture = TestProject::new(&[
        HOOK,
        // A class that calls `this.setState`, which does not take the instance's props.
        (
            "src/Sidebar.tsx",
            "import * as React from 'react'; import { Tooltips } from './Tooltips'; export class Sidebar extends React.Component { componentDidMount() { this.setState({ open: true }); } render() { return <Tooltips descriptor={this.props.descriptor} />; } }",
        ),
        (
            "src/Tooltips.tsx",
            "export function Tooltips({ descriptor }) { return <button onClick={() => descriptor.apply('tooltip')} />; }",
        ),
        (
            "src/Grid.tsx",
            "export function Grid({ onDone }) { return <button onClick={() => onDone('grid')} />; }",
        ),
        (
            "src/Rows.tsx",
            "export function Rows({ onDone }) { return <button onClick={() => onDone('rows')} />; }",
        ),
        // Two wrappers that each pass their parameter on.
        (
            "src/Sheet.tsx",
            "export function Sheet({ onDone }) { const handle = (kind) => onDone(kind); return <button onClick={() => handle('sheet')} />; }",
        ),
        (
            "src/App.tsx",
            "import { Sidebar } from './Sidebar'; import { Grid } from './Grid'; import { Rows } from './Rows'; import { Sheet } from './Sheet'; import { useItemSelection } from './hook'; function Shown({ compact }) { const [, apply] = useItemSelection(['shown']); const List = compact ? Grid : Rows; const outer = (kind) => apply(kind); return <div><Sidebar descriptor={{ apply }} /><List onDone={apply} /><Sheet onDone={outer} /></div>; } export function App() { return <Shown />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let mut actions = callsite
        .capability
        .calls
        .iter()
        .map(|call| format!("{:?}", call.arguments["action"]))
        .collect::<Vec<_>>();
    actions.sort();
    // The calls inside both wrappers are replaced by the wrappers' calls, both components a
    // local may be are followed, and `this.setState` is not an escape.
    assert_eq!(
        actions,
        [
            "[String { value: \"grid\" }]",
            "[String { value: \"rows\" }]",
            "[String { value: \"sheet\" }]",
            "[String { value: \"tooltip\" }]"
        ],
        "{:?}",
        callsite.capability.escapes
    );
    assert!(
        callsite.capability.escapes.is_empty(),
        "{:?}",
        callsite.capability.escapes
    );
}

#[test]
fn callsite_walk_links_state_written_in_one_place_to_where_it_is_read() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/store.ts",
            "import { create } from 'zustand'; export const usePopoverStore = create(() => ({ onClose: () => {} }));",
        ),
        (
            "src/Reader.tsx",
            "import { usePopoverStore } from './store'; export function Reader() { const onClose = usePopoverStore((state) => state.onClose); return <button onClick={() => onClose('store_read')} />; } export function later() { usePopoverStore.getState().onClose('store_get'); }",
        ),
        (
            "src/SheetA.tsx",
            "export default function SheetA({ onDone }) { return <button onClick={() => onDone('sheet_a')} />; }",
        ),
        (
            "src/SheetB.tsx",
            "export default function SheetB({ onDone }) { return <button onClick={() => onDone('sheet_b')} />; }",
        ),
        (
            "src/Opener.tsx",
            "import Sheets from 'external-ui'; export function Opener(props) { Sheets.openDeferred(props.importer(), 'key', { ...props }); return null; }",
        ),
        (
            "src/App.tsx",
            "import * as React from 'react'; import { usePopoverStore } from './store'; import { Opener } from './Opener'; import { useItemSelection } from './hook';\
             function Publisher() { const [, apply] = useItemSelection(['store']); React.useLayoutEffect(() => { usePopoverStore.setState({ onClose: apply }); }, [apply]); return null; }\
             function Holder() { const [, apply] = useItemSelection(['state']); const [initial] = React.useState(apply); const [handler, setHandler] = React.useState(null); const pending = React.useRef('ref_initial'); React.useEffect(() => { setHandler(() => apply); }, []); const choose = () => { pending.current = 'ref_set'; }; return <div><button onClick={() => handler('state_set')} /><button onClick={() => initial('state_initial')} /><button onMouseEnter={choose} onClick={() => apply(pending.current)} /></div>; }\
             class Panel extends React.Component { componentDidMount() { this.setState({ close: this.props.apply }); } render() { return <button onClick={() => this.state.close('class_state')} />; } }\
             function ClassHost() { const [, apply] = useItemSelection(['class']); return <Panel apply={apply} />; }\
             const CloseContext = React.createContext(null);\
             function Consumer() { const { close } = React.useContext(CloseContext); return <button onClick={() => close('context_read')} />; }\
             function Provider() { const [, apply] = useItemSelection(['context']); return <CloseContext.Provider value={{ close: apply }}><Consumer /></CloseContext.Provider>; }\
             function loadA() { return import('./SheetA'); } function loadB() { return import('./SheetB'); }\
             const SHEETS = [{ id: 'a', importer: loadA }, { id: 'b', importer: loadB }];\
             function Tracked({ extra, ...props }) { return <Opener {...props} />; }\
             function Table({ index }) { const [, apply] = useItemSelection(['table']); const [selected] = React.useState(SHEETS[index]); return <Tracked extra={1} importer={selected.importer} onDone={apply} />; }\
             export function App() { return <div><Publisher /><Holder /><ClassHost /><Provider /><Table index={0} /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[state_stores]]\nkind = 'zustand'\nmodule = 'zustand'\nexport = 'create'\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'default.openDeferred'\ncomponent_argument = 0\nprops_argument = 2\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let callsite = |items: &str| {
        report
            .callsites
            .iter()
            .find(|callsite| {
                callsite.factory_arguments["items"]
                    == [QueryValue::Array {
                        elements: vec![QueryValue::String {
                            value: items.to_owned(),
                        }],
                    }]
            })
            .unwrap_or_else(|| panic!("callsite for {items}"))
    };
    let actions = |items: &str| {
        let callsite = callsite(items);
        assert!(
            callsite.capability.escapes.is_empty(),
            "{items}: {:?}",
            callsite.capability.escapes
        );
        let mut actions = callsite
            .capability
            .calls
            .iter()
            .flat_map(|call| &call.arguments["action"])
            .map(|value| match value {
                QueryValue::String { value } => value.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        actions.sort();
        actions
    };
    // A zustand store's setState reaches its selector and getState reads in another file.
    assert_eq!(actions("store"), ["store_get", "store_read"]);
    // useState holds its initial value and what an updater returns, and a ref's current holds
    // its initial value and what is assigned to it.
    assert_eq!(
        actions("state"),
        ["ref_initial", "ref_set", "state_initial", "state_set"]
    );
    // this.setState reaches this.state in the class's methods.
    assert_eq!(actions("class"), ["class_state"]);
    // A context's value reaches each useContext.
    assert_eq!(actions("context"), ["context_read"]);
    // A sheet chosen from a table through state, spread on through a wrapper's props, opens
    // as any sheet the table names, and says it was inferred.
    assert_eq!(actions("table"), ["sheet_a", "sheet_b"]);
    assert!(callsite("table").capability.calls.iter().all(|call| {
        call.via
            .iter()
            .any(|step| step.starts_with("props passed with the component"))
    }));
}

#[test]
fn an_instance_reading_a_table_entry_requests_any_entry_of_the_table() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/Selected.tsx",
            "import { useItemSelection } from './hook'; export function Selected({ contentTypes, children }) { const [, apply] = useItemSelection(contentTypes); return children({ apply }); }",
        ),
        (
            "src/SheetA.tsx",
            "export default function SheetA({ onDone }) { return <button onClick={() => onDone('sheet_a')} />; }",
        ),
        (
            "src/SheetB.tsx",
            "export default function SheetB({ onDone }) { return <button onClick={() => onDone('sheet_b')} />; }",
        ),
        (
            "src/App.tsx",
            "import * as React from 'react'; import { pick } from 'external-pick'; import { openSheet } from 'external-ui'; import { Selected } from './Selected';\
             function loadA() { return import('./SheetA'); } function loadB() { return import('./SheetB'); }\
             const SHEETS = [{ id: 'a', importer: loadA }, { id: 'b', importer: loadB }];\
             function Opener(props) { openSheet(props.importer(), props); return null; }\
             function Table() { const [selected, setSelected] = React.useState(null); React.useEffect(() => { setSelected(pick(SHEETS)); }, []); return <Selected contentTypes={[selected.id]}>{({ apply }) => <Opener importer={selected.importer} onDone={apply} />}</Selected>; }\
             export function App() { return <Table />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n[[component_openers]]\nmodule = 'external-ui'\nexport = 'openSheet'\ncomponent_argument = 0\nprops_argument = 1\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("one callsite: {:?}", report.callsites);
    };
    let strings = |values: &[QueryValue]| {
        let mut strings = values
            .iter()
            .map(|value| match value {
                QueryValue::String { value } => value.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        strings.sort();
        strings
    };
    let mut calls = callsite
        .capability
        .calls
        .iter()
        .map(|call| {
            (
                strings(&call.arguments["action"]),
                strings(&call.elements["items"]),
            )
        })
        .collect::<Vec<_>>();
    calls.sort();
    // The sheet and the content types both come from the entry picked from the table, so each
    // sheet's calls are attributed to every entry's item rather than to none.
    assert_eq!(
        calls,
        [
            (
                vec!["sheet_a".to_owned()],
                vec!["a".to_owned(), "b".to_owned()]
            ),
            (
                vec!["sheet_b".to_owned()],
                vec!["a".to_owned(), "b".to_owned()]
            )
        ],
        "{:?}",
        callsite.capability.escapes
    );
}

#[test]
fn pushes_through_record_properties_reach_the_factory_argument() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { flag } from 'external-flags'; import { useItemSelection } from './hook'; function Picker({ items }) { const [, apply] = useItemSelection(items); apply('pick'); return null; } function Sorted() { const items = [Kind.A]; items.sort(); const [, apply] = useItemSelection(items); apply('sorted'); return null; } export function App() { const contents = { settings: [] }; if (flag) { contents.settings.push(Kind.A); } contents.settings.push(Kind.B); return <div><Picker items={[...contents.settings, Kind.C]} /><Sorted /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let by_enclosing = |prefix: &str| {
        report
            .callsites
            .iter()
            .find(|callsite| {
                callsite
                    .enclosing
                    .as_deref()
                    .is_some_and(|enclosing| enclosing.starts_with(prefix))
            })
            .expect("callsite")
    };
    let member = |name: &str, value: i64| QueryValue::EnumMember {
        enum_name: "Kind".to_owned(),
        member_name: name.to_owned(),
        value,
    };
    let array = |elements: Vec<QueryValue>| QueryValue::Array { elements };
    // A push into `contents.settings` reaches the array after the branch joins the record.
    assert_eq!(
        by_enclosing("Picker ").factory_arguments["items"],
        [QueryValue::Alternatives {
            values: vec![
                array(vec![member("A", 1), member("B", 2), member("C", 3)]),
                array(vec![member("B", 2), member("C", 3)]),
            ]
        }]
    );
    // An in-place method the model does not follow leaves the array unknown, not stale.
    let sorted = by_enclosing("Sorted ");
    assert!(
        !sorted.factory_arguments_resolved,
        "{:?}",
        sorted.factory_arguments
    );
}

#[test]
fn conditions_on_the_path_attribute_calls_to_the_items_they_test() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/kinds.ts",
            "export enum Kind { A = 1, B = 2, C = 3, D = 4 }",
        ),
        (
            "src/Selected.tsx",
            "import { useItemSelection } from './hook'; export default function Selected({ items, children }) { const [visible, apply] = useItemSelection(items); return <>{children({ visible, apply })}</>; }",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { pick } from 'external-pick'; import { useItemSelection } from './hook'; import Selected from './Selected'; function NoticeA({ onClose }) { return <button onClick={() => onClose('a')} />; } function NoticeD({ onClose }) { return <button onClick={() => onClose('d')} />; } function Tooltips({ descriptor }) { switch (descriptor.kind) { case Kind.A: return <NoticeA onClose={descriptor.close} />; case Kind.D: return <NoticeD onClose={descriptor.close} />; default: return null; } } function Header() { const [visible, apply] = useItemSelection([Kind.A, Kind.B]); const [, applyOther] = pick(); const descriptor = visible != null ? { kind: visible, close: apply } : { kind: Kind.D, close: applyOther }; if (visible !== Kind.B) { return <Tooltips descriptor={descriptor} />; } return <button onClick={() => apply('b')} />; } function Notices() { return <Selected items={[Kind.C, Kind.D]}>{({ visible, apply }) => { if (visible === Kind.C) { return <button onClick={() => apply('c')} />; } return <button onClick={() => apply('other')} />; }}</Selected>; } export function App() { return <div><Header /><Notices /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let names = |values: &[QueryValue]| {
        let mut names = Vec::new();
        for value in values {
            collect_enum_members(value, &mut names);
        }
        names
    };
    let summary = |calls: &[code_flow::query::QueryCapabilityCall]| {
        let mut rows = calls
            .iter()
            .map(|call| {
                let QueryValue::String { value } = &call.arguments["action"][0] else {
                    panic!("{:?}", call.arguments);
                };
                (value.clone(), names(&call.elements["items"]))
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };
    let header = report
        .callsites
        .iter()
        .find(|callsite| {
            callsite
                .enclosing
                .as_deref()
                .is_some_and(|name| name.starts_with("Header "))
        })
        .expect("header callsite");
    // The switch case for `Kind.D` renders another hook's callback, so this callsite's result,
    // which only ever selects A or B, cannot reach it; the early return rules B out of the case
    // for A.
    assert_eq!(
        summary(&header.capability.calls),
        [
            ("a".to_owned(), vec!["A".to_owned()]),
            ("b".to_owned(), vec!["B".to_owned()]),
        ]
    );
    assert_eq!(
        summary(&header.capability.excluded_calls),
        [("d".to_owned(), Vec::<String>::new())]
    );
    // A wrapper's calls take the items of the instance that renders it, narrowed by the
    // conditions in its function child.
    let selected = report
        .callsites
        .iter()
        .find(|callsite| {
            callsite
                .enclosing
                .as_deref()
                .is_some_and(|name| name.starts_with("Selected "))
        })
        .expect("wrapper callsite");
    assert_eq!(
        summary(&selected.capability.calls),
        [
            ("c".to_owned(), vec!["C".to_owned()]),
            ("other".to_owned(), vec!["D".to_owned()]),
        ]
    );
    assert!(
        selected
            .capability
            .calls
            .iter()
            .all(|call| call.instance.is_some()),
        "{:?}",
        selected.capability.calls
    );
}

#[test]
fn hook_items_reach_callers_and_default_export_aliases_are_followed() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2 }"),
        (
            "src/Notice.tsx",
            "import * as React from 'react'; class Notice extends React.Component { handleClose = () => { this.props.onClose('class_close'); }; render() { return <button onClick={this.handleClose} />; } } export default Notice;",
        ),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { enabled } from 'external-flags'; import { useItemSelection } from './hook'; import Notice from './Notice'; function useBanner(open) { const [visible, apply] = useItemSelection(enabled ? [Kind.A] : []); return { visible, apply }; } function Banner() { const { apply } = useBanner(true); return <Notice onClose={apply} />; } export function App() { return <Banner />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("{:?}", report.callsites);
    };
    let [call] = callsite.capability.calls.as_slice() else {
        panic!("{:?}", callsite.capability.calls);
    };
    // The class behind `export default Notice` is followed, and the call keeps the hook's own
    // items: its argument does not read the hook's parameter, so every caller gets the same.
    assert_eq!(
        call.arguments["action"],
        [QueryValue::String {
            value: "class_close".to_owned()
        }]
    );
    let mut names = Vec::new();
    for value in &call.elements["items"] {
        collect_enum_members(value, &mut names);
    }
    assert_eq!(names, ["A"]);
    assert!(call.elements_complete);
}

#[test]
fn calls_of_a_wrapper_that_forwards_its_parameter_give_the_argument() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/App.tsx",
            "import * as React from 'react'; import { useItemSelection } from './hook'; import { log } from 'external-log'; function Notice({ onClose }) { return <button onClick={() => onClose('from_child')} />; } function Notices() { const [, apply] = useItemSelection(['wrapped']); const wrapped = React.useCallback((kind = 'default_kind') => { log(kind); apply(kind); }, [apply]); return <div><Notice onClose={wrapped} /><button onClick={() => wrapped()} /></div>; } export function App() { return <Notices />; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", TUPLE_QUERY);
    let report = fixture.report();
    let [callsite] = report.callsites.as_slice() else {
        panic!("{:?}", report.callsites);
    };
    assert_eq!(callsite.capability.status, QueryCapabilityStatus::Called);
    let mut calls = callsite
        .capability
        .calls
        .iter()
        .map(|call| {
            let QueryValue::String { value } = &call.arguments["action"][0] else {
                panic!("{:?}", call.arguments);
            };
            (value.clone(), call.explored, call.via.join(" / "))
        })
        .collect::<Vec<_>>();
    calls.sort();
    // The call inside the wrapper passes its parameter on, so the wrapper's calls replace it.
    assert_eq!(
        calls,
        [
            (
                "default_kind".to_owned(),
                true,
                "through the callback passed to React.useCallback".to_owned()
            ),
            (
                "from_child".to_owned(),
                true,
                "through the callback passed to React.useCallback / prop onClose of <Notice>"
                    .to_owned()
            ),
        ]
    );
}

#[test]
fn unresolved_factory_arguments_report_pushed_elements_and_caller_values() {
    let pushes = (0..6)
        .map(|index| format!("if (flags.f{index}) {{ items.push(Kind.K{index}); }}"))
        .collect::<Vec<_>>()
        .concat();
    let members = (0..6)
        .map(|index| format!("K{index} = {index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let app = format!(
        "import {{ Kind }} from './kinds'; import {{ flags }} from 'external-flags'; import {{ useItemSelection }} from './hook'; function Many() {{ const items = []; {pushes} const [, apply] = useItemSelection(items); apply('many'); return null; }} function Card({{ kind }}) {{ const [, apply] = useItemSelection([kind]); apply('card'); return null; }} function Cards() {{ return <div><Card kind={{Kind.K1}} /><Card kind={{Kind.K2}} /></div>; }} export function App() {{ return <Many />; }}"
    );
    let kinds = format!("export enum Kind {{ {members} }}");
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", kinds.as_str()),
        ("src/App.tsx", app.as_str()),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write(
        "query.toml",
        "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n",
    );
    let report = fixture.report();
    let names = |values: &[QueryValue]| {
        let mut names = Vec::new();
        for value in values {
            collect_enum_members(value, &mut names);
        }
        names
    };
    let by_enclosing = |prefix: &str| {
        report
            .callsites
            .iter()
            .find(|callsite| {
                callsite
                    .enclosing
                    .as_deref()
                    .is_some_and(|enclosing| enclosing.starts_with(prefix))
            })
            .expect("callsite")
    };
    // Six independent pushes exceed the heap alternative budget, but the source still names
    // every element the array may hold.
    let many = by_enclosing("Many ");
    assert!(!many.factory_arguments_resolved);
    assert_eq!(
        names(&many.possible_elements["items"]),
        ["K0", "K1", "K2", "K3", "K4", "K5"]
    );
    // No explored path renders the cards, so the value comes from each caller's props.
    let card = by_enclosing("Card ");
    assert!(!card.factory_arguments_resolved);
    let from_callers = card.values_from_callers["items"]
        .iter()
        .map(|caller| names(std::slice::from_ref(&caller.value)))
        .collect::<Vec<_>>();
    assert_eq!(from_callers, [["K1"], ["K2"]]);
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
fn shared_layer_renders_children_captured_by_closures_and_nested_elements() {
    // Every use reaches the consumer site through a callback built in the layer and an element
    // built in the positioned wrapper. Each carries a different use's children.
    let uses = (0..80)
        .map(|index| format!("<Positioned><Leaf action='use-{index}' /></Positioned>"))
        .collect::<Vec<_>>()
        .concat();
    let app = format!(
        "import {{ Leaf }} from './Leaf'; function Consumer({{ children }}) {{ return children(null); }} function Layer({{ children }}) {{ return <Consumer>{{(value) => children}}</Consumer>; }} function Inner({{ children }}) {{ return children; }} function Positioned(props) {{ return <Layer><Inner {{...props}} /></Layer>; }} export function App() {{ return <div>{uses}</div>; }}"
    );
    let (exact, other) = exact_actions(&[("src/App.tsx", app.as_str())]);
    assert_eq!(exact.len(), 80, "{other:?}");
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
