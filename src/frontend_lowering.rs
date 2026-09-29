use oxc::{
    ast::ast::{
        Argument, ArrowFunctionBody, BindingPattern, Declaration, Expression, FormalParameters,
        Function, ImportDeclaration, ImportDeclarationSpecifier, ImportOrExportKind,
        JSXAttributeItem, JSXAttributeValue, JSXElement, JSXElementName, JSXExpression,
        ObjectPropertyKind, Statement, VariableDeclaration,
    },
    span::{GetSpan, Span},
};

use crate::{
    ids::FileId,
    ir::{
        FlowArrowBody, FlowBinding, FlowExpression, FlowExpressionKind, FlowFileIr, FlowFunction,
        FlowImport, FlowJsxProp, FlowJsxTag, FlowPattern, FlowPatternField, FlowPatternKind,
        FlowRecordField, FlowStatement, SourceSpan, UnsupportedIr,
    },
};

pub fn lower(file_id: FileId, program: &oxc::ast::ast::Program<'_>) -> FlowFileIr {
    let mut lowerer = Lowerer {
        file_id,
        output: FlowFileIr::default(),
    };
    for statement in &program.body {
        lowerer.lower_top_level(statement);
    }
    lowerer.output
}

struct Lowerer {
    file_id: FileId,
    output: FlowFileIr,
}

impl Lowerer {
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
            Statement::ExportDeclaration(export) => {
                self.lower_declaration(&export.declaration);
            }
            Statement::TSEnumDeclaration(_) => {}
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
            Declaration::TSEnumDeclaration(_) => {}
            _ => self
                .output
                .unsupported
                .push(self.unsupported("unsupported_exported_declaration", declaration.span())),
        }
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

    fn lower_params(&self, params: &FormalParameters<'_>) -> Vec<FlowPattern> {
        params
            .items
            .iter()
            .map(|parameter| self.lower_pattern(&parameter.pattern))
            .collect()
    }

    fn lower_pattern(&self, pattern: &BindingPattern<'_>) -> FlowPattern {
        let kind = match pattern {
            BindingPattern::BindingIdentifier(identifier) => FlowPatternKind::Identifier {
                name: identifier.name.to_string(),
            },
            BindingPattern::ObjectPattern(object) if object.rest.is_none() => {
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
                FlowPatternKind::Object { fields }
            }
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
                Statement::VariableDeclaration(declaration) => lowered.extend(
                    self.lower_bindings(declaration)
                        .into_iter()
                        .map(FlowStatement::Bind),
                ),
                Statement::ReturnStatement(statement) => lowered.push(FlowStatement::Return {
                    value: statement
                        .argument
                        .as_ref()
                        .map(|expression| self.lower_expression(expression)),
                    span: self.span(statement.span),
                }),
                Statement::ExpressionStatement(statement) => {
                    lowered.push(FlowStatement::Expression {
                        value: self.lower_expression(&statement.expression),
                        span: self.span(statement.span),
                    });
                }
                _ => lowered.push(FlowStatement::Unsupported(
                    self.unsupported("unsupported_function_statement", statement.span()),
                )),
            }
        }
        lowered
    }

    fn lower_expression(&self, expression: &Expression<'_>) -> FlowExpression {
        let span = self.span(expression.span());
        let kind = match expression {
            Expression::StringLiteral(literal) => FlowExpressionKind::String {
                value: literal.value.to_string(),
            },
            Expression::Identifier(identifier) => FlowExpressionKind::Identifier {
                name: identifier.name.to_string(),
            },
            Expression::ObjectExpression(object) => {
                let mut fields = Vec::with_capacity(object.properties.len());
                for property in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        return self.unsupported_expression("object_spread", expression.span());
                    };
                    let Some(name) = property.key.static_name() else {
                        return self
                            .unsupported_expression("computed_object_key", expression.span());
                    };
                    fields.push(FlowRecordField {
                        property: name.into_owned(),
                        value: self.lower_expression(&property.value),
                        span: self.span(property.span),
                    });
                }
                FlowExpressionKind::Record { fields }
            }
            Expression::ArrayExpression(array) => {
                let mut elements = Vec::with_capacity(array.elements.len());
                for element in &array.elements {
                    let Some(element) = element.as_expression() else {
                        return self
                            .unsupported_expression("array_spread_or_elision", expression.span());
                    };
                    elements.push(self.lower_expression(element));
                }
                FlowExpressionKind::Array { elements }
            }
            Expression::StaticMemberExpression(member) => FlowExpressionKind::StaticMember {
                object: Box::new(self.lower_expression(&member.object)),
                property: member.property.name.to_string(),
            },
            Expression::ComputedMemberExpression(member) => FlowExpressionKind::ComputedMember {
                object: Box::new(self.lower_expression(&member.object)),
                property: Box::new(self.lower_expression(&member.expression)),
            },
            Expression::CallExpression(call) => {
                let arguments = call
                    .arguments
                    .iter()
                    .map(|argument| match argument {
                        Argument::SpreadElement(spread) => {
                            self.unsupported_expression("spread_call_argument", spread.span)
                        }
                        _ => self.lower_expression(argument.to_expression()),
                    })
                    .collect();
                FlowExpressionKind::Call {
                    callee: Box::new(self.lower_expression(&call.callee)),
                    arguments,
                }
            }
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
            Expression::JSXElement(element) => return self.lower_jsx_element(element),
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
            },
        };
        FlowExpression { kind, span }
    }

    fn lower_jsx_element(&self, element: &JSXElement<'_>) -> FlowExpression {
        let tag = match &element.opening_element.name {
            JSXElementName::Identifier(identifier) => FlowJsxTag::Identifier {
                name: identifier.name.to_string(),
                intrinsic: identifier
                    .name
                    .chars()
                    .next()
                    .is_some_and(char::is_lowercase),
            },
            JSXElementName::IdentifierReference(identifier) => FlowJsxTag::Identifier {
                name: identifier.name.to_string(),
                intrinsic: false,
            },
            _ => FlowJsxTag::Unsupported {
                syntax: "unsupported_jsx_tag".to_owned(),
            },
        };
        let props = element
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
            .collect();
        FlowExpression {
            kind: FlowExpressionKind::JsxElement { tag, props },
            span: self.span(element.span),
        }
    }

    fn unsupported_expression(&self, syntax: &str, span: Span) -> FlowExpression {
        FlowExpression {
            kind: FlowExpressionKind::Unsupported {
                syntax: syntax.to_owned(),
            },
            span: self.span(span),
        }
    }
}
