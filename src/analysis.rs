use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    cache::{FRONTEND_VERSION, SNAPSHOT_SCHEMA_VERSION, Snapshot},
    frontend::parse_and_lower,
    ids::FileId,
    ir::{FileIr, FlowExport, FlowExpressionKind, SourceSpan, lazy_component_import},
    link::{LinkedValue, ModuleLinker, SymbolLinker, ValueResolution, pattern_names},
    models::{Model, load_model_files},
    project::{Project, is_source_file},
    queries::AuditReport,
    query::{
        QueryCallsite, QueryCallsiteInventory, QueryCallsiteStatus, QueryLocation, QueryReport,
        QuerySpec,
    },
    solver::{flow_expression_names, flow_statement_names},
};

pub struct Analyzer {
    project: Project,
}

#[derive(Default)]
struct FileUseIndex {
    /// A reference to a local name can be owned by one or more functions or module bindings.
    parents: BTreeMap<String, BTreeSet<String>>,
    exports: BTreeMap<String, BTreeSet<String>>,
    has_star_export: bool,
}

impl FileUseIndex {
    fn new(file: &FileIr) -> Self {
        let mut index = Self::default();
        for function in &file.flow.functions {
            for name in flow_statement_names(&function.body) {
                index
                    .parents
                    .entry(name)
                    .or_default()
                    .insert(function.name.clone());
            }
        }
        for binding in &file.flow.globals {
            let owners = pattern_names(&binding.pattern)
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            for name in flow_expression_names(&binding.value) {
                index
                    .parents
                    .entry(name)
                    .or_default()
                    .extend(owners.iter().cloned());
            }
        }
        for export in &file.flow.exports {
            match export {
                FlowExport::Local {
                    local,
                    exported,
                    type_only: false,
                    ..
                } => {
                    index
                        .exports
                        .entry(local.clone())
                        .or_default()
                        .insert(exported.clone());
                }
                FlowExport::ReExport {
                    exported,
                    type_only: false,
                    ..
                }
                | FlowExport::Namespace {
                    exported,
                    type_only: false,
                    ..
                } => {
                    index
                        .exports
                        .entry(exported.clone())
                        .or_default()
                        .insert(exported.clone());
                }
                FlowExport::Star {
                    type_only: false, ..
                } => index.has_star_export = true,
                _ => {}
            }
        }
        index
    }

    fn exported_names(&self, local: &str) -> BTreeSet<String> {
        let mut names = self.exports.get(local).cloned().unwrap_or_default();
        if self.has_star_export && local != "default" {
            names.insert(local.to_owned());
        }
        names
    }
}

#[derive(Default)]
struct BackwardUseWalk {
    added_files: usize,
    reached_entry: bool,
    limit_hit: bool,
    skipped_files: usize,
}

enum BackwardTask {
    Symbol {
        file_id: FileId,
        symbol: String,
        depth: usize,
    },
    Importer {
        child: PathBuf,
        exported: BTreeSet<String>,
        path: PathBuf,
        depth: usize,
    },
}

fn enqueue_backward_task(
    pending: &mut BTreeMap<(usize, usize, usize), BackwardTask>,
    sequence: &mut usize,
    corridor: &BTreeMap<PathBuf, usize>,
    path: &Path,
    depth: usize,
    task: BackwardTask,
) {
    if let Some(distance) = corridor.get(path) {
        pending.insert((*distance, depth, *sequence), task);
        *sequence += 1;
    }
}

struct SourceCatalog {
    imports_by_stem: BTreeMap<String, Vec<(PathBuf, String)>>,
    configured_files: usize,
    candidate_paths: BTreeSet<PathBuf>,
    skipped_candidate_files: usize,
    inventory_round_limit_hit: bool,
}

impl SourceCatalog {
    const fn new() -> Self {
        Self {
            imports_by_stem: BTreeMap::new(),
            configured_files: 0,
            candidate_paths: BTreeSet::new(),
            skipped_candidate_files: 0,
            inventory_round_limit_hit: false,
        }
    }

    /// Records one configured source; files under `node_modules` are not catalogued.
    fn add_source(&mut self, project: &Project, query: &QuerySpec, path: &Path, source: &str) {
        if path
            .components()
            .any(|part| part.as_os_str() == OsStr::new("node_modules"))
        {
            return;
        }
        self.configured_files += 1;
        if source.contains(&query.factory.export) {
            self.candidate_paths.insert(path.to_path_buf());
        }
        let may_use_configured_lazy_factory = project
            .config
            .lazy_component_factories
            .iter()
            .any(|factory| source.contains(&factory.module) && source.contains(&factory.export));
        let specifiers = module_specifiers(source, may_use_configured_lazy_factory);
        for specifier in specifiers {
            if let Some(stem) = Path::new(&specifier)
                .file_stem()
                .and_then(|name| name.to_str())
            {
                self.imports_by_stem
                    .entry(stem.to_owned())
                    .or_default()
                    .push((path.to_path_buf(), specifier));
            }
        }
    }

