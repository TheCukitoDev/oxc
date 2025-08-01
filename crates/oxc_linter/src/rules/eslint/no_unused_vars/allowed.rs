//! This module checks if an unused variable is allowed. Note that this does not
//! consider variables ignored by name pattern, but by where they are declared.
use oxc_ast::{AstKind, ast::*};
use oxc_semantic::{NodeId, Semantic};

use super::{NoUnusedVars, Symbol, options::ArgsOption};
use crate::{
    ModuleRecord,
    rules::eslint::no_unused_vars::binding_pattern::{BindingContext, HasAnyUsedBinding},
};

impl Symbol<'_, '_> {
    /// Check if the declaration of this [`Symbol`] is use.
    ///
    /// If it's an expression, then it's always passed in as an argument
    /// or assigned to a variable, so it's always used.
    ///
    /// ```js
    /// // True:
    /// const a = class Name{}
    /// export default (function Name() {})
    /// console.log(function Name() {})
    ///
    /// // False
    /// function foo() {}
    /// {
    ///    class Foo {}
    /// }
    /// ```
    #[inline]
    pub(crate) fn is_function_or_class_declaration_used(&self) -> bool {
        match self.declaration().kind() {
            AstKind::Class(class) => class.is_expression(),
            AstKind::Function(func) => func.is_expression(),
            _ => false,
        }
    }

    fn is_declared_in_for_of_loop(&self) -> bool {
        for parent in self.iter_parents() {
            match parent.kind() {
                AstKind::ParenthesizedExpression(_)
                | AstKind::VariableDeclaration(_)
                | AstKind::BindingIdentifier(_)
                | AstKind::SimpleAssignmentTarget(_)
                | AstKind::AssignmentTarget(_) => {}
                AstKind::ForInStatement(ForInStatement { body, .. })
                | AstKind::ForOfStatement(ForOfStatement { body, .. }) => match body {
                    Statement::ReturnStatement(_) => return true,
                    Statement::BlockStatement(b) => {
                        return b
                            .body
                            .first()
                            .is_some_and(|s| matches!(s, Statement::ReturnStatement(_)));
                    }
                    _ => return false,
                },
                _ => return false,
            }
        }

        false
    }

    pub fn is_in_declared_module(&self) -> bool {
        let scopes = self.scoping();
        let nodes = self.nodes();
        scopes.scope_ancestors(self.scope_id())
            .map(|scope_id| scopes.get_node_id(scope_id))
            .map(|node_id| nodes.get_node(node_id))
            .any(|node| matches!(node.kind(), AstKind::TSModuleDeclaration(namespace) if is_ambient_namespace(namespace)))
    }
}

#[inline]
fn is_ambient_namespace(namespace: &TSModuleDeclaration) -> bool {
    namespace.declare || namespace.kind.is_global()
}

