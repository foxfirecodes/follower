use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::ir::{FlowExpression, FlowExpressionKind, FlowFileIr};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub module: PathBuf,
    pub export: String,
}

/// An imported function that returns the result of calling one callback argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackSelectorImport {
    pub module: String,
    pub export: String,
    pub callback_argument: usize,
}

/// An imported factory whose configured callback returns a lazy component module.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LazyComponentFactory {
    pub module: String,
    pub export: String,
    pub promise_property: String,
}

/// An imported higher-order component that may render a component argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentWrapper {
    pub module: String,
    pub export: String,
    pub component_argument: usize,
    /// Whether the export returns the wrapper, as in `connect(mapState)(Component)`, so
    /// `component_argument` is a position in the call of its result.
    #[serde(default)]
    pub curried: bool,
}

/// An imported component that may render one or more of its props.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConsumer {
    pub module: String,
    pub export: String,
    #[serde(default)]
    pub forward_children: bool,
    #[serde(default)]
    pub invoke_children: bool,
    #[serde(default)]
    pub render_props: Vec<String>,
    #[serde(default)]
    pub render_callback_names: Vec<String>,
    #[serde(default)]
    pub component_props: Vec<String>,
    /// With a member, the export is a factory, and the contract applies to tags that are this
    /// member of what it returns, as `Stack.Screen` for `const Stack = createStack()`.
    #[serde(default)]
    pub member: Option<String>,
    /// Whether the factory returns a function whose result has the member, as
    /// `createFactory(View)(config)`.
    #[serde(default)]
    pub curried: bool,
}

/// An imported function that opens a component outside the caller's render, such as a modal or
/// sheet opener: given a component and its props, or a function that renders it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentOpener {
    pub module: String,
    pub export: String,
    /// The argument holding the component: the component, an `import()` of its module, or a
    /// loader that returns either.
    #[serde(default)]
    pub component_argument: Option<usize>,
    /// The argument holding the component's props.
    #[serde(default)]
    pub props_argument: Option<usize>,
    /// The properties from the props argument to the props, as `["props"]` for
    /// `open(Panel, { props: { onClose } })`.
    #[serde(default)]
    pub props_path: Vec<String>,
    /// The argument holding a function that returns the element or component to render, directly
    /// or through a promise.
    #[serde(default)]
    pub render_argument: Option<usize>,
}

/// A state library whose stores hold values that are written in one place and read in others.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateStore {
    /// The library's store semantics; `zustand` reads `S.setState(...)` as a write and
    /// `S(selector)`, `S.getState()`, and `useStore(S, selector)` as reads.
    pub kind: StateStoreKind,
    pub module: String,
    /// The function that creates a store, as `create`.
    pub export: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateStoreKind {
    Zustand,
}

/// A component or function the project declares is rendered or run somewhere, so exploration
/// starts there, as an entry does, when no path from an entry is found.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderRoot {
    pub module: PathBuf,
    pub export: String,
}

