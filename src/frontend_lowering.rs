use std::collections::{BTreeMap, BTreeSet};

use oxc::{
    ast::ast::{
        Argument, ArrayExpressionElement, ArrowFunctionBody, BindingPattern, CallExpression,
        ChainElement, Class, ClassElement, Declaration, ExportDefaultDeclarationKind, Expression,
        FormalParameters, Function, IdentifierReference, ImportDeclaration,
        ImportDeclarationSpecifier, ImportOrExportKind, JSXAttributeItem, JSXAttributeValue,
        JSXChild, JSXElement, JSXElementName, JSXExpression, ObjectPropertyKind,
        SimpleAssignmentTarget, Statement, StaticMemberExpression, TSEnumDeclaration,
        VariableDeclaration,
    },
    ast_visit::Visit,
    semantic::Scoping,
    span::{GetSpan, Span},
    syntax::operator::{AssignmentOperator, BinaryOperator, LogicalOperator, UnaryOperator},
};

use crate::{
    ids::FileId,
    ir::{
        FlowArrowBody, FlowAssignmentTarget, FlowBinding, FlowDefaultProps, FlowExport,
        FlowExpression, FlowExpressionKind, FlowFileIr, FlowFunction, FlowImport, FlowJsxProp,
        FlowJsxTag, FlowLogicalOperator, FlowPattern, FlowPatternField, FlowPatternKind,
        FlowRecordField, FlowStatement, SourceSpan, UnsupportedIr,
    },
};

pub fn lower(
    file_id: FileId,
    program: &oxc::ast::ast::Program<'_>,
    scoping: &Scoping,
) -> FlowFileIr {
    let mut lowerer = Lowerer {
        file_id,
        output: FlowFileIr::default(),
        scoping,
        current_class: None,
        class_members: BTreeMap::new(),
    };
    for statement in &program.body {
        lowerer.lower_top_level(statement);
    }
    lowerer.output
}

fn referenced_names(expression: &Expression<'_>) -> Vec<String> {
    #[derive(Default)]
    struct References(BTreeSet<String>);

    impl<'a> Visit<'a> for References {
        fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
            self.0.insert(identifier.name.to_string());
        }
    }

    let mut references = References::default();
    references.visit_expression(expression);
    references.0.into_iter().collect()
}

struct Lowerer<'s> {
    file_id: FileId,
    output: FlowFileIr,
    scoping: &'s Scoping,
    current_class: Option<String>,
    /// Methods and function-valued properties of the current class, with their arity.
    class_members: BTreeMap<String, usize>,
}