impl NoUnusedVars {
    #[expect(clippy::unused_self)]
    pub(super) fn is_allowed_ts_namespace<'a>(
        &self,
        symbol: &Symbol<'_, 'a>,
        namespace: &TSModuleDeclaration<'a>,
    ) -> bool {
        if is_ambient_namespace(namespace) {
            return true;
        }
        symbol.is_in_declared_module()
    }

    /// Returns `true` if this unused variable declaration should be allowed
    /// (i.e. not reported)
    pub(super) fn is_allowed_variable_declaration<'a>(
        &self,
        symbol: &Symbol<'_, 'a>,
        decl: &VariableDeclarator<'a>,
    ) -> bool {
        if decl.kind.is_var() && self.vars.is_local() && symbol.is_root() {
            return true;
        }

        // allow unused iterators, since they're required for valid syntax
        if symbol.is_declared_in_for_of_loop() {
            return true;
        }

        false
    }

    #[expect(clippy::unused_self)]
    pub(super) fn is_allowed_type_parameter(
        &self,
        symbol: &Symbol<'_, '_>,
        declaration_id: NodeId,
    ) -> bool {
        matches!(symbol.nodes().parent_kind(declaration_id), AstKind::TSMappedType(_))
    }

    /// Returns `true` if this unused parameter should be allowed (i.e. not
    /// reported)
    pub(super) fn is_allowed_argument<'a>(
        &self,
        semantic: &Semantic<'a>,
        module_record: &ModuleRecord,
        symbol: &Symbol<'_, 'a>,
        param: &FormalParameter<'a>,
    ) -> bool {
        // early short-circuit when no argument checking should be performed
        if self.args.is_none() {
            return true;
        }

        // find FormalParameters. Should be the next parent of param, but this
        // is safer.
        let Some((params, params_id)) = symbol.iter_parents().find_map(|p| {
            let params = p.kind().as_formal_parameters()?;
            Some((params, p.id()))
        }) else {
            debug_assert!(false, "FormalParameter should always have a parent FormalParameters");
            return false;
        };

        if Self::is_allowed_param_because_of_method(semantic, param, params_id) {
            return true;
        }

        // Parameters are always checked. Must be done after above checks,
        // because in those cases a parameter is required. However, even if
        // `args` is `all`, it may be ignored using `ignoreRestSiblings` or `destructuredArrayIgnorePattern`.
        if self.args.is_all() {
            return false;
        }

        debug_assert_eq!(self.args, ArgsOption::AfterUsed);

        // from eslint rule documentation:
        // after-used - unused positional arguments that occur before the last
        // used argument will not be checked, but all named arguments and all
        // positional arguments after the last used argument will be checked.

        // unused non-positional arguments are never allowed
        if param.pattern.kind.is_destructuring_pattern() {
            return false;
        }

        // find the index of the parameter in the parameters list. We want to
        // check all parameters after this one for usages.
        let position =
            params.items.iter().enumerate().find(|(_, p)| p.span == param.span).map(|(i, _)| i);
        debug_assert!(
            position.is_some(),
            "could not find FormalParameter in a FormalParameters node that is its parent."
        );
        let Some(position) = position else {
            return false;
        };

        // This is the last parameter, so need to check for usages on following parameters
        if position == params.items.len() - 1 {
            return false;
        }

        let ctx = BindingContext { options: self, semantic, module_record };
        params
            .items
            .iter()
            .skip(position + 1)
            // has_modifier() to handle:
            // constructor(unused: number, public property: string) {}
            // no need to check if param is in a constructor, because if it's
            // not that's a parse error.
            .any(|p| p.has_modifier() || p.pattern.has_any_used_binding(ctx))
    }

    /// `params_id` is the [`NodeId`] to a [`AstKind::FormalParameters`] node.
    ///
    /// The following allowed conditions are handled:
    /// 1. setter parameters - removing them causes a syntax error.
    /// 2. TS constructor property definitions - they declare class members.
    fn is_allowed_param_because_of_method<'a>(
        semantic: &Semantic<'a>,
        param: &FormalParameter<'a>,
        params_id: NodeId,
    ) -> bool {
        let mut parents_iter = semantic.nodes().ancestor_kinds(params_id);

        // in function declarations, the parent immediately before the
        // FormalParameters is a TSDeclareBlock
        let Some(parent) = parents_iter.next() else {
            return false;
        };
        if matches!(parent, AstKind::Function(f) if f.r#type == FunctionType::TSDeclareFunction) {
            return true;
        }

        // for non-overloads, the next parent will be the function
        let Some(maybe_method_or_fn) = parents_iter.next() else {
            return false;
        };

        match maybe_method_or_fn {
            // arguments inside setters are allowed. Without them, the program
            // has invalid syntax
            AstKind::MethodDefinition(MethodDefinition {
                kind: MethodDefinitionKind::Set, ..
            })
            | AstKind::ObjectProperty(ObjectProperty { kind: PropertyKind::Set, .. }) => true,

            // Allow unused parameters in function overloads
            AstKind::Function(f)
                if f.body.is_none() || f.r#type == FunctionType::TSDeclareFunction =>
            {
                true
            }
            // Allow unused parameters in method overloads and overrides
            AstKind::MethodDefinition(method)
                if method.value.r#type == FunctionType::TSEmptyBodyFunctionExpression
                    || method.r#override =>
            {
                true
            }
            // constructor property definitions are allowed because they declare
            // class members
            // e.g. `class Foo { constructor(public a) {} }`
            AstKind::MethodDefinition(method) if method.kind.is_constructor() => {
                param.has_modifier()
            }
            // parameters in abstract methods will never be directly used b/c
            // abstract methods have no bodies. However, since this establishes
            // an API contract and gets used by subclasses, it is allowed.
            AstKind::MethodDefinition(method) if method.r#type.is_abstract() => true,
            _ => false,
        }
    }

    /// Returns `true` if this binding rest element should be allowed (i.e. not
    /// reported). Currently, this handles the case where a rest element is part
    /// of a TS function declaration.
    pub(super) fn is_allowed_binding_rest_element(symbol: &Symbol) -> bool {
        for parent in symbol.iter_parents() {
            // If this is a binding rest element that is part of a TS function parameter,
            // for example: `function foo(...messages: string[]) {}`, then we will allow it.
            if let AstKind::Function(f) = parent.kind() {
                return f.is_typescript_syntax();
            }
        }

        false
    }
}