/// An imported function whose calls render what they are given: at every call of it, the
/// components its listed arguments hold are rendered and the functions are called, as for a
/// registry such as `createSetting(id, { useNotice, render })`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderCall {
    pub module: String,
    pub export: String,
    pub arguments: Vec<usize>,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    pub source_roots: Vec<PathBuf>,
    /// Text prefilter for directory roots. Explicit file roots are always indexed.
    #[serde(default)]
    pub source_contains_any: Vec<String>,
    #[serde(default)]
    pub entries: Vec<EntryPoint>,
    #[serde(default)]
    pub model_files: Vec<PathBuf>,
    #[serde(default)]
    pub resolution_conditions: Vec<String>,
    /// Platform suffixes an import tries before the plain file, in order, as `[".ios", ".native"]`
    /// resolves `./Panel` to `Panel.ios.tsx`, then `Panel.native.tsx`, then `Panel.tsx`.
    #[serde(default)]
    pub platform_extensions: Vec<String>,
    /// Paths the source walk skips, where `**` matches any number of path segments and `*` any
    /// characters within one, as `**/web/**` or `**/*.web.tsx`. Matched against the file's full
    /// path. A skipped file is still parsed when an import resolves to it.
    #[serde(default)]
    pub source_excludes: Vec<String>,
    #[serde(default)]
    pub import_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub callback_selector_imports: Vec<CallbackSelectorImport>,
    #[serde(default)]
    pub lazy_component_factories: Vec<LazyComponentFactory>,
    #[serde(default)]
    pub component_wrappers: Vec<ComponentWrapper>,
    #[serde(default)]
    pub component_consumers: Vec<ComponentConsumer>,
    #[serde(default)]
    pub component_openers: Vec<ComponentOpener>,
    #[serde(default)]
    pub render_roots: Vec<RenderRoot>,
    #[serde(default)]
    pub render_calls: Vec<RenderCall>,
    #[serde(default)]
    pub state_stores: Vec<StateStore>,
    #[serde(default)]
    pub inputs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub limits: Limits,
}

/// File budgets for a filtered query. Larger budgets parse more of a large project, at the cost
/// of time and of exploration spread over more files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Files discovery may add before exploring from the entries.
    pub discovery_files: usize,
    /// Files the root phase may add for what paths from the entries read.
    pub root_phase_files: usize,
    /// Files the backward use walk may add.
    pub backward_walk_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            discovery_files: 256,
            root_phase_files: 512,
            backward_walk_files: 256,
        }
    }
}

impl ProjectConfig {
    pub fn imported_export(
        &self,
        file: &FlowFileIr,
        expression: &FlowExpression,
    ) -> Option<(String, String)> {
        let (local, member) = match &expression.kind {
            FlowExpressionKind::Identifier {
                name,
                module_binding: true,
            } => (name.as_str(), None),
            FlowExpressionKind::StaticMember { object, property } => {
                let FlowExpressionKind::Identifier {
                    name,
                    module_binding: true,
                } = &object.kind
                else {
                    return None;
                };
                (name.as_str(), Some(property.as_str()))
            }
            _ => return None,
        };
        let import = file
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)?;
        let exported = match member {
            Some(name) if import.imported == "*" => name,
            Some(name) => {
                return Some((
                    import.module.clone(),
                    format!("{}.{}", import.imported, name),
                ));
            }
            None => import.imported.as_str(),
        };
        Some((import.module.clone(), exported.to_owned()))
    }

    pub fn lazy_factory_property<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Option<&'a str> {
        let (local, member) = match &callee.kind {
            FlowExpressionKind::Identifier {
                name,
                module_binding: true,
            } => (name.as_str(), None),
            FlowExpressionKind::StaticMember { object, property } => {
                let FlowExpressionKind::Identifier {
                    name,
                    module_binding: true,
                } = &object.kind
                else {
                    return None;
                };
                (name.as_str(), Some(property.as_str()))
            }
            _ => return None,
        };
        let import = file
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)?;
        let exported = match member {
            Some(name) if import.imported == "*" => name,
            None => import.imported.as_str(),
            _ => return None,
        };
        self.lazy_component_factories
            .iter()
            .find(|factory| factory.module == import.module && factory.export == exported)
            .map(|factory| factory.promise_property.as_str())
    }

    /// The wrapper a call's callee is: the export itself, or for a curried wrapper, a call of it.
    pub fn component_wrapper<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Option<&'a ComponentWrapper> {
        let (callee, curried) = match &callee.kind {
            FlowExpressionKind::Call { callee, .. } => (callee.as_ref(), true),
            _ => (callee, false),
        };
        let (module, export) = self.imported_export(file, callee)?;
        self.component_wrappers.iter().find(|model| {
            model.module == module && model.export == export && model.curried == curried
        })
    }

    pub fn component_consumer<'a>(
        &'a self,
        file: &FlowFileIr,
        component: &FlowExpression,
    ) -> Option<&'a ComponentConsumer> {
        let (module, export) = self.imported_export(file, component)?;
        self.component_consumers.iter().find(|model| {
            model.member.is_none() && model.module == module && model.export == export
        })
    }

    /// The members a call of a configured factory returns, as `Screen` and `Navigator` for
    /// `createStack()`, or for `createFactory(View)(config)` when the contract is curried.
    pub fn component_factory_members<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Vec<&'a ComponentConsumer> {
        let (callee, curried) = match &callee.kind {
            FlowExpressionKind::Call { callee, .. } => (callee.as_ref(), true),
            _ => (callee, false),
        };
        let Some((module, export)) = self.imported_export(file, callee) else {
            return Vec::new();
        };
        self.component_consumers
            .iter()
            .filter(|model| {
                model.member.is_some()
                    && model.curried == curried
                    && model.module == module
                    && model.export == export
            })
            .collect()
    }

    /// The store library a call creates a store with, as `create(...)` or the curried
    /// `create<State>()(...)`.
    pub fn state_store<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Option<&'a StateStore> {
        let callee = match &callee.kind {
            FlowExpressionKind::Call { callee, .. } => callee.as_ref(),
            _ => callee,
        };
        let (module, export) = self.imported_export(file, callee)?;
        self.state_stores
            .iter()
            .find(|store| store.module == module && store.export == export)
    }

    pub fn render_call<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Option<&'a RenderCall> {
        let (module, export) = self.imported_export(file, callee)?;
        self.render_calls
            .iter()
            .find(|model| model.module == module && model.export == export)
    }

    pub fn component_opener<'a>(
        &'a self,
        file: &FlowFileIr,
        callee: &FlowExpression,
    ) -> Option<&'a ComponentOpener> {
        let (module, export) = self.imported_export(file, callee)?;
        self.component_openers
            .iter()
            .find(|model| model.module == module && model.export == export)
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub config_hash: String,
}

