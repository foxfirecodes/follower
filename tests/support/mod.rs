use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use code_flow::{
    Analyzer, Project,
    query::{QueryReport, QuerySpec},
};

static NEXT_PROJECT: AtomicUsize = AtomicUsize::new(0);

pub struct TestProject {
    pub root: PathBuf,
}

impl TestProject {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "flow-acceptance-{}-{}",
            std::process::id(),
            NEXT_PROJECT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(root.join("src")).expect("create fixture");
        let fixture = Self { root };
        fixture.write("flow.toml", "schema_version = 1\nname = 'acceptance'\nsource_roots = ['src']\n[[entries]]\nmodule = 'src/Host.tsx'\nexport = 'Host'\n");
        fixture.write(
            "src/factory.ts",
            "export function makeCallback(_value: string) { throw new Error('modeled factory'); }",
        );
        fixture.write("query.toml", "schema_version = 1\nid = 'acceptance'\nkind = 'factory_return_invocations'\nscope = 'reachable'\n[factory]\nproject = 'acceptance'\nmodule = 'src/factory.ts'\nexport = 'makeCallback'\n[[factory_arguments]]\nindex = 0\nlabel = 'created'\n[capability]\nreturned_property = ['callback']\n[[capability.invocation_arguments]]\nindex = 0\nlabel = 'invoked'\n");
        for (path, source) in files {
            fixture.write(path, source);
        }
        fixture
    }

    pub fn write(&self, path: &str, source: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, source).expect("write fixture");
    }

    pub fn analyzer(&self) -> Analyzer {
        Analyzer::new(Project::load(self.root.join("flow.toml")).expect("load project"))
    }

    pub fn query(&self) -> QuerySpec {
        code_flow::query::load_query(&self.root.join("query.toml"))
            .expect("load query")
            .0
    }

    pub fn report(&self) -> QueryReport {
        self.analyzer()
            .query(&self.query(), "test-query")
            .expect("query fixture")
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove fixture");
    }
}
