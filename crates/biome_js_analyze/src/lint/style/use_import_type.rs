use crate::{
    JsRuleAction,
    react::{ReactLibrary, is_global_react_import},
    services::semantic::Semantic,
};
use biome_analyze::{
    FixKind, Rule, RuleDiagnostic, RuleSource, RuleSourceKind, context::RuleContext,
    declare_lint_rule, options::JsxRuntime,
};
use biome_console::markup;
use biome_diagnostics::Severity;
use biome_js_factory::make;
use biome_js_semantic::{ReferencesExtensions, SemanticModel};
use biome_js_syntax::{
    AnyJsCombinedSpecifier, AnyJsIdentifierUsage, AnyJsImportClause, AnyJsModuleItem,
    AnyJsNamedImportSpecifier, JsFileSource, JsIdentifierBinding, JsImport, JsImportCombinedClause,
    JsImportDefaultClause, JsLanguage, JsModuleItemList, JsNamedImportSpecifierList,
    JsNamedImportSpecifiers, JsSyntaxNode, JsSyntaxToken, T,
};
use biome_rowan::{
    AstNode, AstSeparatedList, BatchMutation, BatchMutationExt, SyntaxElement, SyntaxResult,
    TriviaPieceKind, chain_trivia_pieces, trim_leading_trivia_pieces, trim_trailing_trivia_pieces,
};
use rustc_hash::FxHashSet;

declare_lint_rule! {
    /// Promotes the use of `import type` for types.
    ///
    /// _TypeScript_ allows specifying a `type` keyword on an `import` to indicate that the `import` doesn't exist at runtime.
    /// This allows compilers to safely drop imports of types without looking for their definition.
    /// This also ensures that some modules are not loaded at runtime.
    ///
    /// The rule ensures that all imports used only as a type use a type-only `import`.
    /// It also groups inline type imports into a grouped `import type`.
    ///
    /// If you use the TypeScript Compiler (TSC) to compile your code into JavaScript,
    /// then you can disable this rule, as TSC can remove imports only used as types.
    /// However, for consistency and compatibility with other compilers, you may want to enable this rule.
    /// In that case we recommend to enable TSC's [`verbatimModuleSyntax`](https://www.typescriptlang.org/tsconfig/#verbatimModuleSyntax).
    /// This configuration ensures that TSC preserves imports not marked with the `type` keyword.
    ///
    /// You may also want to enable the editor setting [`typescript.preferences.preferTypeOnlyAutoImports`](https://devblogs.microsoft.com/typescript/announcing-typescript-5-3-rc/#settings-to-prefer-type-auto-imports) from the TypeScript LSP.
    /// This setting is available in Visual Studio Code.
    /// It ensures the `type` is used when the editor automatically imports a type.
    ///
    /// ## Caveat with TypeScript experimental decorators
    ///
    /// Some frameworks like Angular and NestJS rely on
    /// [experimental TypeScript decorators](https://www.typescriptlang.org/docs/handbook/decorators.html)
    /// which allow code to be generated based on type annotations.
    /// This is mainly used for dependency injection.
    ///
    /// Since Biome doesn't know how a decorator is implemented,
    /// it is unable to detect that an import used as a type is also used as a value in the code generated by a decorator.
    /// This leads Biome to suggest importing some imports as type, which are actually used as value at runtime.
    ///
    /// We haven't found a way to support this pattern yet.
    /// We recommend disabling this rule when using such decorators.
    ///
    /// ## Options
    ///
    /// This rule respects the [`jsxRuntime`](https://biomejs.dev/reference/configuration/#javascriptjsxruntime)
    /// setting and will make an exception for React globals if it is set to
    /// `"reactClassic"`.
    ///
    /// ## Examples
    ///
    /// ### Invalid
    ///
    /// ```ts,expect_diagnostic
    /// import { A } from "./mod.js";
    /// type TypeOfA = typeof A;
    /// let a: A;
    /// ```
    ///
    /// ```ts,expect_diagnostic
    /// import { type A, type B } from "./mod.js";
    /// ```
    ///
    /// ```ts,expect_diagnostic
    /// import { type A, B } from "./mod.js";
    /// let c: A;
    /// let d: typeof B;
    /// ```
    ///
    /// ### Valid
    ///
    /// ```ts
    /// import type { A } from "./mod.js";
    /// let a: A;
    /// ```
    ///
    /// ```ts
    /// import { B } from "./mod.js";
    /// let a: B = new B();
    /// ```
    ///
    /// ```ts
    /// import { type A, B } from "./mod.js";
    /// let c: A;
    /// let d = new B();
    /// ```
    ///
    /// The rule ignores unused imports and imports with import attributes.
    ///
    /// ```ts
    /// import { A } from "./mod.js";
    ///
    /// import { B } from "./mod.js" with {};
    /// export type { B };
    /// ```
    pub UseImportType {
        version: "1.5.0",
        name: "useImportType",
        language: "ts",
        sources: &[RuleSource::EslintTypeScript("consistent-type-imports")],
        source_kind: RuleSourceKind::Inspired,
        recommended: false,
        severity: Severity::Warning,
        fix_kind: FixKind::Safe,
    }
}

