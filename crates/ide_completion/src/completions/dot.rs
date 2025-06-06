use hir_ty::ty::TyKind;
use itertools::Itertools;

use super::Completions;
use crate::{
    context::{CompletionContext, ImmediateLocation},
    item::{CompletionItem, CompletionItemKind, CompletionRelevance},
};

pub(crate) fn complete_dot(
    accumulator: &mut Completions,
    ctx: &CompletionContext,
) -> Option<()> {
    let field_expression = match &ctx.completion_location {
        Some(ImmediateLocation::FieldAccess { expression }) => expression,
        _ => return Some(()),
    };
    let sa = ctx.sema.analyze(ctx.container?);
    let r#type = sa.type_of_expression(&field_expression.expression()?)?;

    let field_completion_item =
        |name| CompletionItem::new(CompletionItemKind::Field, ctx.source_range(), name).build();

    match r#type.kind(ctx.db).unref(ctx.db).as_ref() {
        TyKind::Vector(vec) => {
            let size = vec.size.as_u8() as usize;
            let swizzle = swizzle_items(size, ctx, &[["x", "y", "z", "w"], ["r", "g", "b", "a"]]);
            accumulator.add_all(swizzle);
        },
        TyKind::Matrix(_) => return None,
        TyKind::Struct(r#struct) => {
            let r#struct = ctx.db.struct_data(*r#struct);
            let items = r#struct
                .fields()
                .iter()
                .map(|(_, field)| field.name.as_str())
                .map(field_completion_item);
            accumulator.add_all(items);
        },
        _ => return None,
    };

    Some(())
}

fn swizzle_items<'a>(
    size: usize,
    ctx: &'a CompletionContext,
    sets: &'a [[&'a str; 4]],
) -> impl Iterator<Item = CompletionItem> + 'a {
    let swizzle = move |set: &'a [&'a str; 4]| {
        (1..=4).flat_map(move |n| {
            (std::iter::repeat_with(|| set[0..size].iter()).take(n))
                .multi_cartesian_product()
                .map(|result| result.into_iter().copied().collect::<String>())
        })
    };
    sets.iter()
        .flat_map(swizzle)
        .enumerate()
        .map(move |(i, label)| {
            CompletionItem::new(CompletionItemKind::Field, ctx.source_range(), label)
                .with_relevance(CompletionRelevance {
                    swizzle_index: Some(i),
                    ..Default::default()
                })
                .build()
        })
}
