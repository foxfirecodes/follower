use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    cache::{FRONTEND_VERSION, SNAPSHOT_SCHEMA_VERSION, Snapshot},
    frontend::parse_and_lower,
    ids::FileId,
    ir::{FlowExport, SourceSpan},
    link::{LinkedValue, ModuleLinker, SymbolLinker, ValueResolution},
    models::{Model, load_model_files},
    project::{Project, is_source_file},
    queries::AuditReport,
    query::{
        QueryCallsite, QueryCallsiteInventory, QueryCallsiteStatus, QueryLocation, QueryReport,
        QuerySpec,
    },
};

pub struct Analyzer {
    project: Project,
}

struct SourceCatalog {
    imports: Vec<(PathBuf, Vec<String>)>,
    configured_files: usize,
    candidate_paths: BTreeSet<PathBuf>,
    skipped_candidate_files: usize,
    inventory_round_limit_hit: bool,
}

impl SourceCatalog {
    fn build(project: &Project, query: &QuerySpec) -> Result<Self> {
        let mut imports = Vec::new();
        let mut configured_files = 0;
        let mut candidate_paths = BTreeSet::new();
        for path in project.discover_all_sources()? {
            if path
                .components()
                .any(|part| part.as_os_str() == OsStr::new("node_modules"))
            {
                continue;
            }
            configured_files += 1;
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if source.contains(&query.factory.export) {
                candidate_paths.insert(path.clone());
            }
            let specifiers = module_specifiers(&source);
            if !specifiers.is_empty() {
                imports.push((path, specifiers));
            }
        }
        let mut catalog = Self {
            imports,
            configured_files,
            candidate_paths,
            skipped_candidate_files: 0,
            inventory_round_limit_hit: false,
        };
        catalog.follow_factory_reexports(project, query)?;
        Ok(catalog)
    }

    fn follow_factory_reexports(&mut self, project: &Project, query: &QuerySpec) -> Result<()> {
        let factory_path = project
            .resolve_path(Path::new(&query.factory.module))
            .canonicalize()?;
        let mut targets = BTreeSet::from([factory_path]);
        let mut visited = BTreeSet::new();
        let mut skipped = BTreeSet::new();
        let linker = ModuleLinker::new(project);
        for round in 0..8 {
            let importers = self.reverse_importers(project, &targets);
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
        project: &Project,
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
        let linker = ModuleLinker::new(project);
        let mut result = BTreeSet::new();
        for (importer, specifiers) in &self.imports {
            if targets.contains(importer) {
                continue;
            }
            for specifier in specifiers {
                let stem = Path::new(specifier)
                    .file_stem()
                    .and_then(|name| name.to_str());
                if !stem.is_some_and(|stem| names.contains(stem)) {
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
                    break;
                }
            }
        }
        result
    }
}

fn module_specifiers(source: &str) -> Vec<String> {
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
        if keyword == b"require" && bytes.get(next) == Some(&b'(') {
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
        let source_paths = self.project.discover_sources()?;
        let mut files = Vec::with_capacity(source_paths.len());
        for (index, path) in source_paths.iter().enumerate() {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            files.push(parse_and_lower(
                FileId(u32::try_from(index).unwrap_or(u32::MAX)),
                path,
                &source,
            )?);
        }

        let linker = ModuleLinker::new(&self.project);
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
        let mut snapshot = self.index()?;
        let catalog = if self.project.config.source_contains_any.is_empty() {
            None
        } else {
            Some(SourceCatalog::build(&self.project, query)?)
        };
        let mut followed = 0;
        let mut skipped = BTreeSet::new();
        let mut reverse_seed_paths = BTreeSet::new();
        let mut reverse_producer_paths = BTreeSet::new();
        for round in 0..=8 {
            let (mut report, requests, producers) = crate::solver::execute_query(
                &self.project,
                &snapshot,
                query,
                query_hash,
                &reverse_seed_paths,
                &reverse_producer_paths,
            )?;
            if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                eprintln!(
                    "query round {round}: indexed={} requested={} producers={} reverse_seeds={}",
                    snapshot.files.len(),
                    requests.len(),
                    producers.len(),
                    reverse_seed_paths.len()
                );
            }
            if self.project.config.source_contains_any.is_empty() {
                self.attach_callsite_inventory(&mut report, &snapshot, query, None)?;
                report.finish_gaps();
                return Ok(report);
            }
            reverse_producer_paths.extend(producers);
            let reverse_requests = catalog.as_ref().map_or_else(BTreeSet::new, |catalog| {
                catalog.reverse_importers(&self.project, &reverse_producer_paths)
            });
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
                if path.is_file()
                    && is_source_file(path)
                    && !path
                        .components()
                        .any(|component| component.as_os_str() == OsStr::new("node_modules"))
                {
                    additions.push(path.clone());
                } else {
                    skipped.insert(path.clone());
                }
            }
            if additions.is_empty() || round == 8 || followed + additions.len() > 256 {
                if !additions.is_empty() {
                    report.coverage.gaps.push(format!(
                        "capability import expansion stopped after {followed} files and {round} rounds; {} requested files remain",
                        additions.len()
                    ));
                }
                if !skipped.is_empty() {
                    report.coverage.gaps.push(format!(
                        "{} capability imports were outside supported source files or node_modules",
                        skipped.len()
                    ));
                }
                if !report.coverage.gaps.is_empty() {
                    report.coverage.complete = false;
                }
                self.attach_callsite_inventory(&mut report, &snapshot, query, catalog.as_ref())?;
                report.finish_gaps();
                return Ok(report);
            }
            followed += additions.len();
            reverse_seed_paths.extend(
                additions
                    .iter()
                    .filter(|path| reverse_requests.contains(*path))
                    .cloned(),
            );
            self.extend_snapshot(&mut snapshot, &additions)?;
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
        }
        unreachable!("bounded import expansion always returns")
    }

    fn attach_callsite_inventory(
        &self,
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
        let linker = ModuleLinker::new(&self.project);
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

    fn extend_snapshot(&self, snapshot: &mut Snapshot, paths: &[PathBuf]) -> Result<()> {
        let linker = ModuleLinker::new(&self.project);
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