impl Rule for UseImportType {
    type Query = Semantic<JsImport>;
    type State = ImportTypeFix;
    type Signals = Option<Self::State>;
    type Options = ();

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let source_type = ctx.source_type::<JsFileSource>();
        if !source_type.language().is_typescript() || source_type.language().is_definition_file() {
            return None;
        }
        let import = ctx.query();
        let import_clause = import.import_clause().ok()?;
        // Import attributes and type-only imports are not compatible.
        if import_clause.attribute().is_some() {
            return None;
        }
        let model = ctx.model();
        match import_clause {
            AnyJsImportClause::JsImportBareClause(_) => None,
            AnyJsImportClause::JsImportCombinedClause(clause) => {
                let default_binding = clause.default_specifier().ok()?.local_name().ok()?;
                let default_binding = default_binding.as_js_identifier_binding()?;
                let is_default_used_as_type = if ctx.jsx_runtime() == JsxRuntime::ReactClassic
                    && is_global_react_import(default_binding, ReactLibrary::React)
                {
                    false
                } else {
                    is_only_used_as_type(model, default_binding)
                };
                match clause.specifier().ok()? {
                    AnyJsCombinedSpecifier::JsNamedImportSpecifiers(named_specifiers) => {
                        match named_import_type_fix(model, &named_specifiers, false) {
                            Some(NamedImportTypeFix::UseImportType(specifiers)) => {
                                if is_default_used_as_type {
                                    Some(ImportTypeFix::UseImportType)
                                } else if specifiers.is_empty() {
                                    // Don't group inline type-imports,
                                    // when the default import is not only used as a type.
                                    None
                                } else {
                                    // Prefer adding type keyword instead of
                                    // splitting the import statement into two import statements
                                    Some(ImportTypeFix::AddInlineTypeQualifiers(specifiers))
                                }
                            }
                            Some(NamedImportTypeFix::AddInlineTypeQualifiers(specifiers)) => {
                                if is_default_used_as_type {
                                    Some(ImportTypeFix::ExtractDefaultImportType(specifiers))
                                } else if specifiers.is_empty() {
                                    None
                                } else {
                                    Some(ImportTypeFix::AddInlineTypeQualifiers(specifiers))
                                }
                            }
                            Some(NamedImportTypeFix::RemoveInlineTypeQualifiers(_)) => {
                                // Should not be reached because we pass `false` to `named_import_type_fix`.
                                None
                            }
                            None => is_default_used_as_type
                                .then_some(ImportTypeFix::ExtractDefaultImportType(vec![])),
                        }
                    }
                    AnyJsCombinedSpecifier::JsNamespaceImportSpecifier(namespace_specifier) => {
                        let namespace_binding = namespace_specifier.local_name().ok()?;
                        let namespace_binding = namespace_binding.as_js_identifier_binding()?;
                        if ctx.jsx_runtime() == JsxRuntime::ReactClassic
                            && is_global_react_import(namespace_binding, ReactLibrary::React)
                        {
                            return None;
                        }

                        match (
                            is_default_used_as_type,
                            is_only_used_as_type(model, namespace_binding),
                        ) {
                            (true, true) => Some(ImportTypeFix::UseImportType),
                            (true, false) => Some(ImportTypeFix::ExtractDefaultImportType(vec![])),
                            (false, true) => Some(ImportTypeFix::ExtractCombinedImportType),
                            (false, false) => None,
                        }
                    }
                }
            }
            AnyJsImportClause::JsImportDefaultClause(clause) => {
                if clause.type_token().is_some() {
                    return None;
                }
                let default_binding = clause.default_specifier().ok()?.local_name().ok()?;
                let default_binding = default_binding.as_js_identifier_binding()?;
                if ctx.jsx_runtime() == JsxRuntime::ReactClassic
                    && is_global_react_import(default_binding, ReactLibrary::React)
                {
                    return None;
                }

                is_only_used_as_type(model, default_binding).then_some(ImportTypeFix::UseImportType)
            }
            AnyJsImportClause::JsImportNamedClause(clause) => {
                match named_import_type_fix(
                    model,
                    &clause.named_specifiers().ok()?,
                    clause.type_token().is_some(),
                )? {
                    NamedImportTypeFix::UseImportType(_) => Some(ImportTypeFix::UseImportType),
                    NamedImportTypeFix::AddInlineTypeQualifiers(specifiers) => {
                        Some(ImportTypeFix::AddInlineTypeQualifiers(specifiers))
                    }
                    NamedImportTypeFix::RemoveInlineTypeQualifiers(type_tokens) => {
                        Some(ImportTypeFix::RemoveTypeQualifiers(type_tokens))
                    }
                }
            }
            AnyJsImportClause::JsImportNamespaceClause(clause) => {
                if clause.type_token().is_some() {
                    return None;
                }
                let namespace_binding = clause.namespace_specifier().ok()?.local_name().ok()?;
                let namespace_binding = namespace_binding.as_js_identifier_binding()?;
                if ctx.jsx_runtime() == JsxRuntime::ReactClassic
                    && is_global_react_import(namespace_binding, ReactLibrary::React)
                {
                    return None;
                }

                is_only_used_as_type(model, namespace_binding)
                    .then_some(ImportTypeFix::UseImportType)
            }
        }
    }

    fn diagnostic(ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let import = ctx.query();
        let import_clause = import.import_clause().ok()?;
        let diagnostic = match state {
            ImportTypeFix::UseImportType => RuleDiagnostic::new(
                rule_category!(),
                import_clause.range(),
                "All these imports are only used as types.",
            ),
            ImportTypeFix::ExtractDefaultImportType(named_specifiers) => {
                if named_specifiers.is_empty() {
                    RuleDiagnostic::new(
                        rule_category!(),
                        import_clause.range(),
                        "The default import is only used as a type.",
                    )
                } else {
                    let mut diagnostic = RuleDiagnostic::new(
                        rule_category!(),
                        import_clause.range(),
                        "The default import and some named imports are only used as types.",
                    );
                    for specifier in named_specifiers {
                        diagnostic = diagnostic
                            .detail(specifier.range(), "This import is only used as a type.")
                    }
                    diagnostic
                }
            }
            ImportTypeFix::ExtractCombinedImportType => {
                let AnyJsImportClause::JsImportCombinedClause(import_combined_clause) =
                    import_clause
                else {
                    unreachable!();
                };
                let specifier = import_combined_clause.specifier().ok()?;
                match specifier {
                    AnyJsCombinedSpecifier::JsNamedImportSpecifiers(_) => RuleDiagnostic::new(
                        rule_category!(),
                        specifier.range(),
                        "These named imports are only used as types.",
                    ),
                    AnyJsCombinedSpecifier::JsNamespaceImportSpecifier(_) => RuleDiagnostic::new(
                        rule_category!(),
                        specifier.range(),
                        "This namespace import is only used as a type.",
                    ),
                }
            }
            ImportTypeFix::AddInlineTypeQualifiers(named_specifiers) => {
                let mut diagnostic = RuleDiagnostic::new(
                    rule_category!(),
                    import_clause.range(),
                    "Some named imports are only used as types.",
                );
                for specifier in named_specifiers {
                    diagnostic =
                        diagnostic.detail(specifier.range(), "This import is only used as a type.")
                }
                diagnostic
            }
            ImportTypeFix::RemoveTypeQualifiers(type_tokens) => {
                let mut diagnostic = RuleDiagnostic::new(
                    rule_category!(),
                    import_clause.type_token()?.text_trimmed_range(),
                    markup! {
                        "This "<Emphasis>"type"</Emphasis>" keyword makes all inline "<Emphasis>"type"</Emphasis>" keywords useless."
                    },
                );
                for type_token in type_tokens {
                    diagnostic = diagnostic.detail(
                        type_token.text_trimmed_range(),
                        markup! {
                            "This inline "<Emphasis>"type"</Emphasis>" keyword is useless."
                        },
                    )
                }
                return Some(diagnostic);
            }
        };
        Some(diagnostic.note(markup! {
            "Importing the types with "<Emphasis>"import type"</Emphasis>" ensures that they are removed by the compilers and avoids loading unnecessary modules."
        }))
    }

    fn action(ctx: &RuleContext<Self>, state: &Self::State) -> Option<JsRuleAction> {
        let import = ctx.query();
        let import_clause = import.import_clause().ok()?;
        let mut mutation = ctx.root().begin();
        match state {
            ImportTypeFix::UseImportType => match import_clause {
                AnyJsImportClause::JsImportBareClause(_) => {
                    unreachable!();
                }
                AnyJsImportClause::JsImportCombinedClause(import_combined_clause) => {
                    let type_token = Some(
                        make::token(T![type])
                            .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                    );
                    let default_clause = extract_into_default_import_clause(
                        &import_combined_clause,
                        type_token.clone(),
                    )
                    .ok()?;
                    let new_import = import.clone().with_import_clause(default_clause.into());
                    let extra_import = extract_combined_specifier_in_new_import(
                        &import_combined_clause,
                        type_token,
                        import
                            .semicolon_token()
                            .is_some()
                            .then_some(make::token(T![;])),
                    )?;
                    add_module_items(
                        &mut mutation,
                        import.syntax(),
                        [new_import.into(), extra_import.into()],
                    );
                }
                AnyJsImportClause::JsImportDefaultClause(import_clause) => {
                    let specifier = import_clause.default_specifier().ok()?;
                    let new_import_clause = import_clause
                        .clone()
                        .with_default_specifier(specifier)
                        .with_type_token(Some(
                            make::token(T![type])
                                .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                        ));
                    mutation.replace_node(import_clause, new_import_clause);
                }
                AnyJsImportClause::JsImportNamedClause(import_clause) => {
                    let named_specifiers = import_clause.named_specifiers().ok()?;
                    let specifiers = named_specifiers.specifiers();
                    let mut new_specifiers = Vec::with_capacity(specifiers.len());
                    let mut new_separators = Vec::with_capacity(specifiers.len());
                    for specifier_element in specifiers.elements() {
                        let specifier = specifier_element.node().ok()?.clone();
                        let trailing_sep = specifier_element.into_trailing_separator().ok()?;
                        if let Some(type_token) = specifier.type_token() {
                            let new_specifier = specifier
                                .with_type_token(None)
                                .trim_leading_trivia()?
                                .prepend_trivia_pieces(chain_trivia_pieces(
                                    type_token.leading_trivia().pieces(),
                                    trim_leading_trivia_pieces(
                                        type_token.trailing_trivia().pieces(),
                                    ),
                                ))?;
                            new_specifiers.push(new_specifier);
                        } else {
                            new_specifiers.push(specifier);
                        }
                        if let Some(trailing_sep) = trailing_sep {
                            new_separators.push(trailing_sep);
                        }
                    }
                    let new_specifiers =
                        make::js_named_import_specifier_list(new_specifiers, new_separators);
                    let named_specifiers = named_specifiers.with_specifiers(new_specifiers);
                    let new_import_clause = import_clause
                        .clone()
                        .with_named_specifiers(named_specifiers)
                        .with_type_token(Some(
                            make::token(T![type])
                                .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                        ));
                    mutation.replace_node(import_clause, new_import_clause);
                }
                AnyJsImportClause::JsImportNamespaceClause(import_clause) => {
                    let specifier = import_clause.namespace_specifier().ok()?;
                    let new_import_clause = import_clause
                        .clone()
                        .with_namespace_specifier(specifier)
                        .with_type_token(Some(
                            make::token(T![type])
                                .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                        ));
                    mutation.replace_node(import_clause, new_import_clause);
                }
            },
            ImportTypeFix::ExtractDefaultImportType(specifiers_requiring_type_marker) => {
                let import_combined_clause = import_clause.as_js_import_combined_clause()?;
                let default_import_clause = extract_into_default_import_clause(
                    import_combined_clause,
                    Some(
                        make::token(T![type])
                            .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                    ),
                )
                .ok()?;
                let new_import = import
                    .clone()
                    .with_import_clause(default_import_clause.into());
                let extra_import = if let Some(AnyJsCombinedSpecifier::JsNamedImportSpecifiers(
                    named_specifiers,
                )) = import_combined_clause
                    .specifier()
                    .ok()
                    .filter(|_| !specifiers_requiring_type_marker.is_empty())
                {
                    let specifiers_requiring_type_marker = specifiers_requiring_type_marker
                        .iter()
                        .map(|specifier| specifier.range().start())
                        .collect::<FxHashSet<_>>();
                    let source = import_combined_clause
                        .source()
                        .ok()?
                        .with_leading_trivia_pieces([])?
                        .with_trailing_trivia_pieces([])?;
                    let specifiers = named_specifiers.specifiers();
                    let mut new_specifiers = Vec::with_capacity(specifiers.len());
                    let mut new_separators = Vec::with_capacity(specifiers.len());
                    for specifier_element in specifiers.elements() {
                        let specifier = specifier_element.node().ok()?.clone();
                        let trailing_sep = specifier_element.into_trailing_separator().ok()?;
                        if specifiers_requiring_type_marker.contains(&specifier.range().start()) {
                            let new_specifier = specifier.with_type_token(Some(
                                make::token(T![type])
                                    .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                            ));
                            new_specifiers.push(new_specifier);
                        } else {
                            new_specifiers.push(specifier);
                        }
                        if let Some(trailing_sep) = trailing_sep {
                            new_separators.push(trailing_sep);
                        }
                    }
                    let new_specifiers =
                        make::js_named_import_specifier_list(new_specifiers, new_separators);
                    let named_specifiers = named_specifiers.with_specifiers(new_specifiers);
                    let import_clause = AnyJsImportClause::from(
                        make::js_import_named_clause(
                            named_specifiers,
                            make::token(T![from])
                                .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                            source,
                        )
                        .build(),
                    );
                    make::js_import(
                        make::token(T![import])
                            .with_leading_trivia([(TriviaPieceKind::Newline, "\n")])
                            .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                        import_clause,
                    )
                    .build()
                } else {
                    extract_combined_specifier_in_new_import(
                        import_combined_clause,
                        None,
                        import
                            .semicolon_token()
                            .is_some()
                            .then_some(make::token(T![;])),
                    )?
                };
                add_module_items(
                    &mut mutation,
                    import.syntax(),
                    [new_import.into(), extra_import.into()],
                );
            }
            ImportTypeFix::ExtractCombinedImportType => {
                let AnyJsImportClause::JsImportCombinedClause(import_combined_clause) =
                    import_clause
                else {
                    unreachable!();
                };
                let default_import_clause =
                    extract_into_default_import_clause(&import_combined_clause, None).ok()?;
                let new_import = import
                    .clone()
                    .with_import_clause(default_import_clause.into());
                let extra_import = extract_combined_specifier_in_new_import(
                    &import_combined_clause,
                    Some(
                        make::token(T![type])
                            .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                    ),
                    import
                        .semicolon_token()
                        .is_some()
                        .then_some(make::token(T![;])),
                )?;
                add_module_items(
                    &mut mutation,
                    import.syntax(),
                    [new_import.into(), extra_import.into()],
                );
            }
            ImportTypeFix::AddInlineTypeQualifiers(specifiers) => {
                for specifier in specifiers {
                    let new_specifier = specifier
                        .clone()
                        .with_leading_trivia_pieces([])?
                        .with_type_token(Some(
                            make::token(T![type])
                                .with_leading_trivia_pieces(
                                    specifier.syntax().first_leading_trivia()?.pieces(),
                                )
                                .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
                        ));
                    mutation.replace_node(specifier.clone(), new_specifier);
                }
                return Some(JsRuleAction::new(
                    ctx.metadata().action_category(ctx.category(), ctx.group()),
                    ctx.metadata().applicability(),
                    markup! { "Add inline "<Emphasis>"type"</Emphasis>" keywords." }.to_owned(),
                    mutation,
                ));
            }
            ImportTypeFix::RemoveTypeQualifiers(type_tokens) => {
                for type_token in type_tokens {
                    mutation.remove_token(type_token.clone());
                }
                return Some(JsRuleAction::new(
                    ctx.metadata().action_category(ctx.category(), ctx.group()),
                    ctx.metadata().applicability(),
                    markup! { "Remove useless inline "<Emphasis>"type"</Emphasis>" keywords." }
                        .to_owned(),
                    mutation,
                ));
            }
        }
        Some(JsRuleAction::new(
            ctx.metadata().action_category(ctx.category(), ctx.group()),
            ctx.metadata().applicability(),
            markup! { "Use "<Emphasis>"import type"</Emphasis>"." }.to_owned(),
            mutation,
        ))
    }
}

