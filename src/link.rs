use std::{
    collections::BTreeSet,
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

pub struct ModuleLinker {
    resolver: Resolver,
    project_root: PathBuf,
    import_aliases: std::collections::BTreeMap<String, String>,
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
        }
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
                resolved_path: Some({
                    let path = resolution.into_path_buf();
                    path.canonicalize().unwrap_or(path)
                }),
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
    pub const fn new(project: &'a Project, snapshot: &'a Snapshot) -> Self {
        Self { project, snapshot }
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
        self.resolve_export(&path, &matcher.export, &mut BTreeSet::new())
            .symbol()
    }

    pub fn resolve_binding(&self, file_id: FileId, local: &str) -> ValueResolution {
        let Some(file) = self.file(file_id) else {
            return ValueResolution::Unresolved;
        };
        self.resolve_local_binding(file, local, &mut BTreeSet::new())
    }

    pub fn resolve_exported_value(&self, file_id: FileId, name: &str) -> ValueResolution {
        let Some(file) = self.file(file_id) else {
            return ValueResolution::Unresolved;
        };
        self.resolve_export(&file.path, name, &mut BTreeSet::new())
    }

    fn resolve_local_binding(
        &self,
        file: &FileIr,
        local: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
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
            if import.imported == "*" {
                return self.resolve_namespace(file, &import.module);
            }
            return self.resolve_re_export(file, &import.module, &import.imported, visited);
        }
        self.resolve_local_declaration(file.file_id, local)
            .map_or(ValueResolution::Missing, |symbol| {
                ValueResolution::Resolved(LinkedValue::Declaration(symbol))
            })
    }

    fn resolve_namespace(&self, file: &FileIr, module: &str) -> ValueResolution {
        self.resolved_module(file, module)
            .and_then(|path| self.snapshot.files.iter().find(|file| file.path == path))
            .map_or(ValueResolution::Unresolved, |file| {
                ValueResolution::Resolved(LinkedValue::Namespace(file.file_id))
            })
    }

    fn resolve_export(
        &self,
        module_path: &Path,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> ValueResolution {
        if !visited.insert((module_path.to_path_buf(), export_name.to_owned())) {
            return ValueResolution::Missing;
        }
        let Some(file) = self
            .snapshot
            .files
            .iter()
            .find(|file| file.path == module_path)
        else {
            visited.remove(&(module_path.to_path_buf(), export_name.to_owned()));
            return ValueResolution::Unresolved;
        };
        let result = self.resolve_export_paths(file, export_name, visited);
        // The recursion stack is path-local so diamond paths stay independent.
        visited.remove(&(module_path.to_path_buf(), export_name.to_owned()));
        result
    }

    fn resolve_export_paths(
        &self,
        file: &FileIr,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
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
            let resolution = match export {
                FlowExport::ReExport {
                    imported,
                    exported,
                    module,
                    type_only: false,
                    ..
                } if exported == export_name => {
                    self.resolve_re_export(file, module, imported, visited)
                }
                FlowExport::Local {
                    local,
                    exported,
                    type_only: false,
                    ..
                } if exported == export_name => self.resolve_local_binding(file, local, visited),
                FlowExport::Star {
                    module,
                    type_only: false,
                    ..
                } => self.resolve_re_export(file, module, export_name, visited),
                FlowExport::Namespace {
                    module,
                    type_only: false,
                    ..
                } => self.resolve_namespace(file, module),
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
                ValueResolution::Missing => {}
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
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> ValueResolution {
        self.resolved_module(importer, module)
            .map_or(ValueResolution::Unresolved, |resolved| {
                self.resolve_export(resolved, export_name, visited)
            })
    }

    fn resolved_module<'b>(&'b self, importer: &FileIr, module: &str) -> Option<&'b Path> {
        self.snapshot
            .resolutions
            .iter()
            .find(|resolution| {
                resolution.importer == importer.path && resolution.specifier == module
            })
            .and_then(|resolution| resolution.resolved_path.as_deref())
    }

    fn file(&self, file_id: FileId) -> Option<&FileIr> {
        self.snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)
    }
}

pub(crate) fn pattern_names(pattern: &FlowPattern) -> Vec<&str> {
    match &pattern.kind {
        FlowPatternKind::Identifier { name } => vec![name],
        FlowPatternKind::Object { fields } => fields
            .iter()
            .flat_map(|field| pattern_names(&field.target))
            .collect(),
        FlowPatternKind::Array { elements } => {
            elements.iter().flatten().flat_map(pattern_names).collect()
        }
        FlowPatternKind::Unsupported { .. } => Vec::new(),
    }
}
