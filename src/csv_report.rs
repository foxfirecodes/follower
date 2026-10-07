//! A flat table of a query report for agents and spreadsheets: one row per call made with a
//! factory result and item it applies to, plus rows for escapes, excluded calls, and callsites
//! whose result is never called. Column names come from the query's labels, so the table has
//! the same shape for any factory.
//!
//! Columns: `row_kind` (`call`, `excluded_call`, `escape`, or `no_call`); `item.<label>`, one
//! element of the first array factory argument, with `item_complete`; the callsite's path, line,
//! enclosing function, reachability, and status; the call's path and line, `arg.<label>` for each
//! projected invocation argument, `arguments_resolved`, and `found` (`explored`, `source`, or
//! `inferred`); the call's `context`, `via`, `instance`, and `conditions`; `unfollowed`, the
//! number of places the callsite's result went that the walk does not follow; a `note`; and
//! `factory.<label>` for other factory arguments. Multiple values in a cell are joined with ` | `.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use crate::query::{
    QueryCallsiteValues, QueryCapabilityCall, QueryCapabilityStatus, QueryLocation, QueryReport,
    QuerySpec, QueryValue, Reachability,
};

/// Builds the table. The item column holds one element per row of the first factory argument
/// whose values are arrays, so a lookup by item is a filter on one column.
#[allow(clippy::too_many_lines)]
pub fn query_report_csv(report: &QueryReport, query: &QuerySpec) -> String {
    let item_label = query
        .factory_arguments
        .iter()
        .map(|projection| projection.label.clone())
        .find(|label| {
            report.callsites.iter().any(|callsite| {
                callsite.possible_elements.contains_key(label)
                    || callsite
                        .factory_arguments
                        .get(label)
                        .is_some_and(|values| values.iter().any(holds_arrays))
            })
        });
    let factory_labels = query
        .factory_arguments
        .iter()
        .map(|projection| projection.label.clone())
        .filter(|label| Some(label) != item_label.as_ref())
        .collect::<Vec<_>>();
    let argument_labels = query
        .capability
        .invocation_arguments
        .iter()
        .map(|projection| projection.label.clone())
        .collect::<Vec<_>>();
    let mut header = vec!["row_kind".to_owned()];
    if let Some(label) = &item_label {
        header.push(format!("item.{label}"));
        header.push("item_complete".to_owned());
    }
    header.extend(
        [
            "callsite_path",
            "callsite_line",
            "enclosing",
            "reachability",
            "unreached_reason",
            "status",
            "call_path",
            "call_line",
        ]
        .map(str::to_owned),
    );
    header.extend(argument_labels.iter().map(|label| format!("arg.{label}")));
    header.extend(
        [
            "arguments_resolved",
            "found",
            "context",
            "via",
            "instance",
            "conditions",
            "unfollowed",
            "note",
        ]
        .map(str::to_owned),
    );
    header.extend(
        factory_labels
            .iter()
            .map(|label| format!("factory.{label}")),
    );

    let mut paths = PathDisplay::default();
    let mut rows = Vec::new();
    let mut callsites = report.callsites.iter().collect::<Vec<_>>();
    callsites.sort_by_key(|callsite| location_key(callsite.location.as_ref()));
    for callsite in callsites {
        let items = item_label
            .as_ref()
            .map(|label| callsite_items(callsite, label));
        let base = |paths: &mut PathDisplay, kind: &str| {
            let mut row = Row::default();
            row.set("row_kind", kind);
            if let Some(location) = &callsite.location {
                row.set("callsite_path", paths.show(&location.path));
                row.set("callsite_line", location.start_line.to_string());
            }
            row.set(
                "enclosing",
                callsite
                    .enclosing
                    .as_deref()
                    .map_or("", |enclosing| enclosing.split(" (").next().unwrap_or("")),
            );
            row.set("reachability", reachability_text(callsite.reachability));
            if let Some(reason) = callsite.unreached_reason {
                row.set("unreached_reason", snake(&format!("{reason:?}")));
            }
            row.set(
                "status",
                snake(&format!("{:?}", callsite.capability.status)),
            );
            row.set("unfollowed", callsite.capability.escapes.len().to_string());
            for label in &factory_labels {
                if let Some(values) = callsite.factory_arguments.get(label) {
                    row.set(&format!("factory.{label}"), values_text(values));
                }
            }
            row
        };
        let expand = |row: Row, elements: Option<(&[QueryValue], bool)>, rows: &mut Vec<Row>| {
            let (Some(label), Some((elements, complete))) = (&item_label, elements) else {
                rows.push(row);
                return;
            };
            let column = format!("item.{label}");
            let complete = if complete { "true" } else { "false" };
            if elements.is_empty() {
                let mut row = row;
                row.set("item_complete", complete);
                rows.push(row);
                return;
            }
            for element in elements {
                let mut row = row.clone();
                row.set(&column, value_text(element));
                row.set("item_complete", complete);
                rows.push(row);
            }
        };
        for (kind, calls) in [
            ("call", &callsite.capability.calls),
            ("excluded_call", &callsite.capability.excluded_calls),
        ] {
            for call in calls {
                let mut row = base(&mut paths, kind);
                fill_call(&mut row, call, &argument_labels, &mut paths);
                if kind == "excluded_call" {
                    row.set(
                        "note",
                        "the conditions on its path match nothing this callsite requests",
                    );
                }
                let elements = item_label.as_ref().map(|label| {
                    (
                        call.elements.get(label).map_or(&[][..], Vec::as_slice),
                        call.elements_complete,
                    )
                });
                // An excluded call applies to no requested item.
                let elements = if kind == "excluded_call" {
                    elements.map(|(_, complete)| (&[][..], complete))
                } else {
                    elements
                };
                expand(row, elements, &mut rows);
            }
        }
        // Where the result went that the walk does not follow. When calls were found these are
        // one row each; when none were, they are the answer for every item the callsite requests.
        let no_calls = callsite.capability.calls.is_empty();
        for escape in &callsite.capability.escapes {
            let mut row = base(&mut paths, "escape");
            if let Some(location) = &escape.location {
                row.set("call_path", paths.show(&location.path));
                row.set("call_line", location.start_line.to_string());
            }
            row.set("context", escape.context.join(" < "));
            row.set("via", escape.via.join(" > "));
            row.set("note", &escape.detail);
            if no_calls {
                expand(
                    row,
                    items
                        .as_ref()
                        .map(|(items, complete)| (items.as_slice(), *complete)),
                    &mut rows,
                );
            } else {
                rows.push(row);
            }
        }
        // Every item the callsite requests appears at least once: an item no call applies to
        // gets a row saying so, unless the escapes above already stand for it.
        let covered = callsite
            .capability
            .calls
            .iter()
            .flat_map(|call| {
                item_label
                    .as_ref()
                    .and_then(|label| call.elements.get(label))
                    .into_iter()
                    .flatten()
            })
            .cloned()
            .collect::<Vec<_>>();
        let explained_by_escapes = no_calls && !callsite.capability.escapes.is_empty();
        if !explained_by_escapes {
            let note = if no_calls {
                match callsite.capability.status {
                    QueryCapabilityStatus::Unused => {
                        "the callsite does not bind the result, or never uses it"
                    }
                    _ => {
                        "the result is used, for example passed to code that ignores it, but never called"
                    }
                }
            } else {
                "no call found for this item; see the callsite's other rows"
            };
            let uncovered = items.as_ref().map(|(items, complete)| {
                (
                    items
                        .iter()
                        .filter(|item| !covered.contains(item))
                        .cloned()
                        .collect::<Vec<_>>(),
                    *complete,
                )
            });
            let needed = match &uncovered {
                Some((uncovered, _)) => !uncovered.is_empty() || no_calls,
                None => no_calls,
            };
            if needed {
                let mut row = base(&mut paths, "no_call");
                row.set("note", note);
                expand(
                    row,
                    uncovered
                        .as_ref()
                        .map(|(items, complete)| (items.as_slice(), *complete)),
                    &mut rows,
                );
            }
        }
    }

    let mut out = String::new();
    write_record(&mut out, header.iter().map(String::as_str));
    for row in &rows {
        write_record(
            &mut out,
            header
                .iter()
                .map(|column| row.0.get(column).map_or("", String::as_str)),
        );
    }
    out
}