    fn follow_factory_reexports(
        &mut self,
        project: &Project,
        query: &QuerySpec,
        linker: &ModuleLinker,
    ) -> Result<()> {
        let factory_path = project
            .resolve_path(Path::new(&query.factory.module))
            .canonicalize()?;
        let mut targets = BTreeSet::from([factory_path]);
        let mut visited = BTreeSet::new();
        let mut skipped = BTreeSet::new();
        for round in 0..8 {
            let importers = self.reverse_importers(linker, &targets);
            skipped.extend(importers.difference(&visited).skip(256).cloned());
            let mut next = BTreeSet::new();
            let pending = importers
                .difference(&visited)
                .take(256)
                .cloned()
                .collect::<Vec<_>>();
            for path in pending {
                visited.insert(path.clone());
                self.candidate_paths.insert(path.clone());
                let source = fs::read_to_string(&path)?;
                if !source.contains("export") {
                    continue;
                }
                let file = parse_and_lower(FileId(u32::MAX), &path, &source)?;
                let forwards = file.flow.exports.iter().any(|export| {
                    let module = match export {
                        FlowExport::ReExport { module, .. }
                        | FlowExport::Star { module, .. }
                        | FlowExport::Namespace { module, .. } => Some(module.as_str()),
                        FlowExport::Local { local, .. } => file
                            .flow
                            .imports
                            .iter()
                            .find(|import| import.local == *local)
                            .map(|import| import.module.as_str()),
                    };
                    module.is_some_and(|module| {
                        linker
                            .resolve(
                                &path,
                                module,
                                SourceSpan {
                                    file_id: FileId(0),
                                    start: 0,
                                    end: 0,
                                },
                                false,
                            )
                            .resolved_path
                            .as_ref()
                            .is_some_and(|target| targets.contains(target))
                    })
                });
                if forwards {
                    next.insert(path.clone());
                }
            }
            if next.is_empty() {
                break;
            }
            targets.extend(next);
            if round == 7 {
                self.inventory_round_limit_hit = true;
            }
        }
        self.skipped_candidate_files = skipped.len();
        Ok(())
    }

    fn reverse_importers(
        &self,
        linker: &ModuleLinker,
        targets: &BTreeSet<PathBuf>,
    ) -> BTreeSet<PathBuf> {
        let names = targets
            .iter()
            .flat_map(|path| {
                let mut names = Vec::new();
                if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                    names.push(stem.to_owned());
                    if stem == "index" {
                        if let Some(parent) = path
                            .parent()
                            .and_then(Path::file_name)
                            .and_then(|name| name.to_str())
                        {
                            names.push(parent.to_owned());
                        }
                    }
                }
                names
            })
            .collect::<BTreeSet<_>>();
        let mut result = BTreeSet::new();
        for name in names {
            for (importer, specifier) in self.imports_by_stem.get(&name).into_iter().flatten() {
                if targets.contains(importer) {
                    continue;
                }
                let resolution = linker.resolve(
                    importer,
                    specifier,
                    SourceSpan {
                        file_id: FileId(0),
                        start: 0,
                        end: 0,
                    },
                    false,
                );
                if resolution
                    .resolved_path
                    .as_ref()
                    .is_some_and(|path| targets.contains(path))
                {
                    result.insert(importer.clone());
                }
            }
        }
        result
    }

    /// Find the cheap import-graph corridor from creation files toward configured roots.
    /// Semantic uses are checked only after this has discarded unrelated importers.
    fn entry_corridor(
        &self,
        project: &Project,
        linker: &ModuleLinker,
        seeds: &BTreeSet<PathBuf>,
    ) -> (BTreeMap<PathBuf, usize>, bool) {
        let entries = project
            .config
            .entries
            .iter()
            .filter_map(|entry| project.resolve_path(&entry.module).canonicalize().ok())
            .collect::<BTreeSet<_>>();
        let mut visited = seeds.clone();
        let mut frontier = seeds.clone();
        let mut children: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
        let mut hit_limit = false;
        for _ in 0..32 {
            if frontier.is_empty() {
                break;
            }
            let mut next = BTreeSet::new();
            for child in &frontier {
                for importer in self.reverse_importers(linker, &BTreeSet::from([child.clone()])) {
                    children
                        .entry(importer.clone())
                        .or_default()
                        .insert(child.clone());
                    if !visited.contains(&importer) {
                        if visited.len() >= 16_384 {
                            hit_limit = true;
                            break;
                        }
                        visited.insert(importer.clone());
                        next.insert(importer);
                    }
                }
                if hit_limit {
                    break;
                }
            }
            if hit_limit {
                break;
            }
            frontier = next;
        }
        if !frontier.is_empty() {
            hit_limit = true;
        }
        let mut corridor = BTreeMap::new();
        let mut pending = entries
            .intersection(&visited)
            .cloned()
            .map(|path| (path, 0_usize))
            .collect::<VecDeque<_>>();
        while let Some((path, distance)) = pending.pop_front() {
            if corridor.contains_key(&path) {
                continue;
            }
            corridor.insert(path.clone(), distance);
            pending.extend(
                children
                    .get(&path)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .map(|child| (child, distance + 1)),
            );
        }
        (corridor, hit_limit)
    }
}

/// Budget for files that configured roots request after discovery stops.
const ROOT_PHASE_FILES: usize = 128;
const ROOT_PHASE_ROUNDS: usize = 6;

struct RootPhase {
    files: usize,
    rounds: usize,
    discovery_stop: Option<String>,
}

fn is_expandable_source(path: &Path) -> bool {
    path.is_file()
        && is_source_file(path)
        && !path
            .components()
            .any(|component| component.as_os_str() == OsStr::new("node_modules"))
}