impl Project {
    /// Whether the source walk skips a file, by `source_excludes`.
    pub(crate) fn excluded(&self, path: &Path) -> bool {
        let path = path.to_string_lossy();
        self.config
            .source_excludes
            .iter()
            .any(|pattern| glob_matches(pattern, &path))
    }

    pub fn load(config_path: impl AsRef<Path>) -> Result<Self> {
        let config_path = config_path.as_ref().canonicalize().with_context(|| {
            format!(
                "failed to locate project configuration {}",
                config_path.as_ref().display()
            )
        })?;
        let root = config_path
            .parent()
            .context("project configuration has no parent directory")?
            .to_path_buf();
        let source = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let config: ProjectConfig = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        if config.schema_version != 1 {
            bail!(
                "unsupported project schema version {}; expected 1",
                config.schema_version
            );
        }
        if config.source_roots.is_empty() {
            bail!("project must configure at least one source root");
        }
        if config
            .source_contains_any
            .iter()
            .any(|term| term.is_empty())
        {
            bail!("source_contains_any terms cannot be empty");
        }
        for (alias, target) in &config.import_aliases {
            if alias.is_empty()
                || target.is_empty()
                || alias.matches('*').count() > 1
                || target.matches('*').count() > 1
            {
                bail!("invalid import alias {alias:?} -> {target:?}");
            }
            if alias.contains('*') != target.contains('*') {
                bail!(
                    "import alias {alias:?} and target {target:?} must both use * or neither use it"
                );
            }
        }
        validate_contracts(&config)?;
        let config_hash = hex::encode(Sha256::digest(source.as_bytes()));
        Ok(Self {
            config_path,
            root,
            config,
            config_hash,
        })
    }

