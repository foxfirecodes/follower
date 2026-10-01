use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use oxc_resolver::{ResolveOptions, Resolver};
use serde::{Deserialize, Serialize};

use crate::{
    cache::Snapshot,
    ids::FileId,
    ir::{FileIr, FlowExport, FlowPattern, FlowPatternKind, SourceSpan},
    models::SymbolMatcher,
    project::Project,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Resolved,
    Unresolved,
    TypeOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportResolution {
    pub importer: PathBuf,
    pub specifier: String,
    pub span: SourceSpan,
    pub status: ResolutionStatus,
    pub resolved_path: Option<PathBuf>,
    pub diagnostic: Option<String>,
}

/// Resolves import specifiers for one analysis run.
///
/// The resolver and canonical-path caches assume source files do not change while the linker is
/// alive, so create a new linker for each run.
pub struct ModuleLinker {
    resolver: Resolver,
    project_root: PathBuf,
    import_aliases: std::collections::BTreeMap<String, String>,
    canonical_paths: RefCell<HashMap<PathBuf, PathBuf>>,
}

impl ModuleLinker {
    pub fn new(project: &Project) -> Self {
        let options = ResolveOptions {
            extensions: vec![
                ".ts".into(),
                ".tsx".into(),
                ".mts".into(),
                ".cts".into(),
                ".js".into(),
                ".jsx".into(),
                ".mjs".into(),
                ".cjs".into(),
                ".json".into(),
            ],
            condition_names: project.config.resolution_conditions.clone(),
            ..ResolveOptions::default()
        };
        Self {
            resolver: Resolver::new(options),
            project_root: project.root.clone(),
            import_aliases: project.config.import_aliases.clone(),
            canonical_paths: RefCell::new(HashMap::new()),
        }
    }

    fn canonical_path(&self, path: PathBuf) -> PathBuf {
        if let Some(canonical) = self.canonical_paths.borrow().get(&path) {
            return canonical.clone();
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        self.canonical_paths
            .borrow_mut()
            .insert(path, canonical.clone());
        canonical
    }

    fn aliased_specifier(&self, specifier: &str) -> Option<String> {
        let mut matches = self
            .import_aliases
            .iter()
            .filter_map(|(alias, target)| {
                let captured = if let Some((prefix, suffix)) = alias.split_once('*') {
                    specifier.strip_prefix(prefix)?.strip_suffix(suffix)?
                } else if alias == specifier {
                    ""
                } else {
                    return None;
                };
                let mapped = target.replace('*', captured);
                Some((
                    alias.len(),
                    self.project_root
                        .join(mapped)
                        .to_string_lossy()
                        .into_owned(),
                ))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.cmp(&left.0));
        matches.into_iter().next().map(|(_, path)| path)
    }

    pub fn resolve(
        &self,
        importer: &Path,
        specifier: &str,
        span: SourceSpan,
        type_only: bool,
    ) -> ImportResolution {
        if type_only {
            return ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::TypeOnly,
                resolved_path: None,
                diagnostic: None,
            };
        }
        let aliased = self.aliased_specifier(specifier);
        let resolved_specifier = aliased.as_deref().unwrap_or(specifier);
        match self.resolver.resolve_file(importer, resolved_specifier) {
            Ok(resolution) => ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::Resolved,
                resolved_path: Some(self.canonical_path(resolution.into_path_buf())),
                diagnostic: None,
            },
            Err(error) => ImportResolution {
                importer: importer.to_path_buf(),
                specifier: specifier.to_owned(),
                span,
                status: ResolutionStatus::Unresolved,
                resolved_path: None,
                diagnostic: Some(error.to_string()),
            },
        }
    }
}

/// Resolves value-level imported bindings to their canonical exported identity.
///
/// This follows named aliases, local re-exports, explicit re-exports, and
/// `export *` chains. Namespace imports are resolved one member at a time.
pub struct SymbolLinker<'a> {
    project: &'a Project,
    snapshot: &'a Snapshot,
    file_by_id: BTreeMap<FileId, usize>,
    file_by_path: HashMap<PathBuf, usize>,
    resolutions_by_importer: HashMap<PathBuf, Vec<usize>>,
    /// Text of resolved modules outside the snapshot, read only to rule out exports.
    unparsed_sources: RefCell<HashMap<PathBuf, Option<UnparsedSource>>>,
    may_export: RefCell<HashMap<(PathBuf, String), bool>>,
}