impl Lowerer<'_> {
    fn is_module_reference(&self, identifier: &IdentifierReference<'_>) -> bool {
        identifier
            .reference_id
            .get()
            .and_then(|reference| self.scoping.get_reference(reference).symbol_id())
            .is_some_and(|symbol| {
                self.scoping.symbol_scope_id(symbol) == self.scoping.root_scope_id()
            })
    }
    /// Whether a name refers to a global, such as the built-in `Set`, rather than a binding.
    fn is_global_reference(&self, identifier: &IdentifierReference<'_>) -> bool {
        identifier
            .reference_id
            .get()
            .is_none_or(|reference| self.scoping.get_reference(reference).symbol_id().is_none())
    }

    fn span(&self, span: Span) -> SourceSpan {
        SourceSpan {
            file_id: self.file_id,
            start: span.start,
            end: span.end,
        }
    }

    fn unsupported(&self, syntax: &str, span: Span) -> UnsupportedIr {
        UnsupportedIr {
            syntax: syntax.to_owned(),
            span: self.span(span),
        }
    }

    fn lower_top_level(&mut self, statement: &Statement<'_>) {
        match statement {
            Statement::ImportDeclaration(declaration) => self.lower_import(declaration),
            Statement::VariableDeclaration(declaration) => {
                let bindings = self.lower_bindings(declaration);
                self.output.globals.extend(bindings);
            }
            Statement::FunctionDeclaration(function) => self.lower_function(function),
            Statement::ClassDeclaration(class) => self.lower_class(class),
            Statement::ExpressionStatement(statement) => {
                if let Expression::AssignmentExpression(assignment) = &statement.expression
                    && let oxc::ast::ast::AssignmentTarget::StaticMemberExpression(member) =
                        &assignment.left
                    && member.property.name == "defaultProps"
                    && let Expression::Identifier(component) = &member.object
                {
                    let value = self.lower_expression(&assignment.right);
                    self.output.default_props.push(FlowDefaultProps {
                        component: component.name.to_string(),
                        value,
                    });
                }
            }
            Statement::ExportDeclaration(export) => {
                self.record_direct_exports(&export.declaration);
                self.lower_declaration(&export.declaration);
            }
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                    if let Some(identifier) = &function.id {
                        self.output.exports.push(FlowExport::Local {
                            local: identifier.name.to_string(),
                            exported: "default".to_owned(),
                            type_only: false,
                            span: self.span(export.span),
                        });
                    }
                    self.lower_function(function);
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    if let Some(identifier) = &class.id {
                        self.output.exports.push(FlowExport::Local {
                            local: identifier.name.to_string(),
                            exported: "default".to_owned(),
                            type_only: false,
                            span: self.span(export.span),
                        });
                    }
                    self.lower_class(class);
                }
                declaration => {
                    if let Some(expression) = declaration.as_expression() {
                        let name = "__follower_default_export".to_owned();
                        self.output.exports.push(FlowExport::Local {
                            local: name.clone(),
                            exported: "default".to_owned(),
                            type_only: false,
                            span: self.span(export.span),
                        });
                        self.output.globals.push(FlowBinding {
                            lexical: true,
                            pattern: FlowPattern {
                                kind: FlowPatternKind::Identifier { name },
                                span: self.span(export.span),
                            },
                            value: self.lower_expression(expression),
                            span: self.span(export.span),
                        });
                    } else {
                        self.output
                            .unsupported
                            .push(self.unsupported("unsupported_default_export", export.span));
                    }
                }
            },
            Statement::ExportNamedDeclaration(export) => {
                for specifier in &export.specifiers {
                    self.output.exports.push(FlowExport::Local {
                        local: specifier.local.name().to_string(),
                        exported: specifier.exported.name().to_string(),
                        type_only: export.export_kind == ImportOrExportKind::Type
                            || specifier.export_kind == ImportOrExportKind::Type,
                        span: self.span(specifier.span),
                    });
                }
            }
            Statement::ExportFromDeclaration(export) => {
                for specifier in &export.specifiers {
                    self.output.exports.push(FlowExport::ReExport {
                        imported: specifier.local.name().to_string(),
                        exported: specifier.exported.name().to_string(),
                        module: export.source.value.to_string(),
                        type_only: export.export_kind == ImportOrExportKind::Type
                            || specifier.export_kind == ImportOrExportKind::Type,
                        span: self.span(specifier.span),
                    });
                }
            }
            Statement::ExportAllDeclaration(export) => {
                let type_only = export.export_kind == ImportOrExportKind::Type;
                self.output
                    .exports
                    .push(export.exported.as_ref().map_or_else(
                        || FlowExport::Star {
                            module: export.source.value.to_string(),
                            type_only,
                            span: self.span(export.span),
                        },
                        |exported| FlowExport::Namespace {
                            exported: exported.name().to_string(),
                            module: export.source.value.to_string(),
                            type_only,
                            span: self.span(export.span),
                        },
                    ));
            }
            Statement::TSEnumDeclaration(declaration) => {
                let binding = self.lower_enum(declaration);
                self.output.globals.push(binding);
            }
            _ => self
                .output
                .unsupported
                .push(self.unsupported("unsupported_top_level_statement", statement.span())),
        }
    }

    fn lower_declaration(&mut self, declaration: &Declaration<'_>) {
        match declaration {
            Declaration::VariableDeclaration(declaration) => {
                let bindings = self.lower_bindings(declaration);
                self.output.globals.extend(bindings);
            }
            Declaration::FunctionDeclaration(function) => self.lower_function(function),
            Declaration::ClassDeclaration(class) => self.lower_class(class),
            Declaration::TSEnumDeclaration(declaration) => {
                let binding = self.lower_enum(declaration);
                self.output.globals.push(binding);
            }
            _ => self
                .output
                .unsupported
                .push(self.unsupported("unsupported_exported_declaration", declaration.span())),
        }
    }

    fn lower_enum(&self, declaration: &TSEnumDeclaration<'_>) -> FlowBinding {
        let span = self.span(declaration.span);
        FlowBinding {
            lexical: true,
            pattern: FlowPattern {
                kind: FlowPatternKind::Identifier {
                    name: declaration.id.name.to_string(),
                },
                span: span.clone(),
            },
            value: FlowExpression {
                kind: FlowExpressionKind::Record {
                    fields: declaration
                        .body
                        .members
                        .iter()
                        .map(|member| FlowRecordField {
                            spread: false,
                            property: member.id.static_name().to_string(),
                            value: member.initializer.as_ref().map_or_else(
                                || self.unsupported_expression("implicit_enum_value", member.span),
                                |expression| {
                                    if let Expression::NumericLiteral(number) = expression
                                        && number.value.fract() == 0.0
                                        && number.value >= i64::MIN as f64
                                        && number.value <= i64::MAX as f64
                                    {
                                        FlowExpression {
                                            kind: FlowExpressionKind::NumericEnumMember {
                                                enum_name: declaration.id.name.to_string(),
                                                member_name: member.id.static_name().to_string(),
                                                value: number.value as i64,
                                            },
                                            span: self.span(expression.span()),
                                        }
                                    } else {
                                        self.lower_expression(expression)
                                    }
                                },
                            ),
                            span: self.span(member.span),
                        })
                        .collect(),
                },
                span: span.clone(),
            },
            span,
        }
    }

    fn record_direct_exports(&mut self, declaration: &Declaration<'_>) {
        let (names, span) = match declaration {
            Declaration::FunctionDeclaration(function) => (
                function
                    .id
                    .iter()
                    .map(|identifier| identifier.name.to_string())
                    .collect::<Vec<_>>(),
                function.span,
            ),
            Declaration::VariableDeclaration(declaration) => (
                declaration
                    .declarations
                    .iter()
                    .flat_map(|declarator| {
                        crate::link::pattern_names(&self.lower_pattern(&declarator.id))
                            .into_iter()
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .collect(),
                declaration.span,
            ),
            Declaration::TSEnumDeclaration(declaration) => {
                (vec![declaration.id.name.to_string()], declaration.span)
            }
            Declaration::ClassDeclaration(class) => (
                class.id.iter().map(|id| id.name.to_string()).collect(),
                class.span,
            ),
            _ => return,
        };
        let span = self.span(span);
        self.output
            .exports
            .extend(names.into_iter().map(|name| FlowExport::Local {
                local: name.clone(),
                exported: name,
                type_only: false,
                span: span.clone(),
            }));
    }

    fn lower_import(&mut self, declaration: &ImportDeclaration<'_>) {
        let Some(specifiers) = &declaration.specifiers else {
            return;
        };
        for specifier in specifiers {
            let (local, imported, specifier_type_only) = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => (
                    specifier.local.name.to_string(),
                    specifier.imported.name().to_string(),
                    specifier.import_kind == ImportOrExportKind::Type,
                ),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => (
                    specifier.local.name.to_string(),
                    "default".to_owned(),
                    false,
                ),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    (specifier.local.name.to_string(), "*".to_owned(), false)
                }
            };
            self.output.imports.push(FlowImport {
                local,
                imported,
                module: declaration.source.value.to_string(),
                type_only: specifier_type_only
                    || declaration.import_kind == ImportOrExportKind::Type,
                span: self.span(specifier.span()),
            });
        }
    }

    fn lower_function(&mut self, function: &Function<'_>) {
        let Some(identifier) = &function.id else {
            self.output
                .unsupported
                .push(self.unsupported("anonymous_function_declaration", function.span));
            return;
        };
        let Some(body) = &function.body else {
            self.output
                .unsupported
                .push(self.unsupported("function_without_implementation", function.span));
            return;
        };
        self.output.functions.push(FlowFunction {
            name: identifier.name.to_string(),
            params: self.lower_params(&function.params),
            body: self.lower_statements(&body.statements),
            span: self.span(function.span),
        });
    }

    fn lower_class(&mut self, class: &Class<'_>) {
        let Some(identifier) = &class.id else {
            self.output
                .unsupported
                .push(self.unsupported("anonymous_class", class.span));
            return;
        };
        let class_name = identifier.name.to_string();
        self.current_class = Some(class_name.clone());
        self.class_members = class
            .body
            .body
            .iter()
            .filter_map(|element| match element {
                ClassElement::MethodDefinition(method)
                    if !method.r#static
                        && !method.computed
                        && method.kind == oxc::ast::ast::MethodDefinitionKind::Method =>
                {
                    let name = method.key.static_name()?;
                    (name != "render").then(|| (name.into_owned(), method.value.params.items.len()))
                }
                ClassElement::PropertyDefinition(property)
                    if !property.r#static && !property.computed =>
                {
                    let arity = match property.value.as_ref()? {
                        Expression::ArrowFunctionExpression(function) => {
                            function.params.items.len()
                        }
                        Expression::FunctionExpression(function) => function.params.items.len(),
                        _ => return None,
                    };
                    Some((property.key.static_name()?.into_owned(), arity))
                }
                _ => None,
            })
            .collect();
        for element in &class.body.body {
            // A function-valued class property is a method bound to the instance.
            if let ClassElement::PropertyDefinition(property) = element
                && !property.r#static
                && !property.computed
                && let Some(name) = property.key.static_name()
                && let Some(value) = &property.value
            {
                let lowered = match value {
                    Expression::ArrowFunctionExpression(function) => Some((
                        self.lower_params(&function.params),
                        match &function.body {
                            ArrowFunctionBody::FunctionBody(body) => {
                                self.lower_statements(&body.statements)
                            }
                            body => vec![FlowStatement::Return {
                                value: Some(self.lower_expression(body.to_expression())),
                                span: self.span(function.span),
                            }],
                        },
                    )),
                    Expression::FunctionExpression(function) => Some((
                        self.lower_params(&function.params),
                        function
                            .body
                            .as_ref()
                            .map_or_else(Vec::new, |body| self.lower_statements(&body.statements)),
                    )),
                    _ => None,
                };
                if let Some((params, body)) = lowered {
                    let mut all = vec![FlowPattern {
                        kind: FlowPatternKind::Identifier {
                            name: "props".to_owned(),
                        },
                        span: self.span(property.span),
                    }];
                    all.extend(params);
                    self.output.functions.push(FlowFunction {
                        name: format!("{class_name}.{name}"),
                        params: all,
                        body,
                        span: self.span(property.span),
                    });
                }
            }
            if let ClassElement::PropertyDefinition(property) = element
                && property.r#static
                && property.key.static_name().as_deref() == Some("defaultProps")
                && let Some(value) = &property.value
            {
                let value = self.lower_expression(value);
                self.output.default_props.push(FlowDefaultProps {
                    component: class_name.clone(),
                    value,
                });
            }
            let ClassElement::MethodDefinition(method) = element else {
                continue;
            };
            if method.r#static
                || method.computed
                || method.kind != oxc::ast::ast::MethodDefinitionKind::Method
            {
                continue;
            }
            let Some(method_name) = method.key.static_name() else {
                continue;
            };
            let Some(body) = &method.value.body else {
                continue;
            };
            let props = FlowPattern {
                kind: FlowPatternKind::Identifier {
                    name: "props".to_owned(),
                },
                span: self.span(method.span),
            };
            let mut params = vec![props];
            params.extend(self.lower_params(&method.value.params));
            self.output.functions.push(FlowFunction {
                name: if method_name == "render" {
                    class_name.clone()
                } else {
                    format!("{class_name}.{method_name}")
                },
                params,
                body: self.lower_statements(&body.statements),
                span: self.span(method.span),
            });
        }
        self.current_class = None;
        self.class_members.clear();
    }

    fn lower_params(&self, params: &FormalParameters<'_>) -> Vec<FlowPattern> {
        params
            .items
            .iter()
            .map(|parameter| {
                let pattern = self.lower_pattern(&parameter.pattern);
                match &parameter.initializer {
                    Some(default) => FlowPattern {
                        span: self.span(parameter.span),
                        kind: FlowPatternKind::Default {
                            target: Box::new(pattern),
                            default: Box::new(self.lower_expression(default)),
                        },
                    },
                    None => pattern,
                }
            })
            .collect()
    }

    fn lower_pattern(&self, pattern: &BindingPattern<'_>) -> FlowPattern {
        let kind = match pattern {
            BindingPattern::BindingIdentifier(identifier) => FlowPatternKind::Identifier {
                name: identifier.name.to_string(),
            },
            BindingPattern::ObjectPattern(object) => {
                let fields = object
                    .properties
                    .iter()
                    .map(|property| {
                        let source_property = property
                            .key
                            .static_name()
                            .map_or_else(|| "<computed>".to_owned(), std::borrow::Cow::into_owned);
                        FlowPatternField {
                            source_property,
                            target: self.lower_pattern(&property.value),
                        }
                    })
                    .collect();
                FlowPatternKind::Object {
                    fields,
                    rest: object
                        .rest
                        .as_ref()
                        .map(|rest| Box::new(self.lower_pattern(&rest.argument))),
                }
            }
            BindingPattern::AssignmentPattern(assignment) => FlowPatternKind::Default {
                target: Box::new(self.lower_pattern(&assignment.left)),
                default: Box::new(self.lower_expression(&assignment.right)),
            },
            BindingPattern::ArrayPattern(array) if array.rest.is_none() => FlowPatternKind::Array {
                elements: array
                    .elements
                    .iter()
                    .map(|element| element.as_ref().map(|element| self.lower_pattern(element)))
                    .collect(),
            },
            _ => FlowPatternKind::Unsupported {
                syntax: "unsupported_binding_pattern".to_owned(),
            },
        };
        FlowPattern {
            kind,
            span: self.span(pattern.span()),
        }
    }

    fn lower_bindings(&self, declaration: &VariableDeclaration<'_>) -> Vec<FlowBinding> {
        declaration
            .declarations
            .iter()
            .map(|declarator| FlowBinding {
                lexical: !declaration.kind.is_var(),
                pattern: self.lower_pattern(&declarator.id),
                value: declarator.init.as_ref().map_or_else(
                    || self.unsupported_expression("binding_without_initializer", declarator.span),
                    |expression| self.lower_expression(expression),
                ),
                span: self.span(declarator.span),
            })
            .collect()
    }

    fn lower_statements(&self, statements: &[Statement<'_>]) -> Vec<FlowStatement> {
        let mut lowered = Vec::new();
        for statement in statements {
            match statement {
                Statement::TSEnumDeclaration(declaration) => {
                    lowered.push(FlowStatement::Bind(self.lower_enum(declaration)));
                }
                Statement::VariableDeclaration(declaration) => lowered.extend(
                    self.lower_bindings(declaration)
                        .into_iter()
                        .map(FlowStatement::Bind),
                ),
                Statement::FunctionDeclaration(function) => {
                    if let (Some(identifier), Some(body)) = (&function.id, &function.body) {
                        lowered.push(FlowStatement::Bind(FlowBinding {
                            lexical: true,
                            pattern: FlowPattern {
                                kind: FlowPatternKind::Identifier {
                                    name: identifier.name.to_string(),
                                },
                                span: self.span(identifier.span),
                            },
                            value: FlowExpression {
                                kind: FlowExpressionKind::Arrow {
                                    params: self.lower_params(&function.params),
                                    body: FlowArrowBody::Statements {
                                        statements: self.lower_statements(&body.statements),
                                    },
                                },
                                span: self.span(function.span),
                            },
                            span: self.span(function.span),
                        }));
                    } else {
                        lowered.push(FlowStatement::Unsupported(
                            self.unsupported("unsupported_local_function", function.span),
                        ));
                    }
                }
                Statement::ReturnStatement(statement) => lowered.push(FlowStatement::Return {
                    value: statement
                        .argument
                        .as_ref()
                        .map(|expression| self.lower_expression(expression)),
                    span: self.span(statement.span),
                }),
                Statement::ExpressionStatement(statement) => {
                    if let Expression::AssignmentExpression(assignment) = &statement.expression
                        && assignment.operator == AssignmentOperator::Assign
                    {
                        lowered.push(FlowStatement::Assign {
                            target: self.lower_assignment_target(&assignment.left),
                            value: self.lower_expression(&assignment.right),
                            span: self.span(statement.span),
                        });
                    } else {
                        lowered.push(FlowStatement::Expression {
                            value: self.lower_expression(&statement.expression),
                            span: self.span(statement.span),
                        });
                    }
                }
                Statement::IfStatement(statement) => lowered.push(FlowStatement::If {
                    test: self.lower_expression(&statement.test),
                    consequent: self.lower_branch(&statement.consequent),
                    alternate: statement
                        .alternate
                        .as_ref()
                        .map_or_else(Vec::new, |alternate| self.lower_branch(alternate)),
                    span: self.span(statement.span),
                }),
                Statement::ForOfStatement(statement) => lowered.push(FlowStatement::If {
                    test: self.unsupported_expression("symbolic_for_of_iteration", statement.span),
                    consequent: self.lower_branch(&statement.body),
                    alternate: Vec::new(),
                    span: self.span(statement.span),
                }),
                // A block keeps its own scope, like a branch that always runs.
                Statement::BlockStatement(block) => {
                    lowered.push(self.always(self.lower_statements(&block.body), block.span));
                }
                // Switch cases and loop bodies are explored independently, so jumps end nothing.
                Statement::BreakStatement(_)
                | Statement::ContinueStatement(_)
                | Statement::EmptyStatement(_)
                | Statement::DebuggerStatement(_) => {}
                Statement::LabeledStatement(statement) => {
                    lowered.extend(self.lower_branch(&statement.body));
                }
                Statement::ThrowStatement(statement) => lowered.push(FlowStatement::Throw {
                    value: self.lower_expression(&statement.argument),
                    span: self.span(statement.span),
                }),
                Statement::TryStatement(statement) => {
                    let block = self.lower_statements(&statement.block.body);
                    match &statement.handler {
                        // The handler runs only if the block throws, which is not known.
                        Some(handler) => {
                            let mut caught = Vec::new();
                            if let Some(parameter) = &handler.param {
                                caught.push(FlowStatement::Bind(FlowBinding {
                                    lexical: true,
                                    pattern: self.lower_pattern(&parameter.pattern),
                                    value: self
                                        .unsupported_expression("caught_exception", handler.span),
                                    span: self.span(handler.span),
                                }));
                            }
                            caught.extend(self.lower_statements(&handler.body.body));
                            lowered.push(FlowStatement::If {
                                test: self.unsupported_expression("try_outcome", statement.span),
                                consequent: block,
                                alternate: caught,
                                span: self.span(statement.span),
                            });
                        }
                        None => lowered.push(self.always(block, statement.span)),
                    }
                    if let Some(finalizer) = &statement.finalizer {
                        lowered.push(
                            self.always(self.lower_statements(&finalizer.body), finalizer.span),
                        );
                    }
                }
                // Loops are explored for zero or one iteration, like `for...of`.
                Statement::ForStatement(statement) => {
                    if let Some(oxc::ast::ast::ForStatementInit::VariableDeclaration(declaration)) =
                        &statement.init
                    {
                        lowered.extend(
                            self.lower_bindings(declaration)
                                .into_iter()
                                .map(FlowStatement::Bind),
                        );
                    }
                    lowered.push(self.maybe(self.lower_branch(&statement.body), statement.span));
                }
                Statement::ForInStatement(statement) => {
                    lowered.push(self.maybe(self.lower_branch(&statement.body), statement.span));
                }
                Statement::WhileStatement(statement) => {
                    lowered.push(self.maybe(self.lower_branch(&statement.body), statement.span));
                }
                Statement::DoWhileStatement(statement) => {
                    lowered.push(self.always(self.lower_branch(&statement.body), statement.span));
                }
                Statement::SwitchStatement(statement) => {
                    lowered.push(FlowStatement::Unsupported(
                        self.unsupported("switch_fallthrough_not_modeled", statement.span),
                    ));
                    for case in &statement.cases {
                        let test = case.test.as_ref().map_or_else(
                            || self.unsupported_expression("symbolic_switch_default", case.span),
                            |case_value| FlowExpression {
                                kind: FlowExpressionKind::StrictEquality {
                                    left: Box::new(self.lower_expression(&statement.discriminant)),
                                    right: Box::new(self.lower_expression(case_value)),
                                    negated: false,
                                },
                                span: self.span(case.span),
                            },
                        );
                        lowered.push(FlowStatement::If {
                            test,
                            consequent: self.lower_statements(&case.consequent),
                            alternate: Vec::new(),
                            span: self.span(case.span),
                        });
                    }
                }
                _ => lowered.push(FlowStatement::Unsupported(
                    self.unsupported("unsupported_function_statement", statement.span()),
                )),
            }
        }
        lowered
    }

    /// Statements that always run, in their own scope.
    fn always(&self, statements: Vec<FlowStatement>, span: Span) -> FlowStatement {
        FlowStatement::If {
            test: FlowExpression {
                kind: FlowExpressionKind::Boolean { value: true },
                span: self.span(span),
            },
            consequent: statements,
            alternate: Vec::new(),
            span: self.span(span),
        }
    }

    /// Statements that may or may not run, such as a loop body.
    fn maybe(&self, statements: Vec<FlowStatement>, span: Span) -> FlowStatement {
        FlowStatement::If {
            test: self.unsupported_expression("symbolic_loop_iteration", span),
            consequent: statements,
            alternate: Vec::new(),
            span: self.span(span),
        }
    }

    fn lower_branch(&self, statement: &Statement<'_>) -> Vec<FlowStatement> {
        match statement {
            Statement::BlockStatement(block) => self.lower_statements(&block.body),
            _ => self.lower_statements(std::slice::from_ref(statement)),
        }
    }

    fn lower_assignment_target(
        &self,
        target: &oxc::ast::ast::AssignmentTarget<'_>,
    ) -> FlowAssignmentTarget {
        let Some(target) = target.as_simple_assignment_target() else {
            return FlowAssignmentTarget::Unsupported {
                syntax: "destructuring_assignment".to_owned(),
                span: self.span(target.span()),
            };
        };
        match target {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                FlowAssignmentTarget::Identifier {
                    name: identifier.name.to_string(),
                }
            }
            SimpleAssignmentTarget::StaticMemberExpression(member) => {
                FlowAssignmentTarget::StaticMember {
                    object: self.lower_expression(&member.object),
                    property: member.property.name.to_string(),
                }
            }
            SimpleAssignmentTarget::ComputedMemberExpression(member) => {
                FlowAssignmentTarget::ComputedMember {
                    object: self.lower_expression(&member.object),
                    property: self.lower_expression(&member.expression),
                }
            }
            _ => FlowAssignmentTarget::Unsupported {
                syntax: "unsupported_assignment_target".to_owned(),
                span: self.span(target.span()),
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_expression(&self, expression: &Expression<'_>) -> FlowExpression {
        let span = self.span(expression.span());
        let kind = match expression {
            Expression::NullLiteral(_) => FlowExpressionKind::Null,
            Expression::StringLiteral(literal) => FlowExpressionKind::String {
                value: literal.value.to_string(),
            },
            Expression::NumericLiteral(literal)
                if literal.value.fract() == 0.0
                    && literal.value >= i64::MIN as f64
                    && literal.value <= i64::MAX as f64 =>
            {
                FlowExpressionKind::Number {
                    value: literal.value as i64,
                }
            }
            Expression::BooleanLiteral(literal) => FlowExpressionKind::Boolean {
                value: literal.value,
            },
            Expression::Identifier(identifier) => FlowExpressionKind::Identifier {
                name: identifier.name.to_string(),
                module_binding: self.is_module_reference(identifier),
            },
            Expression::ObjectExpression(object) => {
                let mut fields = Vec::with_capacity(object.properties.len());
                for property in &object.properties {
                    let property = match property {
                        ObjectPropertyKind::ObjectProperty(property) => property,
                        ObjectPropertyKind::SpreadProperty(spread) => {
                            fields.push(FlowRecordField {
                                property: "...".to_owned(),
                                value: self.lower_expression(&spread.argument),
                                span: self.span(spread.span),
                                spread: true,
                            });
                            continue;
                        }
                    };
                    let Some(name) = property.key.static_name() else {
                        return self
                            .unsupported_expression("computed_object_key", expression.span());
                    };
                    fields.push(FlowRecordField {
                        property: name.into_owned(),
                        value: self.lower_expression(&property.value),
                        span: self.span(property.span),
                        spread: false,
                    });
                }
                FlowExpressionKind::Record { fields }
            }
            Expression::ArrayExpression(array) => {
                let mut elements = Vec::with_capacity(array.elements.len());
                for element in &array.elements {
                    if let ArrayExpressionElement::SpreadElement(spread) = element {
                        elements.push(FlowExpression {
                            kind: FlowExpressionKind::Spread {
                                value: Box::new(self.lower_expression(&spread.argument)),
                            },
                            span: self.span(spread.span),
                        });
                        continue;
                    }
                    let Some(element) = element.as_expression() else {
                        return self
                            .unsupported_expression("array_spread_or_elision", expression.span());
                    };
                    elements.push(self.lower_expression(element));
                }
                FlowExpressionKind::Array { elements }
            }
            Expression::StaticMemberExpression(member) => return self.lower_static_member(member),
            Expression::ComputedMemberExpression(member) => FlowExpressionKind::ComputedMember {
                object: Box::new(self.lower_expression(&member.object)),
                property: Box::new(self.lower_expression(&member.expression)),
            },
            Expression::CallExpression(call) => return self.lower_call(call),
            // A set built from an iterable holds the iterable's members, so membership reads
            // like the iterable's; `set.has(value)` is evaluated like `includes`.
            Expression::NewExpression(new)
                if matches!(&new.callee, Expression::Identifier(identifier)
                    if identifier.name == "Set" && self.is_global_reference(identifier))
                    && new.arguments.len() <= 1 =>
            {
                return match new.arguments.first() {
                    None => FlowExpression {
                        kind: FlowExpressionKind::Array {
                            elements: Vec::new(),
                        },
                        span,
                    },
                    Some(Argument::SpreadElement(spread)) => {
                        self.unsupported_expression("spread_call_argument", spread.span)
                    }
                    Some(argument) => self.lower_expression(argument.to_expression()),
                };
            }
            // `a?.b` and `f?.(x)` read and call like `a.b` and `f(x)`; on a missing value the read
            // or call is unknown, as without the guard.
            Expression::ChainExpression(chain) => {
                return match &chain.expression {
                    ChainElement::CallExpression(call) => self.lower_call(call),
                    ChainElement::StaticMemberExpression(member) => {
                        self.lower_static_member(member)
                    }
                    ChainElement::ComputedMemberExpression(member) => FlowExpression {
                        kind: FlowExpressionKind::ComputedMember {
                            object: Box::new(self.lower_expression(&member.object)),
                            property: Box::new(self.lower_expression(&member.expression)),
                        },
                        span,
                    },
                    ChainElement::TSNonNullExpression(assertion) => {
                        self.lower_expression(&assertion.expression)
                    }
                    ChainElement::PrivateFieldExpression(_) => FlowExpression {
                        kind: FlowExpressionKind::Unsupported {
                            syntax: "unsupported_expression".to_owned(),
                            references: referenced_names(expression),
                        },
                        span,
                    },
                };
            }
            Expression::ImportExpression(import) => match &import.source {
                Expression::StringLiteral(source) => FlowExpressionKind::DynamicImport {
                    module: source.value.to_string(),
                },
                _ => FlowExpressionKind::Unsupported {
                    syntax: "nonliteral_dynamic_import".to_owned(),
                    references: referenced_names(&import.source),
                },
            },
            Expression::BinaryExpression(binary)
                if matches!(
                    binary.operator,
                    BinaryOperator::StrictEquality | BinaryOperator::StrictInequality
                ) =>
            {
                FlowExpressionKind::StrictEquality {
                    left: Box::new(self.lower_expression(&binary.left)),
                    right: Box::new(self.lower_expression(&binary.right)),
                    negated: binary.operator == BinaryOperator::StrictInequality,
                }
            }
            Expression::BinaryExpression(binary)
                if matches!(
                    binary.operator,
                    BinaryOperator::Equality | BinaryOperator::Inequality
                ) && (matches!(&binary.left, Expression::NullLiteral(_))
                    || matches!(&binary.right, Expression::NullLiteral(_))) =>
            {
                let value = if matches!(&binary.left, Expression::NullLiteral(_)) {
                    &binary.right
                } else {
                    &binary.left
                };
                FlowExpressionKind::LooseNullEquality {
                    value: Box::new(self.lower_expression(value)),
                    negated: binary.operator == BinaryOperator::Inequality,
                }
            }
            Expression::LogicalExpression(logical) => FlowExpressionKind::Logical {
                left: Box::new(self.lower_expression(&logical.left)),
                right: Box::new(self.lower_expression(&logical.right)),
                operator: match logical.operator {
                    LogicalOperator::And => FlowLogicalOperator::And,
                    LogicalOperator::Or => FlowLogicalOperator::Or,
                    LogicalOperator::Coalesce => FlowLogicalOperator::Coalesce,
                },
            },
            Expression::UnaryExpression(unary) if unary.operator == UnaryOperator::LogicalNot => {
                FlowExpressionKind::LogicalNot {
                    value: Box::new(self.lower_expression(&unary.argument)),
                }
            }
            Expression::ConditionalExpression(conditional) => FlowExpressionKind::Conditional {
                test: Box::new(self.lower_expression(&conditional.test)),
                consequent: Box::new(self.lower_expression(&conditional.consequent)),
                alternate: Box::new(self.lower_expression(&conditional.alternate)),
            },
            Expression::ArrowFunctionExpression(function) => FlowExpressionKind::Arrow {
                params: self.lower_params(&function.params),
                body: match &function.body {
                    ArrowFunctionBody::FunctionBody(body) => FlowArrowBody::Statements {
                        statements: self.lower_statements(&body.statements),
                    },
                    body => FlowArrowBody::Expression {
                        expression: Box::new(self.lower_expression(body.to_expression())),
                    },
                },
            },
            Expression::FunctionExpression(function) => {
                let body = function
                    .body
                    .as_ref()
                    .map_or_else(Vec::new, |body| self.lower_statements(&body.statements));
                FlowExpressionKind::Arrow {
                    params: self.lower_params(&function.params),
                    body: FlowArrowBody::Statements { statements: body },
                }
            }
            Expression::JSXElement(element) => return self.lower_jsx_element(element),
            Expression::JSXFragment(fragment) => FlowExpressionKind::Array {
                elements: fragment
                    .children
                    .iter()
                    .filter_map(|child| self.lower_jsx_child(child))
                    .collect(),
            },
            Expression::ParenthesizedExpression(parenthesized) => {
                return self.lower_expression(&parenthesized.expression);
            }
            Expression::TSAsExpression(assertion) => {
                return self.lower_expression(&assertion.expression);
            }
            Expression::TSSatisfiesExpression(assertion) => {
                return self.lower_expression(&assertion.expression);
            }
            Expression::TSTypeAssertion(assertion) => {
                return self.lower_expression(&assertion.expression);
            }
            Expression::TSNonNullExpression(assertion) => {
                return self.lower_expression(&assertion.expression);
            }
            _ => FlowExpressionKind::Unsupported {
                syntax: "unsupported_expression".to_owned(),
                references: referenced_names(expression),
            },
        };
        FlowExpression { kind, span }
    }

    fn lower_static_member(&self, member: &StaticMemberExpression<'_>) -> FlowExpression {
        let span = self.span(member.span);
        if !(matches!(&member.object, Expression::ThisExpression(_))
            && self.current_class.is_some())
        {
            return FlowExpression {
                kind: FlowExpressionKind::StaticMember {
                    object: Box::new(self.lower_expression(&member.object)),
                    property: member.property.name.to_string(),
                },
                span,
            };
        }
        let property = member.property.name.as_str();
        let name = if property == "props" {
            "props".to_owned()
        } else {
            format!(
                "{}.{}",
                self.current_class.as_deref().unwrap_or_default(),
                property
            )
        };
        // A method read as a value is bound to the instance, so calling it later passes
        // the instance's props like a direct `this.method()` call.
        if let Some(arity) = self.class_members.get(property).copied() {
            let span = self.span(member.span);
            let identifier = |name: String| FlowExpression {
                kind: FlowExpressionKind::Identifier {
                    name,
                    module_binding: false,
                },
                span: span.clone(),
            };
            let parameters = (0..arity).map(|index| format!("__bound_argument_{index}"));
            return FlowExpression {
                kind: FlowExpressionKind::Arrow {
                    params: parameters
                        .clone()
                        .map(|name| FlowPattern {
                            kind: FlowPatternKind::Identifier { name },
                            span: span.clone(),
                        })
                        .collect(),
                    body: FlowArrowBody::Expression {
                        expression: Box::new(FlowExpression {
                            kind: FlowExpressionKind::Call {
                                callee: Box::new(identifier(name)),
                                arguments: std::iter::once("props".to_owned())
                                    .chain(parameters)
                                    .map(identifier)
                                    .collect(),
                            },
                            span: span.clone(),
                        }),
                    },
                },
                span,
            };
        }
        FlowExpression {
            kind: FlowExpressionKind::Identifier {
                name,
                module_binding: false,
            },
            span: self.span(member.span),
        }
    }

    fn lower_call(&self, call: &CallExpression<'_>) -> FlowExpression {
        let mut arguments: Vec<_> = call
            .arguments
            .iter()
            .map(|argument| match argument {
                Argument::SpreadElement(spread) => {
                    self.unsupported_expression("spread_call_argument", spread.span)
                }
                _ => self.lower_expression(argument.to_expression()),
            })
            .collect();
        if let Expression::StaticMemberExpression(member) = &call.callee
            && matches!(&member.object, Expression::ThisExpression(_))
            && let Some(class) = &self.current_class
        {
            arguments.insert(
                0,
                FlowExpression {
                    kind: FlowExpressionKind::Identifier {
                        name: "props".to_owned(),
                        module_binding: false,
                    },
                    span: self.span(call.span),
                },
            );
            return FlowExpression {
                kind: FlowExpressionKind::Call {
                    callee: Box::new(FlowExpression {
                        kind: FlowExpressionKind::Identifier {
                            name: format!("{class}.{}", member.property.name),
                            module_binding: false,
                        },
                        span: self.span(member.span),
                    }),
                    arguments,
                },
                span: self.span(call.span),
            };
        }
        FlowExpression {
            kind: FlowExpressionKind::Call {
                callee: Box::new(self.lower_expression(&call.callee)),
                arguments,
            },
            span: self.span(call.span),
        }
    }

    fn lower_jsx_element(&self, element: &JSXElement<'_>) -> FlowExpression {
        let tag = match &element.opening_element.name {
            JSXElementName::Identifier(identifier) => FlowJsxTag::Identifier {
                name: identifier.name.to_string(),
                module_binding: false,
                intrinsic: identifier
                    .name
                    .chars()
                    .next()
                    .is_some_and(char::is_lowercase),
            },
            JSXElementName::IdentifierReference(identifier) => FlowJsxTag::Identifier {
                name: identifier.name.to_string(),
                intrinsic: false,
                module_binding: self.is_module_reference(identifier),
            },
            JSXElementName::MemberExpression(member)
                if member.property.name == "Fragment"
                    && matches!(&member.object, oxc::ast::ast::JSXMemberExpressionObject::IdentifierReference(identifier)
                        if self.output.imports.iter().any(|import|
                            import.local == identifier.name.as_str() && import.imported == "*" && import.module == "react")) =>
            {
                FlowJsxTag::Identifier {
                    name: "fragment".to_owned(),
                    intrinsic: true,
                    module_binding: false,
                }
            }
            JSXElementName::MemberExpression(member) => match &member.object {
                oxc::ast::ast::JSXMemberExpressionObject::IdentifierReference(identifier) => {
                    FlowJsxTag::Member {
                        object: identifier.name.to_string(),
                        property: member.property.name.to_string(),
                        module_binding: self.is_module_reference(identifier),
                    }
                }
                _ => FlowJsxTag::Unsupported {
                    syntax: "nested_jsx_member_tag".to_owned(),
                },
            },
            _ => FlowJsxTag::Unsupported {
                syntax: "unsupported_jsx_tag".to_owned(),
            },
        };
        let mut props = element
            .opening_element
            .attributes
            .iter()
            .map(|attribute| match attribute {
                JSXAttributeItem::SpreadAttribute(spread) => FlowJsxProp::Spread {
                    value: self.lower_expression(&spread.argument),
                    span: self.span(spread.span),
                },
                JSXAttributeItem::Attribute(attribute) => {
                    let name = attribute.name.get_identifier().name.to_string();
                    let value = match &attribute.value {
                        Some(JSXAttributeValue::StringLiteral(literal)) => FlowExpression {
                            kind: FlowExpressionKind::String {
                                value: literal.value.to_string(),
                            },
                            span: self.span(literal.span),
                        },
                        Some(JSXAttributeValue::ExpressionContainer(container)) => {
                            match &container.expression {
                                JSXExpression::EmptyExpression(empty) => {
                                    self.unsupported_expression("empty_jsx_expression", empty.span)
                                }
                                expression => self.lower_expression(expression.to_expression()),
                            }
                        }
                        Some(JSXAttributeValue::Element(element)) => {
                            self.lower_jsx_element(element)
                        }
                        _ => self.unsupported_expression(
                            "unsupported_jsx_attribute_value",
                            attribute.span,
                        ),
                    };
                    FlowJsxProp::Property {
                        name,
                        value,
                        span: self.span(attribute.span),
                    }
                }
            })
            .collect::<Vec<_>>();
        let mut children = element
            .children
            .iter()
            .filter_map(|child| self.lower_jsx_child(child))
            .collect::<Vec<_>>();
        if !children.is_empty() {
            // React passes a single child as is and several as an array, so a function child
            // can be called.
            let value = if children.len() == 1 {
                children.pop().expect("one child")
            } else {
                FlowExpression {
                    kind: FlowExpressionKind::Array { elements: children },
                    span: self.span(element.span),
                }
            };
            props.push(FlowJsxProp::Property {
                name: "children".to_owned(),
                value,
                span: self.span(element.span),
            });
        }
        FlowExpression {
            kind: FlowExpressionKind::JsxElement { tag, props },
            span: self.span(element.span),
        }
    }

    fn lower_jsx_child(&self, child: &JSXChild<'_>) -> Option<FlowExpression> {
        match child {
            JSXChild::Text(text) => clean_jsx_text(&text.value).map(|value| FlowExpression {
                kind: FlowExpressionKind::String { value },
                span: self.span(text.span),
            }),
            JSXChild::Element(element) => Some(self.lower_jsx_element(element)),
            JSXChild::Fragment(fragment) => Some(FlowExpression {
                kind: FlowExpressionKind::Array {
                    elements: fragment
                        .children
                        .iter()
                        .filter_map(|child| self.lower_jsx_child(child))
                        .collect(),
                },
                span: self.span(fragment.span),
            }),
            JSXChild::ExpressionContainer(container) => match &container.expression {
                JSXExpression::EmptyExpression(_) => None,
                expression => Some(self.lower_expression(expression.to_expression())),
            },
            JSXChild::Spread(spread) => {
                Some(self.unsupported_expression("jsx_spread_child", spread.span))
            }
        }
    }

    fn unsupported_expression(&self, syntax: &str, span: Span) -> FlowExpression {
        FlowExpression {
            kind: FlowExpressionKind::Unsupported {
                syntax: syntax.to_owned(),
                references: Vec::new(),
            },
            span: self.span(span),
        }
    }
}

/// Applies the JSX whitespace rule: lines are trimmed where they meet a line break, blank lines
/// are dropped, and the rest are joined with spaces. Text that is only indentation is not a child.
fn clean_jsx_text(text: &str) -> Option<String> {
    let lines = text.lines().collect::<Vec<_>>();
    let last_line = lines.len().saturating_sub(1);
    let pieces = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = if index == 0 { line } else { line.trim_start() };
            let line = if index == last_line && !text.ends_with('\n') {
                line
            } else {
                line.trim_end()
            };
            (!line.is_empty()).then_some(line)
        })
        .collect::<Vec<_>>();
    (!pieces.is_empty()).then(|| pieces.join(" "))
}