fn module_specifiers(source: &str, include_dynamic_imports: bool) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut specs = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        let keyword = [
            b"from".as_slice(),
            b"import".as_slice(),
            b"require".as_slice(),
        ]
        .into_iter()
        .find(|keyword| {
            bytes[index..].starts_with(keyword)
                && (index == 0
                    || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_')
                && bytes
                    .get(index + keyword.len())
                    .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
        });
        let Some(keyword) = keyword else {
            index += 1;
            continue;
        };
        let mut next = index + keyword.len();
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if keyword == b"import" && bytes.get(next) == Some(&b'(') && !include_dynamic_imports {
            index += keyword.len();
            continue;
        }
        if (keyword == b"require" || keyword == b"import") && bytes.get(next) == Some(&b'(') {
            next += 1;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
        }
        if let Some(quote @ (b'\'' | b'"')) = bytes.get(next).copied() {
            let start = next + 1;
            next = start;
            while next < bytes.len() && bytes[next] != quote {
                next += 1;
            }
            if next < bytes.len() {
                specs.insert(source[start..next].to_owned());
            }
        }
        index += keyword.len();
    }
    specs.into_iter().collect()
}

impl Analyzer {
    pub const fn new(project: Project) -> Self {
        Self { project }
    }

    pub const fn project(&self) -> &Project {
        &self.project
    }

    pub fn index(&self) -> Result<Snapshot> {
        self.index_with(&ModuleLinker::new(&self.project))
    }

    fn index_with(&self, linker: &ModuleLinker) -> Result<Snapshot> {
        let source_paths = self.project.discover_sources()?;
        self.snapshot_from_sources(
            linker,
            source_paths.into_iter().map(|path| {
                let source = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                Ok((path, source))
            }),
        )
    }

    /// Indexes the text-filtered sources and catalogs every configured source in one read pass.
    fn index_with_catalog(
        &self,
        linker: &ModuleLinker,
        query: &QuerySpec,
    ) -> Result<(Snapshot, SourceCatalog)> {
        let phase_start = Instant::now();
        let mut catalog = SourceCatalog::new();
        let mut indexed = Vec::new();
        for candidate in self.project.discover_source_candidates()? {
            let source = fs::read_to_string(&candidate.path)
                .with_context(|| format!("failed to read {}", candidate.path.display()))?;
            catalog.add_source(&self.project, query, &candidate.path, &source);
            if !candidate.text_filtered || self.project.matches_source_filter(&source) {
                indexed.push((candidate.path, source));
            }
        }
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query source scan: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
        let phase_start = Instant::now();
        let snapshot = self.snapshot_from_sources(linker, indexed.into_iter().map(Ok))?;
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query initial index: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
        let phase_start = Instant::now();
        catalog.follow_factory_reexports(&self.project, query, linker)?;
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query source catalog reexports: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
        Ok((snapshot, catalog))
    }

