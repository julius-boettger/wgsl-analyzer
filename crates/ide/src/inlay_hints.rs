use base_db::{FileId, FileRange, TextRange};
use hir::{Field, HasSource, Semantics};
use hir_def::{InFile, data::FieldId, module_data::Name};
use hir_ty::{
    function::FunctionDetails,
    infer::ResolvedCall,
    layout::{FieldLayout, LayoutAddressSpace},
    ty::pretty::{TypeVerbosity, pretty_type_with_verbosity},
};
use rowan::NodeOrToken;
use smol_str::SmolStr;
use syntax::{AstChildren, AstNode, HasName, SyntaxNode, ast};

use crate::RootDatabase;

#[derive(Clone, Debug)]
pub struct InlayHintsConfig {
    pub enabled: bool,
    pub type_hints: bool,
    pub parameter_hints: bool,
    pub struct_layout_hints: Option<StructLayoutHints>,
    pub type_verbosity: TypeVerbosity,
}

#[derive(Clone, Copy, Debug)]
pub enum StructLayoutHints {
    Offset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InlayKind {
    TypeHint,
    ParameterHint,
    StructLayoutHint,
}

#[derive(Debug)]
pub struct InlayHint {
    pub range: TextRange,
    pub kind: InlayKind,
    pub label: SmolStr,
}

pub(crate) fn inlay_hints(
    db: &RootDatabase,
    file_id: FileId,
    range_limit: Option<FileRange>,
    config: &InlayHintsConfig,
) -> Vec<InlayHint> {
    let sema = Semantics::new(db);
    let file = sema.parse(file_id);

    let mut hints = Vec::new();

    if let Some(range_limit) = range_limit {
        let range_limit = range_limit.range;
        match file.syntax().covering_element(range_limit) {
            NodeOrToken::Token(_) => return hints,
            NodeOrToken::Node(n) => {
                for node in n
                    .descendants()
                    .filter(|descendant| range_limit.contains_range(descendant.text_range()))
                {
                    get_hints(&mut hints, file_id, &sema, config, node);
                }

                get_struct_layout_hints(&mut hints, file_id, &sema, config);
            },
        }
    } else {
        for node in file.syntax().descendants() {
            get_hints(&mut hints, file_id, &sema, config, node);
        }

        get_struct_layout_hints(&mut hints, file_id, &sema, config);
    }

    hints
}

fn get_struct_layout_hints(
    hints: &mut Vec<InlayHint>,
    file_id: FileId,
    sema: &Semantics,
    config: &InlayHintsConfig,
) -> Option<()> {
    let display_kind = config.struct_layout_hints?;

    let module_info = sema.db.module_info(file_id.into());

    for r#struct in module_info.structs() {
        let r#struct = sema.db.intern_struct(InFile::new(file_id.into(), r#struct));
        let fields = sema.db.field_types(r#struct);

        let address_space = if sema.db.struct_is_used_in_uniform(r#struct, file_id.into()) {
            LayoutAddressSpace::Uniform
        } else {
            LayoutAddressSpace::Storage
        };

        hir_ty::layout::struct_member_layout(
            &fields,
            sema.db,
            address_space,
            |field, field_layout| {
                let FieldLayout {
                    offset,
                    align: _,
                    size: _,
                } = field_layout;
                let field = Field {
                    id: FieldId { r#struct, field },
                };

                let source = field.source(sema.db.upcast())?.value;

                // this is only necessary, because the field syntax nodes include the whitespace to the next line...
                let actual_last_token = std::iter::successors(
                    source.syntax().last_token(),
                    rowan::SyntaxToken::prev_token, // spellchecker:disable-line
                )
                .find(|token| !token.kind().is_trivia())?;
                let range = TextRange::new(
                    source.syntax().text_range().start(),
                    actual_last_token.text_range().end(),
                );

                hints.push(InlayHint {
                    range,
                    kind: InlayKind::StructLayoutHint,
                    label: match display_kind {
                        StructLayoutHints::Offset => format!("{offset}").into(),
                    },
                });

                Some(())
            },
        );
    }

    Some(())
}

fn get_hints(
    hints: &mut Vec<InlayHint>,
    file_id: FileId,
    sema: &Semantics,
    config: &InlayHintsConfig,
    node: SyntaxNode,
) -> Option<()> {
    if let Some(expression) = ast::Expression::cast(node.clone()) {
        #[allow(clippy::single_match)] // for extendability
        match &expression {
            ast::Expression::FunctionCall(function_call_expression) => {
                if !config.parameter_hints {
                    return None;
                }
                function_hints(
                    sema,
                    file_id,
                    &node,
                    &expression,
                    function_call_expression.parameters()?.arguments(),
                    hints,
                )?;
            },
            ast::Expression::TypeInitializer(type_initialiser_expression) => {
                if !config.parameter_hints {
                    return None;
                }
                // Show hints for the built-in initializers.
                // `vec4(xyz: val1, w: val2)` could also be
                // `vec4(xy: val1, zw: val2)` without hints
                function_hints(
                    sema,
                    file_id,
                    &node,
                    &expression,
                    type_initialiser_expression.arguments()?.arguments(),
                    hints,
                )?;
            },
            _ => {},
        }
    } else if let Some((binding, r#type)) = ast::VariableStatement::cast(node.clone())
        .and_then(|statement| Some((statement.binding()?, statement.ty())))
        .or_else(|| {
            ast::GlobalConstantDeclaration::cast(node.clone())
                .and_then(|statement| Some((statement.binding()?, statement.ty())))
        })
        .or_else(|| {
            ast::GlobalVariableDeclaration::cast(node.clone())
                .and_then(|statement| Some((statement.binding()?, statement.ty())))
        })
    {
        if !config.type_hints {
            return None;
        }
        if r#type.is_none() {
            let container = sema.find_container(file_id.into(), &node)?;
            let r#type = sema.analyze(container).type_of_binding(&binding)?;

            let label = pretty_type_with_verbosity(sema.db, r#type, config.type_verbosity);
            hints.push(InlayHint {
                range: binding.name()?.ident_token()?.text_range(),
                kind: InlayKind::TypeHint,
                label: label.into(),
            });
        }
    }

    Some(())
}

fn function_hints(
    sema: &Semantics,
    file_id: FileId,
    node: &SyntaxNode,
    expression: &ast::Expression,
    parameter_expressions: AstChildren<ast::Expression>,
    hints: &mut Vec<InlayHint>,
) -> Option<()> {
    let container = sema.find_container(file_id.into(), node)?;
    let analyzed = sema.analyze(container);
    let expression = analyzed.expression_id(expression)?;
    let resolved = analyzed.infer.call_resolution(expression)?;
    let func = match resolved {
        ResolvedCall::Function(func) => func.lookup(analyzed.db),
        ResolvedCall::OtherTypeInitializer(_) => return None,
    };
    let param_hints = func
        .parameter_names()
        .zip(parameter_expressions)
        .filter(|&(name, _)| !Name::is_missing(name))
        .filter(|(param_name, expression)| {
            !should_hide_param_name_hint(&func, param_name, expression)
        })
        .map(|(param_name, expression)| InlayHint {
            range: expression.syntax().text_range(),
            kind: InlayKind::ParameterHint,
            label: param_name.into(),
        });
    hints.extend(param_hints);
    Some(())
}

// taken from https://github.com/rust-lang/rust-analyzer/blob/7308b3ef413cad8c211e239d32c9fab29ae2e664/crates/ide/src/inlay_hints.rs#L422

fn should_hide_param_name_hint(
    func: &FunctionDetails,
    param_name: &str,
    expression: &ast::Expression,
) -> bool {
    is_argument_similar_to_param_name(expression, param_name)
        || (func.parameters.len() == 1 && is_obvious_parameter(param_name))
}

fn is_argument_similar_to_param_name(
    expression: &ast::Expression,
    param_name: &str,
) -> bool {
    let argument = match get_string_representation(expression) {
        Some(argument) => argument,
        None => return false,
    };

    // std is honestly too panic happy...
    let str_split_at = |str: &str, at| str.is_char_boundary(at).then(|| argument.split_at(at));

    let param_name = param_name.trim_start_matches('_');
    let argument = argument.trim_start_matches('_');

    match str_split_at(argument, param_name.len()) {
        Some((prefix, rest)) if prefix.eq_ignore_ascii_case(param_name) => {
            return rest.is_empty() || rest.starts_with('_');
        },
        _ => (),
    }
    match argument
        .len()
        .checked_sub(param_name.len())
        .and_then(|at| str_split_at(argument, at))
    {
        Some((rest, suffix)) if param_name.eq_ignore_ascii_case(suffix) => {
            return rest.is_empty() || rest.ends_with('_');
        },
        _ => (),
    }

    // mixed camelCase/snake_case
    if compare_ignore_case_convention(argument, param_name) {
        return true;
    }

    false
}

fn is_obvious_parameter(param_name: &str) -> bool {
    let is_obvious_param_name = matches!(param_name, "predicate" | "value");
    param_name.len() == 1 || is_obvious_param_name
}

fn compare_ignore_case_convention(
    argument: &str,
    param_name: &str,
) -> bool {
    argument
        .chars()
        .filter(|&c| c != '_')
        .zip(param_name.chars().filter(|&c| c != '_'))
        .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

fn get_string_representation(expression: &ast::Expression) -> Option<String> {
    match expression {
        ast::Expression::PathExpression(expression) => {
            Some(expression.name_ref()?.text().as_str().to_string())
        },
        ast::Expression::PrefixExpression(expression) => {
            get_string_representation(&expression.expression()?)
        },
        ast::Expression::FieldExpression(expression) => {
            Some(expression.name_ref()?.text().as_str().to_string())
        },
        _ => None,
    }
}
