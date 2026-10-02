mod support;

use std::collections::BTreeMap;

use code_flow::csv_report::query_report_csv;
use support::TestProject;

const HOOK: (&str, &str) = (
    "src/hook.ts",
    "export function useItemSelection(_items: unknown[]) { return [null, (_action: string) => {}]; }",
);

const QUERY: &str = "schema_version = 1\nid = 'tuple'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'acceptance'\nmodule = 'src/hook.ts'\nexport = 'useItemSelection'\n[[factory_arguments]]\nindex = 0\nlabel = 'items'\n[[factory_arguments]]\nindex = 1\nlabel = 'group'\n[capability]\nreturned_index = 1\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'action'\n";

/// Parses the CSV the module writes, including quoted fields.
fn parse(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match (quoted, character) {
            (true, '"') if characters.peek() == Some(&'"') => {
                characters.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (false, '"') => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (_, character) => field.push(character),
        }
    }
    let header = records.remove(0);
    records
        .into_iter()
        .map(|record| header.iter().cloned().zip(record).collect())
        .collect()
}

#[test]
fn csv_has_a_row_per_call_and_item_and_one_for_items_without_calls() {
    let fixture = TestProject::new(&[
        HOOK,
        ("src/kinds.ts", "export enum Kind { A = 1, B = 2, C = 3 }"),
        (
            "src/App.tsx",
            "import { Kind } from './kinds'; import { useItemSelection } from './hook'; import { report } from 'external-report'; function Banner() { const [visible, apply] = useItemSelection([Kind.A, Kind.B, Kind.C], 'top, left'); if (visible === Kind.A) { return <button onClick={() => apply('a \"quoted\"')} />; } if (visible === Kind.B) { return <button onClick={() => apply('b')} />; } return null; } function Lost() { const [, apply] = useItemSelection([Kind.C], 'side'); report(apply); return null; } function Quiet() { const [visible] = useItemSelection([Kind.B], 'side'); return visible; } export function App() { return <div><Banner /><Lost /><Quiet /></div>; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", QUERY);
    let report = fixture.report();
    let csv = query_report_csv(&report, &fixture.query());
    let header = csv.lines().next().unwrap();
    assert!(
        header.starts_with("row_kind,item.items,item_complete,callsite_path,callsite_line,"),
        "{header}"
    );
    assert!(header.contains(",arg.action,"), "{header}");
    assert!(header.ends_with(",factory.group"), "{header}");
    let rows = parse(&csv);
    let mut summary = rows
        .iter()
        .map(|row| {
            (
                row["enclosing"].clone(),
                row["row_kind"].clone(),
                row["item.items"].clone(),
                row["arg.action"].clone(),
            )
        })
        .collect::<Vec<_>>();
    summary.sort();
    let row = |enclosing: &str, kind: &str, item: &str, action: &str| {
        (
            enclosing.to_owned(),
            kind.to_owned(),
            item.to_owned(),
            action.to_owned(),
        )
    };
    // Each call names the one item its condition allows; an item no call applies to still has
    // a row, and so does each item of a callsite whose result escapes or is never called.
    assert_eq!(
        summary,
        [
            row("Banner", "call", "Kind.A", "a \"quoted\""),
            row("Banner", "call", "Kind.B", "b"),
            row("Banner", "no_call", "Kind.C", ""),
            row("Lost", "escape", "Kind.C", ""),
            row("Quiet", "no_call", "Kind.B", ""),
        ]
    );
    let banner = rows
        .iter()
        .find(|row| row["enclosing"] == "Banner" && row["row_kind"] == "call")
        .unwrap();
    assert_eq!(banner["factory.group"], "top, left");
    assert_eq!(banner["callsite_path"], "src/App.tsx");
    assert_eq!(banner["item_complete"], "true");
    assert_eq!(banner["status"], "called");
    let lost = rows.iter().find(|row| row["enclosing"] == "Lost").unwrap();
    assert_eq!(lost["status"], "escapes");
    assert!(lost["note"].contains("passed to report"), "{lost:?}");
}

#[test]
fn view_writes_a_private_page_from_the_csv() {
    let fixture = TestProject::new(&[
        HOOK,
        (
            "src/App.tsx",
            "import { useItemSelection } from './hook'; export function App() { const [, apply] = useItemSelection(['only']); apply('done'); return null; }",
        ),
    ]);
    fixture.write(
        "flow.toml",
        "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/App.tsx'\nexport = 'App'\n",
    );
    fixture.write("query.toml", QUERY);
    let run = |args: &[&std::ffi::OsStr]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_follower"))
            .args(args)
            .output()
            .expect("run CLI")
    };
    let flow = fixture.root.join("flow.toml");
    let query = fixture.root.join("query.toml");
    let output = run(&[
        "query".as_ref(),
        "--project".as_ref(),
        flow.as_os_str(),
        "--query".as_ref(),
        query.as_os_str(),
        "--format".as_ref(),
        "csv".as_ref(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let csv = fixture.root.join("report.csv");
    std::fs::write(&csv, &output.stdout).unwrap();
    let viewed = run(&["view".as_ref(), csv.as_os_str()]);
    assert!(
        viewed.status.success(),
        "{}",
        String::from_utf8_lossy(&viewed.stderr)
    );
    let page = std::fs::read_to_string(fixture.root.join("report.html")).unwrap();
    assert!(page.contains("csv-data"));
    assert!(page.contains("done"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fixture.root.join("report.html"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
