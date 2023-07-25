use cairo_lang_defs::ids::{
    FunctionWithBodyId, LanguageElementId, LookupItemId, ModuleFileId, ModuleId, ModuleItemId,
    TopLevelLanguageElementId, TraitFunctionId,
};
use cairo_lang_semantic::db::SemanticGroup;
use cairo_lang_semantic::diagnostic::{NotFoundItemType, SemanticDiagnostics};
use cairo_lang_semantic::expr::inference::infers::InferenceEmbeddings;
use cairo_lang_semantic::expr::inference::solver::SolutionSet;
use cairo_lang_semantic::items::function_with_body::SemanticExprLookup;
use cairo_lang_semantic::items::structure::SemanticStructEx;
use cairo_lang_semantic::items::us::SemanticUseEx;
use cairo_lang_semantic::lookup_item::{HasResolverData, LookupItemEx};
use cairo_lang_semantic::lsp_helpers::TypeFilter;
use cairo_lang_semantic::resolve::{ResolvedConcreteItem, ResolvedGenericItem, Resolver};
use cairo_lang_semantic::types::peel_snapshots;
use cairo_lang_semantic::{ConcreteTypeId, Pattern, TypeLongId};
use cairo_lang_syntax::node::ast::PathSegment;
use cairo_lang_syntax::node::{ast, TypedSyntaxNode};
use lsp::{CompletionItem, CompletionItemKind, Position, Range, TextEdit};