struct UnparsedSource {
    text: String,
    /// The module may export names that do not appear in its text.
    forwards: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct LinkedSymbol {
    pub file_id: FileId,
    pub name: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LinkedValue {
    Declaration(LinkedSymbol),
    Namespace(FileId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueResolution {
    Resolved(LinkedValue),
    Missing,
    Ambiguous,
    Unresolved,
}

impl ValueResolution {
    fn symbol(self) -> Option<LinkedSymbol> {
        match self {
            Self::Resolved(LinkedValue::Declaration(symbol)) => Some(symbol),
            _ => None,
        }
    }
}

impl<'a> SymbolLinker<'a> {
    pub fn new(project: &'a Project, snapshot: &'a Snapshot) -> Self {
        let mut file_by_id = BTreeMap::new();
        let mut file_by_path = HashMap::new();
        for (index, file) in snapshot.files.iter().enumerate() {
            file_by_id.entry(file.file_id).or_insert(index);
            file_by_path.entry(file.path.clone()).or_insert(index);
        }
        let mut resolutions_by_importer = HashMap::<PathBuf, Vec<usize>>::new();
        for (index, resolution) in snapshot.resolutions.iter().enumerate() {
            resolutions_by_importer
                .entry(resolution.importer.clone())
                .or_default()
                .push(index);
        }
        Self {
            project,
            snapshot,
            file_by_id,
            file_by_path,
            resolutions_by_importer,
            unparsed_sources: RefCell::new(HashMap::new()),
            may_export: RefCell::new(HashMap::new()),
        }
    }

    /// Returns the first indexed file with this ID.
    pub fn file(&self, file_id: FileId) -> Option<&'a FileIr> {
        let snapshot = self.snapshot;
        self.file_by_id
            .get(&file_id)
            .and_then(|index| snapshot.files.get(*index))
    }

    /// Returns the first indexed file at this path.
    pub fn file_at(&self, path: &Path) -> Option<&'a FileIr> {
        let snapshot = self.snapshot;
        self.file_by_path
            .get(path)
            .and_then(|index| snapshot.files.get(*index))
    }

    /// Returns every import resolution recorded for `importer`, in snapshot order.
    pub fn import_resolutions(
        &self,
        importer: &Path,
    ) -> impl Iterator<Item = &'a ImportResolution> + '_ {
        let snapshot = self.snapshot;
        self.resolutions_by_importer
            .get(importer)
            .into_iter()
            .flatten()
            .filter_map(move |index| snapshot.resolutions.get(*index))
    }

    pub fn imported_binding_matches(
        &self,
        file_id: FileId,
        local: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_imported_binding(file_id, local)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn namespace_member_matches(
        &self,
        file_id: FileId,
        local: &str,
        member: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_namespace_member(file_id, local, member)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn local_declaration_matches(
        &self,
        file_id: FileId,
        name: &str,
        matcher: &SymbolMatcher,
    ) -> bool {
        self.resolve_local_declaration(file_id, name)
            .zip(self.resolve_matcher(matcher))
            .is_some_and(|(candidate, expected)| candidate == expected)
    }

    pub fn resolve_imported_binding(&self, file_id: FileId, local: &str) -> Option<LinkedSymbol> {
        let file = self.file(file_id)?;
        file.flow
            .imports
            .iter()
            .find(|import| import.local == local)?;
        self.resolve_binding(file_id, local).symbol()
    }

    pub fn resolve_namespace_member(
        &self,
        file_id: FileId,
        local: &str,
        member: &str,
    ) -> Option<LinkedSymbol> {
        let ValueResolution::Resolved(LinkedValue::Namespace(module)) =
            self.resolve_binding(file_id, local)
        else {
            return None;
        };
        self.resolve_exported_value(module, member).symbol()
    }

    pub fn resolve_local_declaration(&self, file_id: FileId, name: &str) -> Option<LinkedSymbol> {
        let file = self.file(file_id)?;
        let declared = file
            .flow
            .functions
            .iter()
            .any(|function| function.name == name)
            || file
                .flow
                .globals
                .iter()
                .any(|binding| pattern_names(&binding.pattern).contains(&name));
        declared.then(|| LinkedSymbol {
            file_id,
            name: name.to_owned(),
        })
    }

    pub fn resolve_matcher(&self, matcher: &SymbolMatcher) -> Option<LinkedSymbol> {
        let path = self
            .project
            .resolve_path(Path::new(&matcher.module))
            .canonicalize()
            .ok()?;
        self.resolve_export(&path, &matcher.export, &mut LinkWalk::default())
            .symbol()
    }

    pub fn resolve_binding(&self, file_id: FileId, local: &str) -> ValueResolution {
        let Some(file) = self.file(file_id) else {
            return ValueResolution::Unresolved;
        };
        self.resolve_local_binding(file, local, &mut LinkWalk::default())
    }

    /// Returns resolved modules outside the snapshot that linking `local` in `file_id` reached.
    /// Parsing them is what an unresolved binding needs; the set is empty when the binding links
    /// or fails for another reason.
    pub fn unparsed_link_targets(&self, file_id: FileId, local: &str) -> BTreeSet<PathBuf> {
        let Some(file) = self.file(file_id) else {
            return BTreeSet::new();
        };
        let mut walk = LinkWalk::default();
        if self.resolve_local_binding(file, local, &mut walk) == ValueResolution::Unresolved {
            walk.unparsed
        } else {
            BTreeSet::new()
        }
    }

    /// Returns the module specifiers and export names, as written at each hop, that linking `local`
    /// in `file_id` passes through: the import itself, then re-exports on branches that may provide
    /// the binding. A contract on any of them applies to the binding.
    pub fn linked_exports(&self, file_id: FileId, local: &str) -> Vec<(String, String)> {
        let Some(file) = self.file(file_id) else {
            return Vec::new();
        };
        let mut walk = LinkWalk::default();
        self.resolve_local_binding(file, local, &mut walk);
        walk.exports
    }

    pub fn resolve_exported_value(&self, file_id: FileId, name: &str) -> ValueResolution {
        let Some(file) = self.file(file_id) else {
            return ValueResolution::Unresolved;
        };
        self.resolve_export(&file.path, name, &mut LinkWalk::default())
    }

    fn resolve_local_binding(
        &self,
        file: &FileIr,
        local: &str,
        walk: &mut LinkWalk,
    ) -> ValueResolution {
        if let Some(import) = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local)
        {
            if import.type_only {
                return ValueResolution::Missing;
            }
            walk.exports
                .push((import.module.clone(), import.imported.clone()));
            if import.imported == "*" {
                return self.resolve_namespace(file, &import.module, walk);
            }
            return self.resolve_re_export(file, &import.module, &import.imported, walk);
        }
        self.resolve_local_declaration(file.file_id, local)
            .map_or(ValueResolution::Missing, |symbol| {
                ValueResolution::Resolved(LinkedValue::Declaration(symbol))
            })
    }

    fn resolve_namespace(
        &self,
        file: &FileIr,
        module: &str,
        walk: &mut LinkWalk,
    ) -> ValueResolution {
        let Some(path) = self.resolved_module(file, module) else {
            return ValueResolution::Unresolved;
        };
        self.file_at(path).map_or_else(
            || {
                walk.unparsed.insert(path.to_path_buf());
                ValueResolution::Unresolved
            },
            |file| ValueResolution::Resolved(LinkedValue::Namespace(file.file_id)),
        )
    }

    fn resolve_export(
        &self,
        module_path: &Path,
        export_name: &str,
        walk: &mut LinkWalk,
    ) -> ValueResolution {
        if !walk
            .visited
            .insert((module_path.to_path_buf(), export_name.to_owned()))
        {
            return ValueResolution::Missing;
        }
        let Some(file) = self.file_at(module_path) else {
            walk.visited
                .remove(&(module_path.to_path_buf(), export_name.to_owned()));
            if !self.may_export(module_path, export_name) {
                return ValueResolution::Missing;
            }
            walk.unparsed.insert(module_path.to_path_buf());
            return ValueResolution::Unresolved;
        };
        let result = self.resolve_export_paths(file, export_name, walk);
        // The recursion stack is path-local so diamond paths stay independent.
        walk.visited
            .remove(&(module_path.to_path_buf(), export_name.to_owned()));
        result
    }

    fn resolve_export_paths(
        &self,
        file: &FileIr,
        export_name: &str,
        walk: &mut LinkWalk,
    ) -> ValueResolution {
        // Explicit exports take precedence regardless of their source order.
        let explicit = file
            .flow
            .exports
            .iter()
            .filter(|export| match export {
                FlowExport::Local {
                    exported,
                    type_only: false,
                    ..
                }
                | FlowExport::ReExport {
                    exported,
                    type_only: false,
                    ..
                }
                | FlowExport::Namespace {
                    exported,
                    type_only: false,
                    ..
                } => exported == export_name,
                _ => false,
            })
            .collect::<Vec<_>>();
        let paths = if explicit.is_empty() {
            file.flow
                .exports
                .iter()
                .filter(|export| {
                    export_name != "default"
                        && matches!(
                            export,
                            FlowExport::Star {
                                type_only: false,
                                ..
                            }
                        )
                })
                .collect::<Vec<_>>()
        } else {
            explicit
        };
        let mut candidates = BTreeSet::new();
        let mut unresolved = false;
        let mut ambiguous = false;
        for export in paths {
            let hops = walk.exports.len();
            let resolution = match export {
                FlowExport::ReExport {
                    imported,
                    exported,
                    module,
                    type_only: false,
                    ..
                } if exported == export_name => {
                    walk.exports.push((module.clone(), imported.clone()));
                    self.resolve_re_export(file, module, imported, walk)
                }
                FlowExport::Local {
                    local,
                    exported,
                    type_only: false,
                    ..
                } if exported == export_name => self.resolve_local_binding(file, local, walk),
                FlowExport::Star {
                    module,
                    type_only: false,
                    ..
                } => {
                    walk.exports.push((module.clone(), export_name.to_owned()));
                    self.resolve_re_export(file, module, export_name, walk)
                }
                FlowExport::Namespace {
                    module,
                    type_only: false,
                    ..
                } => self.resolve_namespace(file, module, walk),
                FlowExport::Local { .. }
                | FlowExport::ReExport { .. }
                | FlowExport::Star { .. }
                | FlowExport::Namespace { .. } => ValueResolution::Missing,
            };
            match resolution {
                ValueResolution::Resolved(value) => {
                    candidates.insert(value);
                }
                ValueResolution::Ambiguous => ambiguous = true,
                ValueResolution::Unresolved => unresolved = true,
                // A branch that cannot provide the name did not carry the binding.
                ValueResolution::Missing => walk.exports.truncate(hops),
            }
        }
        if ambiguous || candidates.len() > 1 {
            ValueResolution::Ambiguous
        } else if unresolved {
            ValueResolution::Unresolved
        } else {
            candidates
                .into_iter()
                .next()
                .map_or(ValueResolution::Missing, ValueResolution::Resolved)
        }
    }

    fn resolve_re_export(
        &self,
        importer: &FileIr,
        module: &str,
        export_name: &str,
        walk: &mut LinkWalk,
    ) -> ValueResolution {
        self.resolved_module(importer, module)
            .map_or(ValueResolution::Unresolved, |resolved| {
                self.resolve_export(resolved, export_name, walk)
            })
    }

    /// Whether a module outside the snapshot may export `name`. An ES module can only export a
    /// name its text spells out, unless it forwards another module's exports, so a module whose
    /// text lacks the name and has no forwarding syntax cannot export it. Unreadable modules and
    /// non-script files such as stylesheets, whose exports a bundler supplies, may export
    /// anything.
    fn may_export(&self, path: &Path, name: &str) -> bool {
        if !crate::project::is_source_file(path) {
            return true;
        }
        let key = (path.to_path_buf(), name.to_owned());
        if let Some(known) = self.may_export.borrow().get(&key) {
            return *known;
        }
        let mut sources = self.unparsed_sources.borrow_mut();
        let source = sources.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(path)
                .ok()
                .map(|text| UnparsedSource {
                    forwards: forwards_exports(&text),
                    text,
                })
        });
        let may = source
            .as_ref()
            .is_none_or(|source| source.forwards || contains_identifier(&source.text, name));
        self.may_export.borrow_mut().insert(key, may);
        may
    }

    fn resolved_module(&self, importer: &FileIr, module: &str) -> Option<&'a Path> {
        // A type-only import of the same specifier records no path, so use the first value record.
        self.import_resolutions(&importer.path)
            .filter(|resolution| resolution.specifier == module)
            .find_map(|resolution| resolution.resolved_path.as_deref())
    }
}

