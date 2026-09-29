use std::path::Path;

use anyhow::{Context, Result};
use oxc::{
    allocator::Allocator,
    ast::ast::{
        ArrowFunctionExpression, Expression, Function, ImportDeclaration, ImportOrExportKind,
        TSEnumDeclaration,
    },
    ast_visit::{Visit, walk},
    parser::{Parser, ParserReturn},
    semantic::{SemanticBuilder, SemanticBuilderReturn},
    span::{SourceType, Span},
    syntax::scope::ScopeFlags,
};
use sha2::{Digest, Sha256};

use crate::{
    ids::{BlockId, FileId, FunctionId, SymbolId},
    ir::{
        EnumMemberIr, FileIr, FlowFileIr, ImportIr, OwnedBlock, OwnedFunction, OwnedSymbol,
        SourceSpan,
    },
};

pub fn parse_and_lower(file_id: FileId, path: &Path, source: &str) -> Result<FileIr> {
    let source_type = SourceType::from_path(path)
        .with_context(|| format!("unsupported source extension for {}", path.display()))?;
    let allocator = Allocator::default();
    let ParserReturn {
        program,
        diagnostics: parser_diagnostics,
        fatal_error,
        ..
    } = Parser::new(&allocator, source, source_type).parse();

    let mut diagnostics = parser_diagnostics
        .iter()
        .map(|diagnostic| format!("{diagnostic:?}"))
        .collect::<Vec<_>>();

    if fatal_error {
        return Ok(empty_file_ir(file_id, path, source, diagnostics));
    }

    let SemanticBuilderReturn {
        semantic,
        diagnostics: semantic_diagnostics,
    } = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .with_cfg(true)
        .build(&program);
    diagnostics.extend(
        semantic_diagnostics
            .iter()
            .map(|diagnostic| format!("{diagnostic:?}")),
    );

    let scoping = semantic.scoping();
    let symbols = scoping
        .symbol_ids()
        .enumerate()
        .map(|(index, symbol_id)| {
            let span = scoping.symbol_span(symbol_id);
            OwnedSymbol {
                id: SymbolId(u32::try_from(index).unwrap_or(u32::MAX)),
                name: scoping.symbol_name(symbol_id).to_owned(),
                declaration: owned_span(file_id, span),
                reference_count: scoping.get_resolved_reference_ids(symbol_id).len(),
            }
        })
        .collect();

    let mut collector = SyntaxCollector::new(file_id);
    collector.visit_program(&program);
    let flow = crate::frontend_lowering::lower(file_id, &program);

    let cfg_enabled = semantic.cfg().is_some();
    let blocks = semantic.cfg().map_or_else(Vec::new, |cfg| {
        let mut successors = vec![0_usize; cfg.basic_blocks.len()];
        for node in cfg.graph().node_indices() {
            let block_id = cfg.graph()[node];
            successors[block_id.index()] = cfg.graph().edges(node).count();
        }
        cfg.basic_blocks
            .iter()
            .enumerate()
            .map(|(index, block)| OwnedBlock {
                id: BlockId(u32::try_from(index).unwrap_or(u32::MAX)),
                instruction_count: block.instructions().len(),
                successor_count: successors[index],
            })
            .collect()
    });

    Ok(FileIr {
        file_id,
        path: path.to_path_buf(),
        content_hash: content_hash(source),
        source_len: source.len(),
        symbols,
        functions: collector.functions,
        blocks,
        imports: collector.imports,
        enum_members: collector.enum_members,
        flow,
        diagnostics,
        cfg_enabled,
    })
}

fn empty_file_ir(file_id: FileId, path: &Path, source: &str, diagnostics: Vec<String>) -> FileIr {
    FileIr {
        file_id,
        path: path.to_path_buf(),
        content_hash: content_hash(source),
        source_len: source.len(),
        symbols: Vec::new(),
        functions: Vec::new(),
        blocks: Vec::new(),
        imports: Vec::new(),
        enum_members: Vec::new(),
        flow: FlowFileIr::default(),
        diagnostics,
        cfg_enabled: false,
    }
}

fn content_hash(source: &str) -> String {
    hex::encode(Sha256::digest(source.as_bytes()))
}

fn owned_span(file_id: FileId, span: Span) -> SourceSpan {
    SourceSpan {
        file_id,
        start: span.start,
        end: span.end,
    }
}

struct SyntaxCollector {
    file_id: FileId,
    next_function_id: u32,
    functions: Vec<OwnedFunction>,
    imports: Vec<ImportIr>,
    enum_members: Vec<EnumMemberIr>,
}

impl SyntaxCollector {
    const fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            next_function_id: 0,
            functions: Vec::new(),
            imports: Vec::new(),
            enum_members: Vec::new(),
        }
    }

    fn push_function(&mut self, name: Option<String>, span: Span) {
        let id = FunctionId(self.next_function_id);
        self.next_function_id += 1;
        self.functions.push(OwnedFunction {
            id,
            name,
            span: owned_span(self.file_id, span),
        });
    }
}

impl<'a> Visit<'a> for SyntaxCollector {
    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        self.push_function(
            function
                .id
                .as_ref()
                .map(|identifier| identifier.name.to_string()),
            function.span,
        );
        walk::walk_function(self, function, flags);
    }

    fn visit_arrow_function_expression(&mut self, function: &ArrowFunctionExpression<'a>) {
        self.push_function(None, function.span);
        walk::walk_arrow_function_expression(self, function);
    }

    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.imports.push(ImportIr {
            specifier: declaration.source.value.to_string(),
            span: owned_span(self.file_id, declaration.span),
            type_only: declaration.import_kind == ImportOrExportKind::Type,
        });
        walk::walk_import_declaration(self, declaration);
    }

    fn visit_ts_enum_declaration(&mut self, declaration: &TSEnumDeclaration<'a>) {
        let enum_name = declaration.id.name.to_string();
        for member in &declaration.body.members {
            let string_value = match member.initializer.as_ref() {
                Some(Expression::StringLiteral(literal)) => Some(literal.value.to_string()),
                _ => None,
            };
            self.enum_members.push(EnumMemberIr {
                enum_name: enum_name.clone(),
                member_name: member.id.static_name().to_string(),
                string_value,
                span: owned_span(self.file_id, member.span),
            });
        }
        walk::walk_ts_enum_declaration(self, declaration);
    }
}