pub fn generic_completions(
    db: &(dyn SemanticGroup + 'static),
    module_file_id: ModuleFileId,
    lookup_items: Vec<LookupItemId>,
) -> Vec<CompletionItem> {
    let mut completions = vec![];

    // Crates.
    completions.extend(db.crate_roots().keys().map(|crate_id| CompletionItem {
        label: db.lookup_intern_crate(*crate_id).0.into(),
        kind: Some(CompletionItemKind::MODULE),
        ..CompletionItem::default()
    }));

    // Module completions.
    completions.extend(db.module_items(module_file_id.0).unwrap_or_default().iter().map(|item| {
        CompletionItem {
            label: item.name(db.upcast()).to_string(),
            kind: Some(CompletionItemKind::MODULE),
            ..CompletionItem::default()
        }
    }));

    // Local variables.
    let Some(lookup_item_id) = lookup_items.into_iter().next() else {
        return completions;
    };
    let function_id = match lookup_item_id {
        LookupItemId::ModuleItem(ModuleItemId::FreeFunction(free_function_id)) => {
            FunctionWithBodyId::Free(free_function_id)
        }
        LookupItemId::ImplFunction(impl_function_id) => FunctionWithBodyId::Impl(impl_function_id),
        _ => {
            return completions;
        }
    };
    let Ok(body) = db.function_body(function_id) else {
        return completions;
    };
    for (_id, pat) in &body.patterns {
        if let Pattern::Variable(var) = pat {
            completions.push(CompletionItem {
                label: var.name.clone().into(),
                kind: Some(CompletionItemKind::VARIABLE),
                ..CompletionItem::default()
            });
        }
    }
    completions
}

pub fn colon_colon_completions(
    db: &(dyn SemanticGroup + 'static),
    module_file_id: ModuleFileId,
    lookup_items: Vec<LookupItemId>,
    segments: Vec<PathSegment>,
) -> Option<Vec<CompletionItem>> {
    // Get a resolver in the current context.
    let resolver_data = match lookup_items.into_iter().next() {
        Some(item) => (*item.resolver_data(db).ok()?).clone(),
        None => Resolver::new(db, module_file_id).data,
    };
    let mut resolver = Resolver::with_data(db, resolver_data);

    let mut diagnostics = SemanticDiagnostics::new(module_file_id);
    let item = resolver
        .resolve_concrete_path(&mut diagnostics, segments, NotFoundItemType::Identifier)
        .ok()?;

    Some(match item {
        ResolvedConcreteItem::Module(module_id) => db
            .module_items(module_id)
            .unwrap_or_default()
            .iter()
            .map(|item| CompletionItem {
                label: item.name(db.upcast()).to_string(),
                kind: Some(CompletionItemKind::MODULE),
                ..CompletionItem::default()
            })
            .collect(),
        ResolvedConcreteItem::Trait(_) => todo!(),
        ResolvedConcreteItem::Impl(_) => todo!(),
        ResolvedConcreteItem::Type(_) => todo!(),
        _ => vec![],
    })
}

pub fn dot_completions(
    db: &(dyn SemanticGroup + 'static),
    lookup_items: Vec<LookupItemId>,
    expr: ast::ExprBinary,
) -> Option<Vec<CompletionItem>> {
    let syntax_db = db.upcast();
    // Get a resolver in the current context.
    let lookup_item_id = lookup_items.into_iter().next()?;
    let function_with_body = lookup_item_id.function_with_body()?;
    let module_id = function_with_body.module_file_id(db.upcast()).0;
    let resolver_data = lookup_item_id.resolver_data(db).ok()?;
    let resolver = Resolver::with_data(db, resolver_data.as_ref().clone());

    // Extract lhs node.
    let node = expr.lhs(syntax_db);
    let stable_ptr = node.stable_ptr().untyped();
    // Get its semantic model.
    let expr_id = db.lookup_expr_by_ptr(function_with_body, node.stable_ptr()).ok()?;
    let semantic_expr = db.expr_semantic(function_with_body, expr_id);
    // Get the type.
    let ty = semantic_expr.ty();
    if ty.is_missing(db) {
        eprintln!("Type is missing");
        return None;
    }

    // Find relevant methods for type.
    let relevant_methods = find_methods_for_type(db, resolver, ty, stable_ptr);

    let mut completions = Vec::new();
    for trait_function in relevant_methods {
        let Some(completion) = completion_for_method(db, module_id, trait_function) else {
            continue;
        };
        completions.push(completion);
    }

    // Find members of the type.
    let (_, long_ty) = peel_snapshots(db, ty);
    if let TypeLongId::Concrete(ConcreteTypeId::Struct(concrete_struct_id)) = long_ty {
        db.concrete_struct_members(concrete_struct_id).ok()?.into_iter().for_each(
            |(name, member)| {
                let completion = CompletionItem {
                    label: name.to_string(),
                    detail: Some(member.ty.format(db.upcast())),
                    kind: Some(CompletionItemKind::FIELD),
                    ..CompletionItem::default()
                };
                completions.push(completion);
            },
        );
    }
    Some(completions)
}

/// Returns a completion item for a method.
fn completion_for_method(
    db: &dyn SemanticGroup,
    module_id: ModuleId,
    trait_function: TraitFunctionId,
) -> Option<CompletionItem> {
    let trait_id = trait_function.trait_id(db.upcast());
    let name = trait_function.name(db.upcast());
    db.trait_function_signature(trait_function).ok()?;

    // TODO(spapini): Add signature.
    let detail = trait_id.full_path(db.upcast());
    let trait_full_path = trait_id.full_path(db.upcast());
    let mut additional_text_edits = vec![];

    // If the trait is not in scope, add a use statement.
    if !module_has_trait(db, module_id, trait_id)? {
        additional_text_edits.push(TextEdit {
            range: Range::new(
                Position { line: 0, character: 0 },
                Position { line: 0, character: 0 },
            ),
            new_text: format!("use {trait_full_path};\n"),
        });
    }

    let completion = CompletionItem {
        label: format!("{}()", name),
        insert_text: Some(format!("{}(", name)),
        detail: Some(detail),
        kind: Some(CompletionItemKind::METHOD),
        additional_text_edits: Some(additional_text_edits),
        ..CompletionItem::default()
    };
    Some(completion)
}

/// Checks if a module has a trait in scope.
fn module_has_trait(
    db: &dyn SemanticGroup,
    module_id: ModuleId,
    trait_id: cairo_lang_defs::ids::TraitId,
) -> Option<bool> {
    if db.module_traits_ids(module_id).ok()?.contains(&trait_id) {
        return Some(true);
    }
    for use_id in db.module_uses_ids(module_id).ok()? {
        if db.use_resolved_item(use_id) == Ok(ResolvedGenericItem::Trait(trait_id)) {
            return Some(true);
        }
    }
    Some(false)
}

/// Finds all methods that can be called on a type.
fn find_methods_for_type(
    db: &(dyn SemanticGroup + 'static),
    mut resolver: Resolver<'_>,
    ty: cairo_lang_semantic::TypeId,
    stable_ptr: cairo_lang_syntax::node::ids::SyntaxStablePtrId,
) -> Vec<TraitFunctionId> {
    let type_filter = match ty.head(db) {
        Some(head) => TypeFilter::TypeHead(head),
        None => TypeFilter::NoFilter,
    };

    let mut relevant_methods = Vec::new();
    // Find methods on type.
    // TODO(spapini): Look only in current crate dependencies.
    for crate_id in db.crates() {
        let methods = db.methods_in_crate(crate_id, type_filter.clone());
        for trait_function in methods {
            let clone_data = &mut resolver.inference().clone_data();
            let mut inference = clone_data.inference(db);
            let lookup_context = resolver.impl_lookup_context();
            // Check if trait function signature's first param can fit our expr type.
            let Some((concrete_trait_id, _)) = inference.infer_concrete_trait_by_self(
                trait_function,
                ty,
                &lookup_context,
                Some(stable_ptr),
            ) else {
                eprintln!("Can't fit");
                continue;
            };

            // Find impls for it.
            inference.solve().ok();
            if !matches!(
                inference.trait_solution_set(concrete_trait_id, lookup_context),
                Ok(SolutionSet::Unique(_) | SolutionSet::Ambiguous(_))
            ) {
                continue;
            }
            relevant_methods.push(trait_function);
        }
    }
    relevant_methods
}