#[derive(Debug)]
pub enum ImportTypeFix {
    UseImportType,
    ExtractDefaultImportType(Vec<AnyJsNamedImportSpecifier>),
    ExtractCombinedImportType,
    AddInlineTypeQualifiers(Vec<AnyJsNamedImportSpecifier>),
    RemoveTypeQualifiers(Vec<JsSyntaxToken>),
}

/// Returns `true` if all references of `binding` are only used as a type.
/// If there is no reference, then returns `false`.
fn is_only_used_as_type(model: &SemanticModel, binding: &JsIdentifierBinding) -> bool {
    let mut result = false;
    for reference in binding.all_references(model) {
        if let Some(reference) = AnyJsIdentifierUsage::cast_ref(reference.syntax()) {
            result = reference.is_only_type();
            if !result {
                break;
            }
        }
    }
    result
}

#[derive(Debug)]
pub enum NamedImportTypeFix {
    UseImportType(Vec<AnyJsNamedImportSpecifier>),
    AddInlineTypeQualifiers(Vec<AnyJsNamedImportSpecifier>),
    RemoveInlineTypeQualifiers(Vec<JsSyntaxToken>),
}

fn named_import_type_fix(
    model: &SemanticModel,
    named_specifiers: &JsNamedImportSpecifiers,
    has_type_token: bool,
) -> Option<NamedImportTypeFix> {
    let specifiers = named_specifiers.specifiers();
    if specifiers.is_empty() {
        return None;
    };
    if has_type_token {
        let useless_type_tokens: Vec<_> = specifiers
            .iter()
            .filter_map(|specifier| specifier.ok()?.type_token())
            .collect();
        if useless_type_tokens.is_empty() {
            None
        } else {
            Some(NamedImportTypeFix::RemoveInlineTypeQualifiers(
                useless_type_tokens,
            ))
        }
    } else {
        let mut imports_only_types = true;
        let mut specifiers_requiring_type_marker = Vec::with_capacity(specifiers.len());
        for specifier in specifiers.iter() {
            let Ok(specifier) = specifier else {
                imports_only_types = false;
                continue;
            };
            if specifier.type_token().is_none() {
                if specifier
                    .local_name()
                    .and_then(|local_name| {
                        Some(is_only_used_as_type(
                            model,
                            local_name.as_js_identifier_binding()?,
                        ))
                    })
                    .unwrap_or(false)
                {
                    specifiers_requiring_type_marker.push(specifier);
                } else {
                    imports_only_types = false;
                }
            }
        }
        if imports_only_types {
            Some(NamedImportTypeFix::UseImportType(
                specifiers_requiring_type_marker,
            ))
        } else if specifiers_requiring_type_marker.is_empty() {
            None
        } else {
            Some(NamedImportTypeFix::AddInlineTypeQualifiers(
                specifiers_requiring_type_marker,
            ))
        }
    }
}