    fn snapshot_from_sources(
        &self,
        linker: &ModuleLinker,
        sources: impl Iterator<Item = Result<(PathBuf, String)>>,
    ) -> Result<Snapshot> {
        let mut files = Vec::new();
        for (index, source) in sources.enumerate() {
            let (path, source) = source?;
            files.push(parse_and_lower(
                FileId(u32::try_from(index).unwrap_or(u32::MAX)),
                &path,
                &source,
            )?);
        }

        let mut resolutions = Vec::new();
        for file in &files {
            for import in &file.imports {
                resolutions.push(linker.resolve(
                    &file.path,
                    &import.specifier,
                    import.span.clone(),
                    import.type_only,
                ));
            }
        }

        let snapshot_id = self.snapshot_id(&files);

        Ok(Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            frontend_version: FRONTEND_VERSION.to_owned(),
            snapshot_id,
            project_name: self.project.config.name.clone(),
            config_hash: self.project.config_hash.clone(),
            files,
            resolutions,
        })
    }

    fn walk_backward_uses(
        &self,
        linker: &ModuleLinker,
        snapshot: &mut Snapshot,
        catalog: &SourceCatalog,
        corridor: &BTreeMap<PathBuf, usize>,
        callsites: &[SourceSpan],
        file_budget: usize,
    ) -> BackwardUseWalk {
        let mut result = BackwardUseWalk::default();
        let mut pending = BTreeMap::new();
        let mut sequence = 0;
        for callsite in callsites {
            let Some(file) = snapshot
                .files
                .iter()
                .find(|file| file.file_id == callsite.file_id)
            else {
                continue;
            };
            let owner = file
                .flow
                .functions
                .iter()
                .filter(|function| {
                    function.span.start <= callsite.start && callsite.end <= function.span.end
                })
                .min_by_key(|function| function.span.end - function.span.start)
                .map(|function| function.name.clone());
            if let Some(owner) = owner {
                enqueue_backward_task(
                    &mut pending,
                    &mut sequence,
                    corridor,
                    &file.path,
                    0,
                    BackwardTask::Symbol {
                        file_id: file.file_id,
                        symbol: owner,
                        depth: 0,
                    },
                );
            } else {
                for binding in &file.flow.globals {
                    if binding.value.span.start <= callsite.start
                        && callsite.end <= binding.value.span.end
                    {
                        for owner in pattern_names(&binding.pattern) {
                            enqueue_backward_task(
                                &mut pending,
                                &mut sequence,
                                corridor,
                                &file.path,
                                0,
                                BackwardTask::Symbol {
                                    file_id: file.file_id,
                                    symbol: owner.to_owned(),
                                    depth: 0,
                                },
                            );
                        }
                    }
                }
            }
        }
        let entries = self
            .project
            .config
            .entries
            .iter()
            .filter_map(|entry| {
                self.project
                    .resolve_path(&entry.module)
                    .canonicalize()
                    .ok()
                    .map(|path| (path, entry.export.clone()))
            })
            .collect::<BTreeSet<_>>();
        let mut visited = BTreeSet::new();
        let mut indexes: BTreeMap<FileId, FileUseIndex> = BTreeMap::new();
        let mut reverse_cache: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
        // Importer tasks check the same modules repeatedly as the snapshot
        // grows. Keep all resolved targets for a specifier to preserve the
        // previous `any` behavior if duplicate resolution records exist.
        let mut resolved_modules: BTreeMap<PathBuf, BTreeMap<String, BTreeSet<PathBuf>>> =
            BTreeMap::new();
        for resolution in &snapshot.resolutions {
            if let Some(target) = &resolution.resolved_path {
                resolved_modules
                    .entry(resolution.importer.clone())
                    .or_default()
                    .entry(resolution.specifier.clone())
                    .or_default()
                    .insert(target.clone());
            }
        }
        while let Some((_, task)) = pending.pop_first() {
            match task {
                BackwardTask::Symbol {
                    file_id,
                    symbol,
                    depth,
                } => {
                    if !visited.insert((file_id, symbol.clone())) {
                        continue;
                    }
                    if visited.len() > 20_000 || depth > 32 {
                        result.limit_hit = true;
                        break;
                    }
                    let Some(file) = snapshot.files.iter().find(|file| file.file_id == file_id)
                    else {
                        continue;
                    };
                    let path = file.path.clone();
                    let index = indexes
                        .entry(file_id)
                        .or_insert_with(|| FileUseIndex::new(file));
                    let parents = index.parents.get(&symbol).cloned().unwrap_or_default();
                    let exported = index.exported_names(&symbol);
                    if entries.iter().any(|(entry_path, entry_export)| {
                        *entry_path == path && exported.contains(entry_export)
                    }) {
                        result.reached_entry = true;
                        continue;
                    }
                    for parent in parents {
                        enqueue_backward_task(
                            &mut pending,
                            &mut sequence,
                            corridor,
                            &path,
                            depth + 1,
                            BackwardTask::Symbol {
                                file_id,
                                symbol: parent,
                                depth: depth + 1,
                            },
                        );
                    }
                    if exported.is_empty() {
                        continue;
                    }
                    let importers = reverse_cache
                        .entry(path.clone())
                        .or_insert_with(|| {
                            catalog.reverse_importers(linker, &BTreeSet::from([path.clone()]))
                        })
                        .clone();
                    for importer_path in importers {
                        enqueue_backward_task(
                            &mut pending,
                            &mut sequence,
                            corridor,
                            &importer_path,
                            depth + 1,
                            BackwardTask::Importer {
                                child: path.clone(),
                                exported: exported.clone(),
                                path: importer_path.clone(),
                                depth: depth + 1,
                            },
                        );
                    }
                }
                BackwardTask::Importer {
                    child,
                    exported,
                    path,
                    depth,
                } => {
                    if !snapshot.files.iter().any(|file| file.path == path) {
                        if result.added_files >= file_budget {
                            result.limit_hit = true;
                            break;
                        }
                        let old_resolutions = snapshot.resolutions.len();
                        if self
                            .extend_snapshot(linker, snapshot, std::slice::from_ref(&path))
                            .is_err()
                        {
                            result.skipped_files += 1;
                            continue;
                        }
                        for resolution in snapshot.resolutions.iter().skip(old_resolutions) {
                            if let Some(target) = &resolution.resolved_path {
                                resolved_modules
                                    .entry(resolution.importer.clone())
                                    .or_default()
                                    .entry(resolution.specifier.clone())
                                    .or_default()
                                    .insert(target.clone());
                            }
                        }
                        result.added_files += 1;
                    }
                    let Some(importer) = snapshot.files.iter().find(|file| file.path == path)
                    else {
                        continue;
                    };
                    let importer_id = importer.file_id;
                    let importer_index = indexes
                        .entry(importer_id)
                        .or_insert_with(|| FileUseIndex::new(importer));
                    let matches_target = |module: &str| {
                        resolved_modules
                            .get(&path)
                            .and_then(|modules| modules.get(module))
                            .is_some_and(|targets| targets.contains(&child))
                    };
                    for import in &importer.flow.imports {
                        if import.type_only
                            || !matches_target(&import.module)
                            || (import.imported != "*" && !exported.contains(&import.imported))
                        {
                            continue;
                        }
                        if importer_index.parents.contains_key(&import.local)
                            || !importer_index.exported_names(&import.local).is_empty()
                        {
                            enqueue_backward_task(
                                &mut pending,
                                &mut sequence,
                                corridor,
                                &path,
                                depth,
                                BackwardTask::Symbol {
                                    file_id: importer_id,
                                    symbol: import.local.clone(),
                                    depth,
                                },
                            );
                        }
                    }
                    if exported.contains("default") {
                        for binding in &importer.flow.globals {
                            let FlowExpressionKind::Call { callee, .. } = &binding.value.kind
                            else {
                                continue;
                            };
                            let Some(property) = self
                                .project
                                .config
                                .lazy_factory_property(&importer.flow, callee)
                            else {
                                continue;
                            };
                            let Some(module) = lazy_component_import(&binding.value, property)
                            else {
                                continue;
                            };
                            if !matches_target(module) {
                                continue;
                            }
                            for owner in pattern_names(&binding.pattern) {
                                enqueue_backward_task(
                                    &mut pending,
                                    &mut sequence,
                                    corridor,
                                    &path,
                                    depth,
                                    BackwardTask::Symbol {
                                        file_id: importer_id,
                                        symbol: owner.to_owned(),
                                        depth,
                                    },
                                );
                            }
                        }
                    }
                    for export in &importer.flow.exports {
                        let forwarded = match export {
                            FlowExport::ReExport {
                                imported,
                                exported: forwarded,
                                module,
                                type_only: false,
                                ..
                            } if matches_target(module) && exported.contains(imported) => {
                                vec![forwarded.clone()]
                            }
                            FlowExport::Star {
                                module,
                                type_only: false,
                                ..
                            } if matches_target(module) => exported
                                .iter()
                                .filter(|name| *name != "default")
                                .cloned()
                                .collect(),
                            _ => Vec::new(),
                        };
                        for symbol in forwarded {
                            enqueue_backward_task(
                                &mut pending,
                                &mut sequence,
                                corridor,
                                &path,
                                depth,
                                BackwardTask::Symbol {
                                    file_id: importer_id,
                                    symbol,
                                    depth,
                                },
                            );
                        }
                    }
                }
            }
        }
        result
    }

    pub fn default_snapshot_path(&self) -> std::path::PathBuf {
        self.project.root.join(Path::new(".flow/snapshot.json"))
    }

    pub fn audit(&self, model_id: &str) -> Result<AuditReport> {
        let snapshot = self.index()?;
        let models = load_model_files(&self.project.root, &self.project.config.model_files)?;
        let model = models
            .find(model_id)
            .with_context(|| format!("configured model not found: {model_id}"))?;
        match model {
            Model::CallbackFactory(factory) => {
                crate::solver::audit(&self.project, &snapshot, factory, &models.content_hash)
            }
        }
    }

    pub fn query(&self, query: &QuerySpec, query_hash: &str) -> Result<QueryReport> {
        query.validate()?;
        let linker = ModuleLinker::new(&self.project);
        let (mut snapshot, catalog) = if self.project.config.source_contains_any.is_empty() {
            let phase_start = Instant::now();
            let snapshot = self.index_with(&linker)?;
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query initial index: {} ms",
                    phase_start.elapsed().as_millis()
                );
            }
            (snapshot, None)
        } else {
            let (snapshot, catalog) = self.index_with_catalog(&linker, query)?;
            (snapshot, Some(catalog))
        };
        let mut followed = 0;
        let mut skipped = BTreeSet::new();
        let mut reverse_seed_paths = BTreeSet::new();
        let mut reverse_producer_paths = BTreeSet::new();
        let mut walked_callsites = BTreeSet::new();
        let mut backward_limit_hit = false;
        let mut backward_skipped_files = 0;
        let mut no_modeled_entry_use = false;
        let mut corridor_limit_hit = false;
        let mut entry_path_not_found = 0;
        let mut corridor_seeds = BTreeSet::new();
        let mut entry_corridor = BTreeMap::new();
        let mut run_roots = query.scope != crate::query::QueryScope::AllCreations
            || self.project.config.entries.is_empty()
            || self.project.config.source_contains_any.is_empty();
        if self.project.config.source_contains_any.is_empty() {
            let mut report = crate::solver::execute_query(
                &self.project,
                &snapshot,
                query,
                query_hash,
                &reverse_seed_paths,
                &reverse_producer_paths,
                &entry_corridor,
                run_roots,
            )?
            .report;
            self.attach_callsite_inventory(&linker, &mut report, &snapshot, query, None)?;
            report.finish_gaps();
            return Ok(report);
        }
        let catalog = catalog
            .as_ref()
            .context("filtered query has no source catalog")?;
        let mut root_phase: Option<RootPhase> = None;
        let mut next_round = 0;
        let mut report = loop {
            let round = next_round;
            next_round += 1;
            let crate::solver::QueryPass {
                mut report,
                requested_imports: requests,
                root_requested_imports,
                producer_paths: producers,
                reachable_seed_callsites,
            } = crate::solver::execute_query(
                &self.project,
                &snapshot,
                query,
                query_hash,
                &reverse_seed_paths,
                &reverse_producer_paths,
                &entry_corridor,
                run_roots,
            )?;
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query round {round}: indexed={} requested={} root_requested={} producers={} reverse_seeds={}",
                    snapshot.files.len(),
                    requests.len(),
                    root_requested_imports.len(),
                    producers.len(),
                    reverse_seed_paths.len()
                );
            }
            if let Some(phase) = &mut root_phase {
                // Follow only what root paths read, plus components on the entry corridor.
                let mut additions = Vec::new();
                for path in &root_requested_imports {
                    if snapshot.files.iter().any(|file| &file.path == path) {
                        continue;
                    }
                    if is_expandable_source(path) {
                        additions.push(path.clone());
                    } else {
                        skipped.insert(path.clone());
                    }
                }
                if additions.is_empty()
                    || phase.rounds == ROOT_PHASE_ROUNDS
                    || phase.files + additions.len() > ROOT_PHASE_FILES
                {
                    report.coverage.gaps.extend(phase.discovery_stop.take());
                    if !additions.is_empty() {
                        report.coverage.gaps.push(format!(
                            "root path expansion stopped after {} files and {} rounds; {} requested files remain",
                            phase.files,
                            phase.rounds,
                            additions.len()
                        ));
                    }
                    break report;
                }
                phase.files += additions.len();
                phase.rounds += 1;
                self.extend_snapshot(&linker, &mut snapshot, &additions)?;
                continue;
            }
            reverse_producer_paths.extend(producers);
            let reverse_requests = catalog.reverse_importers(&linker, &reverse_producer_paths);
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query round {round}: reverse_candidates={}",
                    reverse_requests.len()
                );
            }
            let indexed = snapshot
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<BTreeSet<_>>();
            let mut additions = Vec::new();
            for path in requests
                .union(&reverse_requests)
                .filter(|path| !indexed.contains(*path))
            {
                if is_expandable_source(path) {
                    additions.push(path.clone());
                } else {
                    skipped.insert(path.clone());
                }
            }
            if additions.is_empty() && round < 8 && !self.project.config.entries.is_empty() {
                let callsites = report
                    .creations
                    .iter()
                    .map(|creation| creation.factory_callsite.clone())
                    .chain(reachable_seed_callsites)
                    .filter(|span| walked_callsites.insert((span.file_id, span.start, span.end)))
                    .collect::<Vec<_>>();
                if !callsites.is_empty() {
                    let new_paths = callsites
                        .iter()
                        .filter_map(|span| {
                            snapshot
                                .files
                                .iter()
                                .find(|file| file.file_id == span.file_id)
                                .map(|file| file.path.clone())
                        })
                        .filter(|path| corridor_seeds.insert(path.clone()))
                        .collect::<BTreeSet<_>>();
                    if !new_paths.is_empty() {
                        let phase_start = Instant::now();
                        let (paths, hit_limit) =
                            catalog.entry_corridor(&self.project, &linker, &new_paths);
                        entry_path_not_found += new_paths
                            .iter()
                            .filter(|path| !paths.contains_key(*path))
                            .count();
                        entry_corridor.extend(paths);
                        corridor_limit_hit |= hit_limit;
                        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                            eprintln!(
                                "query entry corridor: seeds={} paths={} limit_hit={}",
                                new_paths.len(),
                                entry_corridor.len(),
                                hit_limit
                            );
                            eprintln!(
                                "query entry corridor elapsed: {} ms",
                                phase_start.elapsed().as_millis()
                            );
                        }
                    }
                    let original_files = snapshot.files.len();
                    let original_resolutions = snapshot.resolutions.len();
                    let original_snapshot_id = snapshot.snapshot_id.clone();
                    let phase_start = Instant::now();
                    let walk = self.walk_backward_uses(
                        &linker,
                        &mut snapshot,
                        catalog,
                        &entry_corridor,
                        &callsites,
                        256_usize.saturating_sub(followed),
                    );
                    backward_limit_hit |= walk.limit_hit;
                    backward_skipped_files += walk.skipped_files;
                    no_modeled_entry_use |= !walk.reached_entry;
                    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                        eprintln!(
                            "query backward uses: parsed={} reached_entry={} limit_hit={}",
                            walk.added_files, walk.reached_entry, walk.limit_hit
                        );
                        eprintln!(
                            "query backward uses elapsed: {} ms",
                            phase_start.elapsed().as_millis()
                        );
                    }
                    if walk.reached_entry && walk.added_files > 0 {
                        followed += walk.added_files;
                        continue;
                    }
                    snapshot.files.truncate(original_files);
                    snapshot.resolutions.truncate(original_resolutions);
                    snapshot.snapshot_id = original_snapshot_id;
                }
            }
            if additions.is_empty() || round == 8 || followed + additions.len() > 256 {
                let discovery_stop = (!additions.is_empty()).then(|| {
                    format!(
                        "capability import expansion stopped after {followed} files and {round} rounds; {} requested files remain",
                        additions.len()
                    )
                });
                if run_roots {
                    report.coverage.gaps.extend(discovery_stop);
                    break report;
                }
                // Discovery has settled or reached its budget. Explore the configured roots on
                // this snapshot, then follow what those paths need under a separate budget.
                run_roots = true;
                if entry_corridor.is_empty() && !self.project.config.entries.is_empty() {
                    let seeds = report
                        .creations
                        .iter()
                        .filter_map(|creation| {
                            snapshot
                                .files
                                .iter()
                                .find(|file| file.file_id == creation.factory_callsite.file_id)
                                .map(|file| file.path.clone())
                        })
                        .collect::<BTreeSet<_>>();
                    let (paths, hit_limit) = catalog.entry_corridor(&self.project, &linker, &seeds);
                    entry_path_not_found += seeds
                        .iter()
                        .filter(|path| !paths.contains_key(*path))
                        .count();
                    entry_corridor.extend(paths);
                    corridor_limit_hit |= hit_limit;
                }
                root_phase = Some(RootPhase {
                    files: 0,
                    rounds: 0,
                    discovery_stop,
                });
                continue;
            }
            followed += additions.len();
            reverse_seed_paths.extend(
                additions
                    .iter()
                    .filter(|path| reverse_requests.contains(*path))
                    .cloned(),
            );
            let phase_start = Instant::now();
            self.extend_snapshot(&linker, &mut snapshot, &additions)?;
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query snapshot expansion: files={} elapsed={} ms",
                    additions.len(),
                    phase_start.elapsed().as_millis()
                );
            }
            for path in additions
                .iter()
                .filter(|path| reverse_requests.contains(*path))
            {
                let Some(file) = snapshot.files.iter().find(|file| &file.path == path) else {
                    continue;
                };
                let forwards = file.flow.exports.iter().any(|export| {
                    let module = match export {
                        FlowExport::ReExport { module, .. }
                        | FlowExport::Star { module, .. }
                        | FlowExport::Namespace { module, .. } => Some(module.as_str()),
                        FlowExport::Local { local, .. } => file
                            .flow
                            .imports
                            .iter()
                            .find(|import| import.local == *local)
                            .map(|import| import.module.as_str()),
                    };
                    module.is_some_and(|module| {
                        snapshot.resolutions.iter().any(|resolution| {
                            resolution.importer == file.path
                                && resolution.specifier == module
                                && resolution
                                    .resolved_path
                                    .as_ref()
                                    .is_some_and(|target| reverse_producer_paths.contains(target))
                        })
                    })
                });
                if forwards {
                    reverse_producer_paths.insert(path.clone());
                }
            }
        };
        if !skipped.is_empty() {
            report.coverage.gaps.push(format!(
                "{} capability imports were outside supported source files or node_modules",
                skipped.len()
            ));
        }
        if backward_limit_hit {
            report.coverage.gaps.push(
                "backward component use walk stopped at its file, depth, or symbol budget"
                    .to_owned(),
            );
        }
        if corridor_limit_hit {
            report.coverage.gaps.push(
                "backward import scan stopped at its graph or depth budget; entry paths may be missing"
                    .to_owned(),
            );
        }
        if entry_path_not_found > 0 {
            report.coverage.gaps.push(format!(
                "backward import scan found no modeled path to an entry for {entry_path_not_found} factory-host files"
            ));
        }
        if backward_skipped_files > 0 {
            report.coverage.gaps.push(format!(
                "backward component use walk could not parse {backward_skipped_files} candidate files"
            ));
        }
        if no_modeled_entry_use {
            report.coverage.gaps.push(
                "backward component use walk found no modeled use chain to an entry; unmodeled loaders or registries may still connect them"
                    .to_owned(),
            );
        }
        if !report.coverage.gaps.is_empty() {
            report.coverage.complete = false;
        }
        let phase_start = Instant::now();
        self.attach_callsite_inventory(&linker, &mut report, &snapshot, query, Some(catalog))?;
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query callsite inventory: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
        report.finish_gaps();
        Ok(report)
    }

    fn attach_callsite_inventory(
        &self,
        linker: &ModuleLinker,
        report: &mut QueryReport,
        snapshot: &Snapshot,
        query: &QuerySpec,
        catalog: Option<&SourceCatalog>,
    ) -> Result<()> {
        let indexed = snapshot
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<BTreeSet<_>>();
        let mut candidate_paths =
            catalog.map_or_else(BTreeSet::new, |catalog| catalog.candidate_paths.clone());
        candidate_paths.extend(indexed.iter().cloned());
        let analyzed = report
            .creations
            .iter()
            .filter_map(|creation| creation.factory_location.clone())
            .collect::<Vec<_>>();
        let mut callsites = Vec::new();
        let mut inventory_snapshot = snapshot.clone();
        for path in &candidate_paths {
            if indexed.contains(path) {
                continue;
            }
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let file_id = FileId(u32::try_from(inventory_snapshot.files.len())?);
            let file = match parse_and_lower(file_id, path, &source) {
                Ok(file) => file,
                Err(error) => {
                    callsites.push(QueryCallsite {
                        location: QueryLocation {
                            path: display_path(&self.project.root, path),
                            start_line: 1,
                            start_column: 1,
                            end_line: 1,
                            end_column: 1,
                        },
                        status: QueryCallsiteStatus::Skipped,
                        reason: format!("candidate source could not be parsed: {error}"),
                    });
                    continue;
                }
            };
            for import in &file.imports {
                inventory_snapshot.resolutions.push(linker.resolve(
                    &file.path,
                    &import.specifier,
                    import.span.clone(),
                    import.type_only,
                ));
            }
            inventory_snapshot.files.push(file);
        }
        let symbol_linker = SymbolLinker::new(&self.project, &inventory_snapshot);
        let model_symbol = symbol_linker.resolve_matcher(&query.factory);
        for path in &candidate_paths {
            let Some(file) = inventory_snapshot
                .files
                .iter()
                .find(|file| &file.path == path)
            else {
                continue;
            };
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let names = std::iter::once(query.factory.export.clone())
                .chain(
                    file.flow
                        .imports
                        .iter()
                        .filter(|import| {
                            model_symbol.as_ref().is_some_and(|symbol| {
                                symbol_linker.resolve_binding(file.file_id, &import.local)
                                    == ValueResolution::Resolved(LinkedValue::Declaration(
                                        symbol.clone(),
                                    ))
                            })
                        })
                        .map(|import| import.local.clone()),
                )
                .collect::<BTreeSet<_>>();
            for span in crate::solver::syntactic_callsite_spans(file, &names) {
                let location = inventory_location(&self.project.root, path, &source, &span);
                let status = if analyzed.contains(&location) {
                    QueryCallsiteStatus::Analyzed
                } else if indexed.contains(path) {
                    QueryCallsiteStatus::Unresolved
                } else {
                    QueryCallsiteStatus::Filtered
                };
                let reason = match status {
                    QueryCallsiteStatus::Analyzed => "matched configured factory".to_owned(),
                    QueryCallsiteStatus::Filtered => {
                        "possible factory call outside analyzed source set".to_owned()
                    }
                    QueryCallsiteStatus::Unresolved => {
                        "same-name call did not resolve to configured factory".to_owned()
                    }
                    QueryCallsiteStatus::Skipped => unreachable!(),
                };
                callsites.push(QueryCallsite {
                    location,
                    status,
                    reason,
                });
            }
        }
        for location in analyzed {
            if !callsites
                .iter()
                .any(|callsite| callsite.location == location)
            {
                callsites.push(QueryCallsite {
                    location,
                    status: QueryCallsiteStatus::Analyzed,
                    reason: "matched configured factory through an imported alias".to_owned(),
                });
            }
        }
        callsites.sort_by(|left, right| {
            (
                &left.location.path,
                left.location.start_line,
                left.location.start_column,
            )
                .cmp(&(
                    &right.location.path,
                    right.location.start_line,
                    right.location.start_column,
                ))
        });
        callsites
            .dedup_by(|left, right| left.location == right.location && left.status == right.status);
        report.callsite_inventory = QueryCallsiteInventory {
            configured_files: catalog
                .map_or(snapshot.files.len(), |catalog| catalog.configured_files),
            candidate_files: candidate_paths.len(),
            skipped_candidate_files: catalog.map_or(0, |catalog| catalog.skipped_candidate_files),
            round_limit_hit: catalog.is_some_and(|catalog| catalog.inventory_round_limit_hit),
            callsites,
        };
        if report.callsite_inventory.skipped_candidate_files > 0 {
            report.coverage.complete = false;
            report.coverage.gaps.push(format!(
                "callsite inventory skipped {} importers after its candidate budget",
                report.callsite_inventory.skipped_candidate_files
            ));
        }
        if report.callsite_inventory.round_limit_hit {
            report.coverage.complete = false;
            report
                .coverage
                .gaps
                .push("callsite inventory stopped after eight re-export rounds".to_owned());
        }
        Ok(())
    }

    fn extend_snapshot(
        &self,
        linker: &ModuleLinker,
        snapshot: &mut Snapshot,
        paths: &[PathBuf],
    ) -> Result<()> {
        for path in paths {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let file_id =
                FileId(u32::try_from(snapshot.files.len()).context("too many source files")?);
            let file = parse_and_lower(file_id, path, &source)?;
            for import in &file.imports {
                snapshot.resolutions.push(linker.resolve(
                    &file.path,
                    &import.specifier,
                    import.span.clone(),
                    import.type_only,
                ));
            }
            snapshot.files.push(file);
        }
        snapshot.snapshot_id = self.snapshot_id(&snapshot.files);
        Ok(())
    }

    fn snapshot_id(&self, files: &[crate::ir::FileIr]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.project.config_hash.as_bytes());
        hasher.update(FRONTEND_VERSION.as_bytes());
        for file in files {
            hasher.update(file.path.to_string_lossy().as_bytes());
            hasher.update(file.content_hash.as_bytes());
        }
        hex::encode(hasher.finalize())
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn inventory_location(root: &Path, path: &Path, source: &str, span: &SourceSpan) -> QueryLocation {
    fn line_column(source: &str, offset: u32) -> (u32, u32) {
        let mut offset = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(source.len());
        while !source.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit_once('\n')
            .map_or(prefix, |(_, line)| line)
            .chars()
            .count()
            + 1;
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
        )
    }
    let (start_line, start_column) = line_column(source, span.start);
    let (end_line, end_column) = line_column(source, span.end);
    QueryLocation {
        path: display_path(root, path),
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Analyzer;
    use crate::{link::ModuleLinker, project::Project, query::load_query};

    #[test]
    fn single_read_pass_indexes_the_same_sources_as_index() {
        let root = std::env::temp_dir().join(format!("flow-one-pass-{}", std::process::id()));
        let files = [
            (
                "flow.toml",
                "schema_version = 1\nname = 'sample'\nsource_roots = ['src', 'src/Root.ts']\nsource_contains_any = ['makeCallback']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n",
            ),
            (
                "query.toml",
                "schema_version = 1\nid = 'sample'\nkind = 'factory_return_invocations'\nscope = 'all_creations'\n[factory]\nproject = 'sample'\nmodule = 'src/factory.ts'\nexport = 'makeCallback'\n[[factory_arguments]]\nindex = 0\nlabel = 'created'\n[capability]\nreturned_property = ['callback']\n",
            ),
            (
                "src/factory.ts",
                "export function makeCallback(_value: string) { return { callback() {} }; }",
            ),
            (
                "src/Uses.ts",
                "import { makeCallback } from './factory'; export const used = makeCallback('alpha');",
            ),
            ("src/Host.tsx", "export function Host() { return null; }"),
            ("src/Root.ts", "export const root = true;"),
            ("src/Unrelated.ts", "export const unrelated = true;"),
        ];
        for (path, source) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("create fixture");
            fs::write(path, source).expect("write fixture");
        }
        let analyzer = Analyzer::new(Project::load(root.join("flow.toml")).expect("load project"));
        let (query, _) = load_query(&root.join("query.toml")).expect("load query");
        let linker = ModuleLinker::new(analyzer.project());
        let (snapshot, catalog) = analyzer
            .index_with_catalog(&linker, &query)
            .expect("index with catalog");
        let indexed = analyzer.index().expect("index");
        fs::remove_dir_all(&root).expect("remove fixture");

        assert_eq!(snapshot, indexed);
        let names = snapshot
            .files
            .iter()
            .filter_map(|file| file.path.file_name()?.to_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Host.tsx", "Root.ts", "Uses.ts", "factory.ts"]);
        assert_eq!(catalog.configured_files, 5);
        assert_eq!(catalog.candidate_paths.len(), 2);
    }
}
