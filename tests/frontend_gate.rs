use std::path::PathBuf;

use code_flow::{Analyzer, Project, link::ResolutionStatus};

fn fixture_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/reference/flow.toml")
}

#[test]
fn parses_binds_builds_cfg_and_resolves_imports() {
    let analyzer = Analyzer::new(Project::load(fixture_config()).expect("load fixture"));
    let snapshot = analyzer.index().expect("index fixture");

    assert_eq!(snapshot.files.len(), 4);
    assert!(snapshot.files.iter().all(|file| file.cfg_enabled));
    assert!(snapshot.files.iter().any(|file| !file.blocks.is_empty()));
    assert!(snapshot.files.iter().any(|file| {
        file.symbols
            .iter()
            .any(|symbol| symbol.name == "onDismiss" && symbol.reference_count > 0)
    }));
    assert!(snapshot.resolutions.iter().all(|resolution| {
        matches!(
            resolution.status,
            ResolutionStatus::Resolved | ResolutionStatus::TypeOnly
        )
    }));
    assert!(snapshot.files.iter().any(|file| {
        file.enum_members.iter().any(|member| {
            member.enum_name == "Notice"
                && member.member_name == "Welcome"
                && member.string_value.as_deref() == Some("welcome")
        })
    }));
}

#[test]
fn lowering_is_owned_and_contains_reference_operations() {
    let analyzer = Analyzer::new(Project::load(fixture_config()).expect("load fixture"));
    let snapshot = analyzer.index().expect("index fixture");
    let host = snapshot
        .files
        .iter()
        .find(|file| file.path.ends_with("Host.tsx"))
        .expect("Host file");

    assert!(host.flow.globals.iter().any(|binding| matches!(
        &binding.pattern.kind,
        code_flow::ir::FlowPatternKind::Identifier { name } if name == "registry"
    )));
    assert!(
        host.flow
            .functions
            .iter()
            .any(|function| function.name == "Host")
    );
    assert!(
        host.flow
            .functions
            .iter()
            .any(|function| function.name == "CloseButton")
    );

    let encoded = serde_json::to_vec(host).expect("serialize owned IR");
    let decoded: code_flow::ir::FileIr =
        serde_json::from_slice(&encoded).expect("deserialize owned IR");
    assert_eq!(&decoded, host);
}