fn add_module_items(
    mutation: &mut BatchMutation<JsLanguage>,
    preceding_item: &JsSyntaxNode,
    new_items: impl IntoIterator<Item = AnyJsModuleItem>,
) {
    let Some(module_item_list) = preceding_item.parent().and_then(JsModuleItemList::cast) else {
        return;
    };
    let module_item_list = module_item_list.into_syntax();
    let Some(slot) = module_item_list
        .slots()
        .position(|slot| slot.into_node().as_ref() == Some(preceding_item))
    else {
        return;
    };
    let new_module_item_list = module_item_list.clone().splice_slots(
        slot..(slot + 1),
        new_items
            .into_iter()
            .map(|item| Some(SyntaxElement::Node(item.into_syntax()))),
    );
    mutation.replace_element(module_item_list.into(), new_module_item_list.into());
}

fn extract_into_default_import_clause(
    import_clause: &JsImportCombinedClause,
    type_token: Option<JsSyntaxToken>,
) -> SyntaxResult<JsImportDefaultClause> {
    let from_token = import_clause
        .from_token()?
        .with_leading_trivia([(TriviaPieceKind::Whitespace, " ")]);
    let result = make::js_import_default_clause(
        import_clause.default_specifier()?,
        from_token,
        import_clause.source()?,
    )
    .build()
    .with_type_token(type_token);
    Ok(result)
}