    pub fn resolve_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    pub fn discover_sources(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for candidate in self.discover_source_candidates()? {
            if candidate.text_filtered && !self.config.source_contains_any.is_empty() {
                let source = fs::read_to_string(&candidate.path)
                    .with_context(|| format!("failed to read {}", candidate.path.display()))?;
                if !self.matches_source_filter(&source) {
                    continue;
                }
            }
            files.push(candidate.path);
        }
        Ok(files)
    }

    pub fn discover_all_sources(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .discover_source_candidates()?
            .into_iter()
            .map(|candidate| candidate.path)
            .collect())
    }

    /// Returns whether source text contains a configured `source_contains_any` term.
    pub(crate) fn matches_source_filter(&self, source: &str) -> bool {
        self.config
            .source_contains_any
            .iter()
            .any(|term| source.contains(term))
    }

    /// Lists configured sources in path order without reading them.
    pub(crate) fn discover_source_candidates(&self) -> Result<Vec<SourceCandidate>> {
        let mut files = Vec::new();
        for configured_root in &self.config.source_roots {
            let root = self.resolve_path(configured_root);
            if !root.exists() {
                bail!("source root does not exist: {}", root.display());
            }
            let root = root.canonicalize().with_context(|| {
                format!("failed to canonicalize source root {}", root.display())
            })?;
            if root.is_file() {
                if !is_source_file(&root) {
                    bail!(
                        "source root is not a supported source file: {}",
                        root.display()
                    );
                }
                files.push(SourceCandidate {
                    path: root,
                    text_filtered: false,
                });
                continue;
            }
            for entry in WalkDir::new(&root).follow_links(false) {
                let entry =
                    entry.with_context(|| format!("failed while walking {}", root.display()))?;
                if entry.file_type().is_file()
                    && is_source_file(entry.path())
                    && !self.excluded(entry.path())
                {
                    files.push(SourceCandidate {
                        path: entry.path().to_path_buf(),
                        text_filtered: true,
                    });
                }
            }
        }
        let roots = self
            .config
            .entries
            .iter()
            .map(|entry| (&entry.module, "entry"))
            .chain(
                self.config
                    .render_roots
                    .iter()
                    .map(|root| (&root.module, "render root")),
            );
        for (module, kind) in roots {
            let path = self.resolve_path(module);
            if !is_source_file(&path) {
                bail!(
                    "{kind} module is not a supported source file: {}",
                    path.display()
                );
            }
            files.push(SourceCandidate {
                path: path.canonicalize().with_context(|| {
                    format!("failed to locate {kind} module {}", path.display())
                })?,
                text_filtered: false,
            });
        }
        // A file listed as a root or entry is kept even when a directory walk also finds it.
        files.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.text_filtered.cmp(&right.text_filtered))
        });
        files.dedup_by(|later, earlier| later.path == earlier.path);
        Ok(files)
    }
}

/// A discovered source file. Directory-walk files are subject to the text prefilter.
pub(crate) struct SourceCandidate {
    pub path: PathBuf,
    pub text_filtered: bool,
}