/// Whether a module may export names its text does not spell out: through `export *` or TS
/// `export =`, identifier escapes, or `CommonJS` `exports` used other than as `exports.name` or
/// `Object.defineProperty(exports, "name", ...)`. Prose such as comments that mention exports is
/// ignored when another word follows on the same line.
fn forwards_exports(text: &str) -> bool {
    let after =
        |index: usize, word: &str| text[index + word.len()..].trim_start_matches([' ', '\t']);
    identifier_positions(text, "export").any(|index| after(index, "export").starts_with(['*', '=']))
        || text.contains("\\u")
        || identifier_positions(text, "exports").any(|index| {
            let rest = after(index, "exports");
            let named = rest
                .strip_prefix('.')
                .is_some_and(|rest| rest.starts_with(is_identifier_start));
            let defined = rest.strip_prefix(',').is_some_and(|rest| {
                text[..index].trim_end().ends_with("defineProperty(")
                    && rest.trim_start().starts_with(['"', '\''])
            });
            !(named || defined || rest.starts_with(is_identifier_start))
        })
}

fn is_identifier_start(character: char) -> bool {
    character.is_alphabetic() || matches!(character, '_' | '$')
}

fn identifier_positions<'t>(text: &'t str, name: &'t str) -> impl Iterator<Item = usize> + 't {
    let is_part = |character: char| character.is_alphanumeric() || matches!(character, '_' | '$');
    text.match_indices(name)
        .map(|(index, _)| index)
        .filter(move |index| {
            !text[..*index].chars().next_back().is_some_and(is_part)
                && !text[index + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(is_part)
        })
}

fn contains_identifier(text: &str, name: &str) -> bool {
    identifier_positions(text, name).next().is_some()
}

/// State for one linkage walk: the recursion stack, resolved modules the snapshot lacks, and the
/// module specifiers and export names the walk passed through.
#[derive(Default)]
struct LinkWalk {
    visited: BTreeSet<(PathBuf, String)>,
    unparsed: BTreeSet<PathBuf>,
    exports: Vec<(String, String)>,
}

pub(crate) fn pattern_names(pattern: &FlowPattern) -> Vec<&str> {
    match &pattern.kind {
        FlowPatternKind::Identifier { name } => vec![name],
        FlowPatternKind::Object { fields, rest } => fields
            .iter()
            .flat_map(|field| pattern_names(&field.target))
            .chain(rest.iter().flat_map(|rest| pattern_names(rest)))
            .collect(),
        FlowPatternKind::Array { elements } => {
            elements.iter().flatten().flat_map(pattern_names).collect()
        }
        FlowPatternKind::Unsupported { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_identifier, forwards_exports};

    #[test]
    fn forwarding_syntax_can_export_unnamed_bindings() {
        for text in [
            "export * from './a';",
            "export*from'./a'",
            "export = Thing;",
            "__exportStar(require('./a'), exports);",
            "module.exports = require('./a');",
            "Object.keys(a).forEach(function (key) { exports[key] = a[key]; });",
            "const \\u0061 = 1;",
            "var e = exports\nfoo();",
        ] {
            assert!(forwards_exports(text), "{text}");
        }
    }

    #[test]
    fn literal_exports_do_not_forward() {
        for text in [
            "export const a = 1; export { b as c } from './b';",
            "var r = require('react'); exports.a = 1; module.exports.b = 2;",
            "Object.defineProperty(exports, \"__esModule\", { value: true });",
            "// This module exports helpers for the panel.",
        ] {
            assert!(!forwards_exports(text), "{text}");
        }
    }

    #[test]
    fn identifiers_match_whole_words() {
        assert!(contains_identifier("export { Panel };", "Panel"));
        assert!(contains_identifier("exports.Panel=1", "Panel"));
        assert!(!contains_identifier("export const PanelBody = 1;", "Panel"));
        assert!(!contains_identifier("export const SidePanel = 1;", "Panel"));
        assert!(!contains_identifier("export const $Panel = 1;", "Panel"));
    }
}