fn extract_combined_specifier_in_new_import(
    import_clause: &JsImportCombinedClause,
    type_token: Option<JsSyntaxToken>,
    semicolon_token: Option<JsSyntaxToken>,
) -> Option<JsImport> {
    let comma_trailing_trivia =
        trim_leading_trivia_pieces(import_clause.comma_token().ok()?.trailing_trivia().pieces());
    let comma_leading_trivia =
        trim_trailing_trivia_pieces(import_clause.comma_token().ok()?.leading_trivia().pieces());
    let from_token =
        make::token(T![from]).with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]);
    let source = import_clause
        .source()
        .ok()?
        .with_leading_trivia_pieces([])?
        .with_trailing_trivia_pieces([])?;
    let import_clause = match import_clause.specifier().ok()? {
        AnyJsCombinedSpecifier::JsNamedImportSpecifiers(named_specifiers) => {
            let import_clause = if type_token.is_some() {
                let specifiers = named_specifiers.specifiers();
                let mut new_specifiers = specifiers.syntax().clone();
                for specifier in specifiers {
                    let Ok(specifier) = specifier else {
                        continue;
                    };
                    if let Some(type_token) = specifier.type_token() {
                        let new_specifier = specifier
                            .clone()
                            .with_type_token(None)
                            .trim_leading_trivia()?
                            .prepend_trivia_pieces(chain_trivia_pieces(
                                type_token.leading_trivia().pieces(),
                                trim_leading_trivia_pieces(type_token.trailing_trivia().pieces()),
                            ))?;
                        new_specifiers = new_specifiers.replace_child(
                            specifier.clone().into_syntax().into(),
                            new_specifier.into_syntax().into(),
                        )?;
                    }
                }
                let new_specifiers = JsNamedImportSpecifierList::unwrap_cast(new_specifiers);
                let named_specifiers = named_specifiers.with_specifiers(new_specifiers);
                make::js_import_named_clause(named_specifiers, from_token, source)
                    .build()
                    .prepend_trivia_pieces(comma_trailing_trivia)?
                    .with_type_token(type_token)
            } else {
                make::js_import_named_clause(named_specifiers, from_token, source).build()
            };
            AnyJsImportClause::from(import_clause)
        }
        AnyJsCombinedSpecifier::JsNamespaceImportSpecifier(specifier) => AnyJsImportClause::from(
            make::js_import_namespace_clause(specifier, from_token, source)
                .build()
                .prepend_trivia_pieces(comma_trailing_trivia)?
                .with_type_token(type_token),
        ),
    };
    make::js_import(
        make::token(T![import])
            .with_leading_trivia([(TriviaPieceKind::Newline, "\n")])
            .with_trailing_trivia([(TriviaPieceKind::Whitespace, " ")]),
        import_clause,
    )
    .build()
    .with_semicolon_token(semicolon_token)
    .prepend_trivia_pieces(comma_leading_trivia)
}