/// Checks the contracts and resolution settings a project configuration declares.
fn validate_contracts(config: &ProjectConfig) -> Result<()> {
    for selector in &config.callback_selector_imports {
        if selector.module.trim().is_empty() || selector.export.trim().is_empty() {
            bail!("callback selector imports need a module and export name");
        }
    }
    for factory in &config.lazy_component_factories {
        if factory.module.trim().is_empty()
            || factory.export.trim().is_empty()
            || factory.promise_property.trim().is_empty()
        {
            bail!("lazy component factories need a module, export, and promise_property");
        }
    }
    for wrapper in &config.component_wrappers {
        if wrapper.module.trim().is_empty() || wrapper.export.trim().is_empty() {
            bail!("component wrappers need a module and export");
        }
    }
    for extension in &config.platform_extensions {
        if !extension.starts_with('.') || extension.len() < 2 || extension.contains('/') {
            bail!("platform extensions must look like \".native\": {extension:?}");
        }
    }
    if config
        .source_excludes
        .iter()
        .any(|pattern| pattern.trim().is_empty())
    {
        bail!("source excludes cannot be empty");
    }
    for call in &config.render_calls {
        if call.module.trim().is_empty()
            || call.export.trim().is_empty()
            || call.arguments.is_empty()
        {
            bail!("render calls need a module, an export, and at least one argument");
        }
    }
    if config
        .render_roots
        .iter()
        .any(|root| root.export.trim().is_empty())
    {
        bail!("render roots need an export");
    }
    for opener in &config.component_openers {
        if opener.module.trim().is_empty() || opener.export.trim().is_empty() {
            bail!("component openers need a module and export");
        }
        if opener.component_argument.is_none() && opener.render_argument.is_none() {
            bail!("component openers need a component_argument or a render_argument");
        }
        if opener.component_argument.is_none()
            && (opener.props_argument.is_some() || !opener.props_path.is_empty())
        {
            bail!("component opener props need a component_argument");
        }
        if opener.props_path.iter().any(|step| step.trim().is_empty()) {
            bail!("component opener props_path steps cannot be empty");
        }
    }
    for consumer in &config.component_consumers {
        if consumer.curried && consumer.member.is_none() {
            bail!("a curried component consumer needs a member");
        }
        if consumer
            .member
            .as_ref()
            .is_some_and(|member| member.trim().is_empty())
        {
            bail!("component consumer members cannot be empty");
        }
        if consumer.module.trim().is_empty()
            || consumer.export.trim().is_empty()
            || consumer
                .render_props
                .iter()
                .chain(&consumer.component_props)
                .any(|prop| prop.trim().is_empty())
            || consumer
                .render_callback_names
                .iter()
                .any(|name| name.trim().is_empty())
        {
            bail!("component consumers need a module, export, and nonempty prop names");
        }
    }
    Ok(())
}

/// Whether a path matches a pattern in which `**` matches any number of path segments and `*`
/// any characters within one segment.
pub(crate) fn glob_matches(pattern: &str, path: &str) -> bool {
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| segments(rest, &path[skip..])),
            Some((first, rest)) => path.split_first().is_some_and(|(segment, others)| {
                segment_matches(first, segment) && segments(rest, others)
            }),
        }
    }
    fn segment_matches(pattern: &str, text: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == text,
            Some((prefix, rest)) => text.strip_prefix(prefix).is_some_and(|tail| {
                tail.char_indices()
                    .map(|(index, _)| index)
                    .chain(std::iter::once(tail.len()))
                    .any(|index| segment_matches(rest, &tail[index..]))
            }),
        }
    }
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let path = path.split('/').collect::<Vec<_>>();
    segments(&pattern, &path)
}

pub(crate) fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    ) && !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".d.ts"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{glob_matches, is_source_file};

    #[test]
    fn source_filter_excludes_declaration_files() {
        assert!(is_source_file(Path::new("Host.tsx")));
        assert!(!is_source_file(Path::new("types.d.ts")));
        assert!(!is_source_file(Path::new("package.json")));
    }

    #[test]
    fn glob_patterns_match_segments_and_any_depth() {
        assert!(glob_matches(
            "**/web/**",
            "/repo/modules/panel/web/Panel.tsx"
        ));
        assert!(!glob_matches(
            "**/web/**",
            "/repo/modules/panel/native/Panel.tsx"
        ));
        assert!(glob_matches("**/*.web.tsx", "/repo/modules/Panel.web.tsx"));
        assert!(!glob_matches(
            "**/*.web.tsx",
            "/repo/modules/Panel.native.tsx"
        ));
        assert!(glob_matches("/repo/*/Panel.tsx", "/repo/modules/Panel.tsx"));
        assert!(!glob_matches(
            "/repo/*/Panel.tsx",
            "/repo/modules/panel/Panel.tsx"
        ));
        assert!(glob_matches("**", "/repo/Panel.tsx"));
    }
}