fn fill_call(
    row: &mut Row,
    call: &QueryCapabilityCall,
    argument_labels: &[String],
    paths: &mut PathDisplay,
) {
    if let Some(location) = &call.location {
        row.set("call_path", paths.show(&location.path));
        row.set("call_line", location.start_line.to_string());
    }
    let mut unknown = Vec::new();
    for label in argument_labels {
        let values = call.arguments.get(label).map_or(&[][..], Vec::as_slice);
        row.set(&format!("arg.{label}"), values_text(values));
        for value in values {
            if let QueryValue::Unknown { reason } = value {
                unknown.push(format!("{label} is unknown: {}", reason.replace('_', " ")));
            }
        }
    }
    row.set(
        "arguments_resolved",
        if call.arguments_resolved {
            "true"
        } else {
            "false"
        },
    );
    let inferred = call
        .via
        .iter()
        .any(|step| step.starts_with("props passed with the component"));
    row.set(
        "found",
        if call.explored {
            "explored"
        } else if inferred {
            "inferred"
        } else {
            "source"
        },
    );
    row.set("context", call.context.join(" < "));
    row.set("via", call.via.join(" > "));
    if let Some(instance) = &call.instance {
        row.set(
            "instance",
            format!("{}:{}", paths.show(&instance.path), instance.start_line),
        );
    }
    let mut conditions = call
        .guards
        .iter()
        .map(|set| format!("in {}", values_text(set)))
        .collect::<Vec<_>>();
    if !call.guards_not.is_empty() {
        conditions.push(format!(
            "not {}",
            call.guards_not
                .iter()
                .map(value_text)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    row.set("conditions", conditions.join("; "));
    row.set("note", unknown.join("; "));
}

/// The items a callsite requests, and whether all of them are known.
fn callsite_items(callsite: &QueryCallsiteValues, label: &str) -> (Vec<QueryValue>, bool) {
    let mut items = Vec::new();
    let mut add = |value: &QueryValue| {
        if !items.contains(value) {
            items.push(value.clone());
        }
    };
    if let Some(elements) = callsite.possible_elements.get(label) {
        elements.iter().for_each(&mut add);
    } else {
        for value in callsite.factory_arguments.get(label).into_iter().flatten() {
            collect_elements(value, &mut add);
        }
    }
    for caller in callsite
        .values_from_callers
        .get(label)
        .into_iter()
        .flatten()
    {
        collect_elements(&caller.value, &mut add);
    }
    let complete = items
        .iter()
        .all(|item| !matches!(item, QueryValue::Unknown { .. }));
    (items, complete)
}

fn collect_elements(value: &QueryValue, add: &mut impl FnMut(&QueryValue)) {
    // An element chosen among values, such as `table[key]`, may be any of them.
    fn add_element(element: &QueryValue, add: &mut impl FnMut(&QueryValue)) {
        if let QueryValue::Alternatives { values } = element {
            // A choice that may find nothing, such as a missing table entry, adds no item.
            for value in values {
                if !matches!(value, QueryValue::Null | QueryValue::Undefined) {
                    add_element(value, add);
                }
            }
        } else {
            add(element);
        }
    }
    match value {
        QueryValue::Array { elements } => {
            for element in elements {
                add_element(element, add);
            }
        }
        QueryValue::Alternatives { values } => {
            for value in values {
                collect_elements(value, add);
            }
        }
        QueryValue::Unknown { .. } => add(value),
        _ => {}
    }
}

fn holds_arrays(value: &QueryValue) -> bool {
    match value {
        QueryValue::Array { .. } => true,
        QueryValue::Alternatives { values } => values.iter().any(holds_arrays),
        _ => false,
    }
}

/// A value as a cell: `Kind.A`, a bare string, or `?reason` for an unknown.
pub fn value_text(value: &QueryValue) -> String {
    match value {
        QueryValue::Null => "null".to_owned(),
        QueryValue::Undefined => "undefined".to_owned(),
        QueryValue::String { value } => value.clone(),
        QueryValue::Number { value } => value.to_string(),
        QueryValue::Boolean { value } => value.to_string(),
        QueryValue::EnumMember {
            enum_name,
            member_name,
            ..
        } => format!("{enum_name}.{member_name}"),
        QueryValue::Array { elements } => format!(
            "[{}]",
            elements
                .iter()
                .map(value_text)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        QueryValue::Alternatives { values } => values_text(values),
        QueryValue::Unknown { reason } => format!("?{reason}"),
    }
}

fn values_text(values: &[QueryValue]) -> String {
    values
        .iter()
        .map(value_text)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn reachability_text(reachability: Reachability) -> &'static str {
    match reachability {
        Reachability::Reachable => "reachable",
        Reachability::Possible => "possible",
        Reachability::Declared => "declared",
        Reachability::Unknown => "unknown",
    }
}

/// `CalledWithUnknownArguments` as `called_with_unknown_arguments`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(character.to_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

fn location_key(location: Option<&QueryLocation>) -> (String, u32, u32) {
    location.map_or_else(
        || (String::new(), 0, 0),
        |location| {
            (
                location.path.clone(),
                location.start_line,
                location.start_column,
            )
        },
    )
}

#[derive(Clone, Default)]
struct Row(BTreeMap<String, String>);

impl Row {
    fn set(&mut self, column: &str, value: impl Into<String>) {
        self.0.insert(column.to_owned(), value.into());
    }
}

/// Paths relative to the repository that holds them, found by the nearest `.git`, so the table
/// reads the same on any machine.
#[derive(Default)]
struct PathDisplay {
    roots: HashMap<PathBuf, Option<PathBuf>>,
}

impl PathDisplay {
    fn show(&mut self, path: &str) -> String {
        let path = Path::new(path);
        if !path.is_absolute() {
            return path.display().to_string();
        }
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let root = self
            .roots
            .entry(directory.clone())
            .or_insert_with(|| {
                directory
                    .ancestors()
                    .find(|ancestor| ancestor.join(".git").exists())
                    .map(Path::to_path_buf)
            })
            .clone();
        root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf))
            .unwrap_or_else(|| path.to_path_buf())
            .display()
            .to_string()
    }
}

/// Writes one CSV record, quoting fields that need it (RFC 4180).
fn write_record<'a>(out: &mut String, fields: impl Iterator<Item = &'a str>) {
    for (index, field) in fields.enumerate() {
        if index > 0 {
            out.push(',');
        }
        if field.contains([',', '"', '\n', '\r']) {
            let _ = write!(out, "\"{}\"", field.replace('"', "\"\""));
        } else {
            out.push_str(field);
        }
    }
    out.push_str("\r\n");
}

/// The records of a CSV, each without its line ending; a quoted field may hold line breaks.
pub fn csv_records(text: &str) -> Vec<&str> {
    let mut records = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (index, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '\n' if !quoted => {
                records.push(text[start..index].trim_end_matches('\r'));
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        records.push(&text[start..]);
    }
    records
}

/// The fields of one CSV record, unquoted (RFC 4180).
fn csv_fields(record: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = record.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' if quoted && characters.peek() == Some(&'"') => {
                field.push('"');
                characters.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    fields.push(field);
    fields
}

/// What the callsites of a query CSV say about one item.
#[derive(Default)]
struct ItemOutcome {
    called: std::collections::BTreeSet<String>,
    unknown: std::collections::BTreeSet<String>,
    not_called: std::collections::BTreeSet<String>,
    arguments: Vec<std::collections::BTreeSet<String>>,
    unresolved: bool,
}

impl ItemOutcome {
    fn outcome(&self, unknown_item: bool) -> &'static str {
        if unknown_item {
            "unknown"
        } else if !self.called.is_empty() {
            if self.unresolved {
                "called_with_unknown_arguments"
            } else {
                "called"
            }
        } else if !self.unknown.is_empty() {
            "unknown"
        } else {
            "not_called"
        }
    }
}

/// One row per item of a query CSV, or of several joined by `follower view`, saying whether any
/// call applies to it across every callsite that requests it: `called`, with the argument values
/// of those calls, `called_with_unknown_arguments`, `not_called` when every callsite that
/// requests it is known to make no call for it, or `unknown`. A callsite leaves its uncalled items
/// unknown when its result escapes or one of its calls applies to items not all known, since that
/// call could be for any of them. Unknown items (`?reason`) are `unknown`. With a `source` column,
/// `sources` gives the outcome in each source and places are prefixed by their source.
#[allow(clippy::too_many_lines)]
pub fn item_outcomes(csv: &str) -> String {
    let records = csv_records(csv);
    let Some((header, records)) = records.split_first() else {
        return String::new();
    };
    let header = csv_fields(header);
    let column = |name: &str| header.iter().position(|column| column == name);
    let Some(item) = header.iter().position(|column| column.starts_with("item.")) else {
        return String::new();
    };
    let arguments = header
        .iter()
        .enumerate()
        .filter(|(_, column)| column.starts_with("arg."))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let (kind, complete, resolved, source) = (
        column("row_kind"),
        column("item_complete"),
        column("arguments_resolved"),
        column("source"),
    );
    let (path, line) = (column("callsite_path"), column("callsite_line"));
    let rows = records
        .iter()
        .map(|record| csv_fields(record))
        .filter(|row| row.len() > 1)
        .collect::<Vec<_>>();
    let get = |row: &[String], index: Option<usize>| -> String {
        index
            .and_then(|index| row.get(index))
            .cloned()
            .unwrap_or_default()
    };
    let place = |row: &[String]| {
        let place = format!("{}:{}", get(row, path), get(row, line));
        match source {
            Some(_) => format!("{} {place}", get(row, source)),
            None => place,
        }
    };
    let uncertain_row = |row: &[String]| {
        let kind = get(row, kind);
        kind == "escape"
            || (kind == "call" && (row[item].starts_with('?') || get(row, complete) == "false"))
    };
    let uncertain = rows
        .iter()
        .filter(|row| uncertain_row(row))
        .map(|row| place(row))
        .collect::<std::collections::BTreeSet<_>>();
    // Each item's outcome overall and in each source.
    let mut items = BTreeMap::<String, (ItemOutcome, BTreeMap<String, ItemOutcome>)>::new();
    for row in &rows {
        let value = &row[item];
        let kind = get(row, kind);
        if value.is_empty() || kind == "excluded_call" {
            continue;
        }
        let at = place(row);
        let (entry, sources) = items.entry(value.clone()).or_default();
        let source_entry = sources.entry(get(row, source)).or_default();
        for outcome in [source_entry, entry] {
            if kind == "call" {
                outcome.called.insert(at.clone());
                outcome
                    .arguments
                    .resize_with(arguments.len(), Default::default);
                for (values, &index) in outcome.arguments.iter_mut().zip(&arguments) {
                    values.extend(
                        row[index]
                            .split(" | ")
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned),
                    );
                }
                outcome.unresolved |= get(row, resolved) == "false";
            } else if kind == "escape" || uncertain.contains(&at) {
                outcome.unknown.insert(at.clone());
            } else {
                outcome.not_called.insert(at.clone());
            }
        }
    }
    let item_column = &header[item];
    let mut out = String::new();
    let mut columns = vec![item_column.clone(), "outcome".to_owned()];
    columns.extend(arguments.iter().map(|&index| header[index].clone()));
    if source.is_some() {
        columns.push("sources".to_owned());
    }
    columns.extend(["called_at", "unknown_at", "not_called_at"].map(str::to_owned));
    write_record(&mut out, columns.iter().map(String::as_str));
    // Known items first, then unknown ones.
    let mut items = items.into_iter().collect::<Vec<_>>();
    items.sort_by_key(|(value, _)| value.starts_with('?'));
    for (value, (outcome, sources)) in &items {
        let unknown_item = value.starts_with('?');
        let joined = |places: &std::collections::BTreeSet<String>| {
            places.iter().cloned().collect::<Vec<_>>().join(" | ")
        };
        let mut fields = vec![value.clone(), outcome.outcome(unknown_item).to_owned()];
        fields.extend(
            (0..arguments.len())
                .map(|index| outcome.arguments.get(index).map(joined).unwrap_or_default()),
        );
        if source.is_some() {
            fields.push(
                sources
                    .iter()
                    .map(|(source, outcome)| format!("{source}: {}", outcome.outcome(unknown_item)))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
        }
        // An unknown item's calls are places it is unknown at.
        let (called, unknown) = if unknown_item {
            (
                String::new(),
                joined(&outcome.called.union(&outcome.unknown).cloned().collect()),
            )
        } else {
            (joined(&outcome.called), joined(&outcome.unknown))
        };
        fields.extend([called, unknown, joined(&outcome.not_called)]);
        write_record(&mut out, fields.iter().map(String::as_str));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{csv_fields, csv_records, item_outcomes};

    #[test]
    fn records_keep_line_breaks_inside_quoted_fields() {
        assert_eq!(
            csv_records("a,b\n1,\"two\nlines\"\n3,4\n"),
            ["a,b", "1,\"two\nlines\"", "3,4"]
        );
        assert_eq!(csv_fields("a,\"b, \"\"c\"\"\",d"), ["a", "b, \"c\"", "d"]);
    }

    #[test]
    fn items_are_called_not_called_or_unknown_across_every_callsite_and_source() {
        let csv = "source,row_kind,item.kind,item_complete,callsite_path,callsite_line,status,arg.action,arguments_resolved\n\
            hook,call,Kind.A,true,src/a.tsx,1,called,close,true\n\
            hook,no_call,Kind.B,true,src/a.tsx,1,called,,\n\
            hook,no_call,Kind.C,true,src/b.tsx,2,unused,,\n\
            hook,call,?computed,false,src/c.tsx,3,called,open,true\n\
            hook,no_call,Kind.D,false,src/c.tsx,3,called,,\n\
            hook,escape,Kind.E,true,src/d.tsx,4,escapes,,\n\
            hook,excluded_call,Kind.C,true,src/b.tsx,2,unused,later,true\n\
            direct,call,Kind.B,true,src/e.tsx,5,called,?unknown,false\n\
            direct,call,Kind.A,true,src/e.tsx,6,called,open,true\n";
        let rows = csv_records(&item_outcomes(csv))
            .into_iter()
            .map(csv_fields)
            .collect::<Vec<_>>();
        let row = |item: &str| rows.iter().find(|row| row[0] == item).unwrap();
        assert_eq!(
            rows[0],
            [
                "item.kind",
                "outcome",
                "arg.action",
                "sources",
                "called_at",
                "unknown_at",
                "not_called_at"
            ]
        );
        assert_eq!(
            row("Kind.A")[1..5],
            [
                "called",
                "close | open",
                "direct: called | hook: called",
                "direct src/e.tsx:6 | hook src/a.tsx:1"
            ]
        );
        assert_eq!(
            row("Kind.B")[1..4],
            [
                "called_with_unknown_arguments",
                "?unknown",
                "direct: called_with_unknown_arguments | hook: not_called"
            ]
        );
        assert_eq!(row("Kind.B")[6], "hook src/a.tsx:1");
        // An excluded call is no call.
        assert_eq!(row("Kind.C")[1], "not_called");
        // A call whose items are not all known could be for an item its callsite requests.
        assert_eq!(row("Kind.D")[1], "unknown");
        assert_eq!(row("Kind.D")[5], "hook src/c.tsx:3");
        assert_eq!(row("Kind.E")[1], "unknown");
        assert_eq!(
            row("?computed")[1..6],
            ["unknown", "open", "hook: unknown", "", "hook src/c.tsx:3"]
        );
        assert_eq!(rows.last().unwrap()[0], "?computed");
    }
}
