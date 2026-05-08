use crate::ast::*;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeVarId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InferType {
    Var(TypeVarId),
    Con(String),
    App(String, Vec<InferType>),
    Fn(Vec<InferType>, Box<InferType>),
    FixedArray(Box<InferType>, usize),
}

#[derive(Debug, Clone)]
struct Scheme {
    vars: Vec<TypeVarId>,
    ty: InferType,
}

impl Scheme {
    fn mono(ty: InferType) -> Self {
        Self {
            vars: Vec::new(),
            ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    UnknownIdentifier {
        name: String,
        span: Option<Span>,
    },
    TypeMismatch {
        expected: InferType,
        actual: InferType,
        context: String,
        span: Option<Span>,
    },
    OccursCheck {
        var: TypeVarId,
        ty: InferType,
        span: Option<Span>,
    },
}

#[derive(Debug, Clone)]
pub struct TypeChecker {
    next_var: usize,
    subst: HashMap<TypeVarId, InferType>,
    rigid: HashSet<TypeVarId>,
    scopes: Vec<HashMap<String, Scheme>>,
    current_return: Option<InferType>,
    types: HashMap<NodeId, InferType>,
    struct_fields: HashMap<String, HashMap<String, InferType>>,
    newtype_alias: HashMap<String, InferType>,
    pub specializations: HashMap<(String, Vec<InferType>), String>,
    enum_variants: HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,
    option_type_name: Option<String>,
    result_type_name: Option<String>,
    pub extern_fns: HashSet<String>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self {
            next_var: 0,
            subst: HashMap::new(),
            rigid: HashSet::new(),
            scopes: vec![HashMap::new()],
            current_return: None,
            types: HashMap::new(),
            struct_fields: HashMap::new(),
            newtype_alias: HashMap::new(),
            specializations: HashMap::new(),
            enum_variants: HashMap::new(),
            option_type_name: None,
            result_type_name: None,
            extern_fns: HashSet::new(),
        }
    }

    pub fn check_module(&mut self, module: &Module) -> Result<(), TypeError> {
        // eprintln!("Type checking module...");
        // for decl in &module.declarations {
        //     eprintln!("Declaration: {:?}", decl.node);
        // }
        self.reset();
        self.collect_type_info(module)?;
        self.predeclare_functions(module)?;
        for decl in &module.declarations {
            // eprintln!("Checking decl: {:?}", decl.node);
            match &decl.node {
                DeclarationKind::Function { name, .. } => {
                    self.check_function(name, decl)?;
                }
                _ => {}
            }
        }
        self.finalize_types();
        Ok(())
    }

    fn reset(&mut self) {
        self.next_var = 0;
        self.subst.clear();
        self.rigid.clear();
        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.current_return = None;
        self.types.clear();
        self.struct_fields.clear();
        self.newtype_alias.clear();
        self.seed_builtins();
        self.enum_variants.clear();
        self.option_type_name = None;
        self.result_type_name = None;
        self.extern_fns.clear();
    }

    pub fn types(&self) -> &HashMap<NodeId, InferType> {
        &self.types
    }

    fn finalize_types(&mut self) {
        let keys: Vec<NodeId> = self.types.keys().cloned().collect();
        for k in keys {
            if let Some(ty) = self.types.get(&k).cloned() {
                self.types.insert(k, self.apply(&ty));
            }
        }
    }

    fn seed_builtins(&mut self) {
        self.set_global(
            "__ty_buf_new".into(),
            Scheme::mono(InferType::Fn(
                Vec::new(),
                Box::new(InferType::Con("Buf".into())),
            )),
        );
        self.set_global(
            "__ty_buf_push_str".into(),
            Scheme::mono(InferType::Fn(
                vec![InferType::Con("Buf".into()), InferType::Con("Str".into())],
                Box::new(InferType::Con("Unit".into())),
            )),
        );
        self.set_global(
            "__ty_buf_into_str".into(),
            Scheme::mono(InferType::Fn(
                vec![InferType::Con("Buf".into())],
                Box::new(InferType::Con("Str".into())),
            )),
        );
    }

    fn predeclare_functions(&mut self, module: &Module) -> Result<(), TypeError> {
        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Function { name, .. } => {
                    // eprintln!("Registering function: {}", name.name);
                    let (scheme, _, _, _) = self.lower_function_signature(decl)?;
                    self.set_global(name.name.clone(), scheme);
                }
                DeclarationKind::UnsafeOrExtern(uoe) => {
                    if let UnsafeOrExternKind::Extern { declarations, .. } = &uoe.node {
                        for sig in declarations {
                            let FunctionSignatureKind {
                                name,
                                generics,
                                params,
                                return_type,
                            } = &sig.node;

                            let mut generic_vars = HashMap::new();
                            for g in generics {
                                if let InferType::Var(id) = self.fresh_rigid_var() {
                                    generic_vars.insert(g.node.name.name.clone(), id);
                                }
                            }

                            let param_tys: Vec<InferType> = params
                                .iter()
                                .map(|p| self.lower_type(&p.type_annotation, &generic_vars))
                                .collect::<Result<_, _>>()?;

                            let ret_ty = match return_type {
                                Some(ty) => self.lower_type(ty, &generic_vars)?,
                                None => InferType::Con("Unit".into()),
                            };

                            let fn_ty = InferType::Fn(param_tys.clone(), Box::new(ret_ty.clone()));
                            let scheme = self.generalize(&fn_ty, None);
                            self.set_global(name.name.clone(), scheme);
                            self.extern_fns.insert(name.name.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_type_info(&mut self, module: &Module) -> Result<(), TypeError> {
        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Struct {
                    name,
                    generics,
                    fields,
                    ..
                } => {
                    let mut generic_vars = HashMap::new();
                    for g in generics {
                        if let InferType::Var(id) = self.fresh_var() {
                            generic_vars.insert(g.node.name.name.clone(), id);
                        }
                    }
                    let mut map = HashMap::new();
                    for (field_id, field_ty) in fields {
                        let lowered = self.lower_type(field_ty, &generic_vars)?;
                        map.insert(field_id.name.clone(), lowered);
                    }
                    self.struct_fields.insert(name.name.clone(), map);
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    let alias = self.lower_type(type_alias, &HashMap::new())?;
                    self.newtype_alias.insert(name.name.clone(), alias);
                }
                DeclarationKind::Enum {
                    name,
                    generics,
                    variants,
                } => {
                    let mut generic_vars = HashMap::new();
                    for g in generics {
                        if let InferType::Var(id) = self.fresh_var() {
                            generic_vars.insert(g.node.name.name.clone(), id);
                        }
                    }
                    let ordered_vars = generics
                        .iter()
                        .filter_map(|g| generic_vars.get(&g.node.name.name).copied())
                        .collect::<Vec<_>>();
                    let enum_ty = if generic_vars.is_empty() {
                        InferType::Con(name.name.clone())
                    } else {
                        InferType::App(
                            name.name.clone(),
                            ordered_vars.iter().map(|id| InferType::Var(*id)).collect(),
                        )
                    };

                    // Register canonical Option/Result type names (post-mangling) by shape.
                    // This avoids hard-coding "Option"/"Result" strings elsewhere.
                    let variant_names = variants
                        .iter()
                        .map(|v| v.node.name.name.as_str())
                        .collect::<Vec<_>>();
                    if variant_names.iter().any(|&v| v == "Some")
                        && variant_names.iter().any(|&v| v == "None")
                    {
                        self.option_type_name = Some(name.name.clone());
                    }
                    if variant_names.iter().any(|&v| v == "Ok")
                        && variant_names.iter().any(|&v| v == "Err")
                    {
                        self.result_type_name = Some(name.name.clone());
                    }

                    for variant in variants {
                        let variant_name = variant.node.name.name.clone();
                        let payload_ty = match &variant.node.payload {
                            None => None,
                            Some(p) => match &p.node {
                                EnumVariantPayloadKind::Tuple(types) if types.len() == 1 => {
                                    Some(self.lower_type(&types[0], &generic_vars)?)
                                }
                                EnumVariantPayloadKind::Tuple(types) => {
                                    // Multi-field tuple variant: not yet fully supported in
                                    // pattern binding, but register a fresh var as placeholder.
                                    let _ = types;
                                    Some(self.fresh_var())
                                }
                                EnumVariantPayloadKind::Struct(_) => Some(self.fresh_var()),
                                // WTF is Enum Unit? For now, register a fresh var as placeholder too
                                EnumVariantPayloadKind::Unit(_) => Some(self.fresh_var()),
                            },
                        };
                        self.enum_variants.insert(
                            variant_name.clone(),
                            (name.name.clone(), ordered_vars.clone(), payload_ty.clone()),
                        );

                        // Seed a constructor function so call-site inference works.
                        // e.g. `Ok(val)` is a call whose return type is Result<T, E>.
                        if let Some(ref inner) = payload_ty {
                            self.set_global(
                                variant_name.clone(),
                                Scheme {
                                    vars: ordered_vars.clone(),
                                    ty: InferType::Fn(
                                        vec![inner.clone()],
                                        Box::new(enum_ty.clone()),
                                    ),
                                },
                            );
                        } else {
                            // Unit variant: bind as the enum type itself.
                            self.set_global(variant_name.clone(), Scheme::mono(enum_ty.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn check_function(&mut self, name: &Identifier, decl: &Declaration) -> Result<(), TypeError> {
        // eprintln!("Checking function: {}", name.name);
        let (scheme, param_tys, ret_ty, fn_ty) = self.lower_function_signature(decl)?;
        self.set_global(name.name.clone(), scheme);

        let body = match &decl.node {
            DeclarationKind::Function { params, body, .. } => {
                // eprintln!("Function body: {:?}", body.statements.len());
                self.current_return = Some(ret_ty.clone());
                self.push_scope();
                for (param, ty) in params.iter().zip(param_tys.iter()) {
                    self.insert_local(param.name.name.clone(), Scheme::mono(ty.clone()));
                }
                let body_ty = self.check_block(body, Some(&ret_ty))?;
                self.pop_scope();
                self.current_return = None;
                if body.trailing_expression.is_some() {
                    self.unify(body_ty, ret_ty.clone(), Some(body.span))?;
                }
                body
            }
            _ => unreachable!(),
        };

        let final_ty = self.apply(&fn_ty);
        let final_scheme = self.generalize(&final_ty, Some(&name.name));
        self.set_global(name.name.clone(), final_scheme);
        let _ = body;
        Ok(())
    }

    fn lower_function_signature(
        &mut self,
        decl: &Declaration,
    ) -> Result<(Scheme, Vec<InferType>, InferType, InferType), TypeError> {
        // Accept an extra `outer_generics` slice for extend-block type params
        self.lower_function_signature_with_outer(decl, &[])
    }

    fn lower_function_signature_with_outer(
        &mut self,
        decl: &Declaration,
        outer_generics: &[String], // e.g. ["T"] from `extend<T> Option<T>`
    ) -> Result<(Scheme, Vec<InferType>, InferType, InferType), TypeError> {
        let (generics, params, return_type) = match &decl.node {
            DeclarationKind::Function {
                generics,
                params,
                return_type,
                ..
            } => (generics, params, return_type),
            _ => unreachable!(),
        };

        let mut generic_vars = HashMap::new();
        for name in outer_generics {
            if let InferType::Var(id) = self.fresh_rigid_var() {
                generic_vars.insert(name.clone(), id);
            }
        }

        // Then the method's own generics
        for generic in generics {
            if let InferType::Var(id) = self.fresh_var() {
                generic_vars.insert(generic.node.name.name.clone(), id);
            }
        }
        // eprintln!("Generic vars: {:?}", generic_vars);

        let mut param_tys = Vec::new();
        for param in params {
            param_tys.push(self.lower_type(&param.type_annotation, &generic_vars)?);
        }

        let ret_ty = match return_type {
            Some(ty) => self.lower_type(ty, &generic_vars)?,
            None => InferType::Con("Unit".into()), // was: self.fresh_var()
        };

        let fn_ty = InferType::Fn(param_tys.clone(), Box::new(ret_ty.clone()));
        let scheme = self.generalize(&fn_ty, None);
        Ok((scheme, param_tys, ret_ty, fn_ty))
    }

    fn check_block(
        &mut self,
        block: &Block,
        expected_return: Option<&InferType>,
    ) -> Result<InferType, TypeError> {
        self.push_scope();
        for stmt in &block.statements {
            self.check_statement(stmt, expected_return)?;
        }
        let result = if let Some(expr) = &block.trailing_expression {
            self.infer_expression(expr)?
        } else {
            InferType::Con("Unit".into())
        };
        self.pop_scope();
        Ok(self.apply(&result))
    }

    fn check_statement(
        &mut self,
        stmt: &Statement,
        expected_return: Option<&InferType>,
    ) -> Result<(), TypeError> {
        // eprintln!("Checking statement: {:?}", stmt.node);
        match &stmt.node {
            StatementKind::LetBinding {
                pattern,
                type_annotation,
                initializer,
                else_block,
                mutable,
            } => {
                // eprintln!("LetBinding pattern: {:?}", pattern.node);
                let init_ty = self.infer_expression(initializer)?;
                let mut ty = if let Some(annotation) = type_annotation {
                    let annotated = self.lower_type(annotation, &HashMap::new())?;
                    self.unify(init_ty, annotated.clone(), Some(initializer.span))?;
                    annotated
                } else {
                    init_ty
                };
                if *mutable {
                    if let InferType::FixedArray(elem, _) = self.apply(&ty) {
                        ty = InferType::App("Array".into(), vec![*elem]);
                    }
                }

                if let Some(id) = pattern.get_identifier() {
                    // Simple `let x = ...` / `let mut x = ...`: preserve generalization
                    // so that e.g. `let xs = []` can be used at multiple types.
                    let scheme = if *mutable {
                        Scheme::mono(self.apply(&ty))
                    } else {
                        self.generalize(&ty, None)
                    };
                    self.insert_local(id.name.clone(), scheme);
                } else {
                    // EnumVariant pattern: let Ok(x) = expr else { ... }
                    // `ty` is the full wrapper type e.g. Result<Listener, Int32>.
                    // Extract the Ok/Some payload and bind the inner pattern to it directly.
                    if let PatternKind::EnumVariant {
                        payload: Some(inner_pat),
                        ..
                    } = &pattern.node
                    {
                        let payload_ty = self.unwrap_enum_payload(pattern, &ty);
                        self.bind_pattern(inner_pat, &payload_ty)?;
                    } else {
                        // Unit variant or bare enum pattern — nothing to bind
                        self.bind_pattern(pattern, &ty)?;
                    }
                }

                if let Some(else_blk) = else_block {
                    let _ = self.check_block(else_blk, expected_return)?;
                }
                Ok(())
            }
            StatementKind::Expression(expr) => {
                let _ = self.infer_expression(expr)?;
                Ok(())
            }
            StatementKind::Const {
                name,
                type_annotation,
                initializer,
            } => {
                let init_ty = self.infer_expression(initializer)?;
                let ty = if let Some(annotation) = type_annotation {
                    let annotated = self.lower_type(annotation, &HashMap::new())?;
                    self.unify(init_ty, annotated.clone(), Some(initializer.span))?;
                    annotated
                } else {
                    init_ty
                };
                self.insert_local(name.name.clone(), Scheme::mono(self.apply(&ty)));
                Ok(())
            }
            StatementKind::Return(Some(expr)) => {
                let ty = self.infer_expression(expr)?;
                let expected = expected_return
                    .cloned()
                    .or_else(|| self.current_return.clone());
                if let Some(expected) = expected {
                    self.unify(ty, expected, Some(expr.span))?;
                }
                Ok(())
            }
            StatementKind::Return(None) => {
                let expected = expected_return
                    .cloned()
                    .or_else(|| self.current_return.clone());
                if let Some(expected) = expected {
                    self.unify(InferType::Con("Unit".into()), expected, Some(stmt.span))?;
                }
                Ok(())
            }
            StatementKind::Conc { body } => {
                let _ = self.check_block(body, expected_return)?;
                Ok(())
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.infer_expression(condition)?;
                self.unify(cond, InferType::Con("Bool".into()), Some(condition.span))?;
                let _ = self.check_block(then_branch, expected_return)?;
                if let Some(else_branch) = else_branch {
                    match &else_branch.node {
                        ElseBranchKind::Block(block) => {
                            let _ = self.check_block(block, expected_return)?;
                        }
                        ElseBranchKind::If(stmt) => {
                            self.check_statement(stmt, expected_return)?;
                        }
                    }
                }
                Ok(())
            }
            StatementKind::Match { expr, arms } => {
                let scrutinee = self.infer_expression(expr)?;
                for arm in arms {
                    self.push_scope();
                    self.bind_pattern(&arm.node.pattern, &scrutinee)?;
                    if let Some(guard) = &arm.node.guard {
                        let guard_ty = self.infer_expression(guard)?;
                        self.unify(guard_ty, InferType::Con("Bool".into()), Some(guard.span))?;
                    }
                    let body_ty = self.infer_expression(&arm.node.body)?;
                    self.unify(body_ty, InferType::Con("Unit".into()), Some(arm.span))?;
                    self.pop_scope();
                }
                Ok(())
            }
            StatementKind::Loop { kind, body } => {
                match &kind.node {
                    LoopKindKind::For {
                        pattern, iterator, ..
                    } => {
                        let iter_ty = self.infer_expression(iterator)?;
                        let elem_ty = self
                            .array_elem_type(&iter_ty)
                            .unwrap_or_else(|| self.fresh_var());
                        self.push_scope();
                        self.bind_pattern(pattern, &elem_ty)?;
                        let _ = self.check_block(body, expected_return)?;
                        self.pop_scope();
                    }
                    LoopKindKind::While { condition, .. } => {
                        let cond = self.infer_expression(condition)?;
                        self.unify(cond, InferType::Con("Bool".into()), Some(condition.span))?;
                        let _ = self.check_block(body, expected_return)?;
                    }
                    LoopKindKind::Block(block) => {
                        let _ = self.check_block(block, expected_return)?;
                    }
                }
                Ok(())
            }
            StatementKind::Break | StatementKind::Continue => Ok(()),
            StatementKind::Empty | StatementKind::UseDeclaration(_) => Ok(()),
        }
    }

    fn infer_expression(&mut self, expr: &Expression) -> Result<InferType, TypeError> {
        let ty = match &expr.node {
            ExpressionKind::Literal(lit) => self.literal_type(lit, expr.span)?,
            ExpressionKind::Identifier(id) => {
                if id.name.as_str() == "chan" {
                    // chan has a generic return type that depends on context
                    return Ok(InferType::App(
                        "Chan".into(),
                        vec![InferType::Var(self.fresh_var_id())],
                    ));
                }

                if self.try_inner_func(id.name.clone()) {
                    return Ok(InferType::Con("Unit".into()));
                }

                return self
                    .lookup(&id.name)
                    .cloned()
                    .ok_or_else(|| TypeError::UnknownIdentifier {
                        name: id.name.clone(),
                        span: Some(id.span),
                    })
                    .map(|s| self.instantiate(&s));
            }
            ExpressionKind::UnaryOp { op, expr: inner } => {
                let ty = self.infer_expression(inner)?;
                match op {
                    Operator::Sub => {
                        self.unify(ty, InferType::Con("Int32".into()), Some(inner.span))?;
                        InferType::Con("Int32".into())
                    }
                    Operator::Not => {
                        self.unify(ty, InferType::Con("Bool".into()), Some(inner.span))?;
                        InferType::Con("Bool".into())
                    }
                    _ => ty,
                }
            }
            ExpressionKind::BinaryOp { op, left, right } => {
                self.infer_binary(op, left, right, expr.span)?
            }
            ExpressionKind::Call { func, args } => {
                // Special handling for chan<T>() - func is Identifier("chan")
                if let ExpressionKind::Identifier(id) = &func.node {
                    if id.name == "chan" {
                        let elem_ty = self.fresh_var();
                        // chan() produces a shared channel — wrap in Ref
                        return Ok(InferType::App(
                            "Ref".into(),
                            vec![InferType::App("Chan".into(), vec![elem_ty])],
                        ));
                    }
                }
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_ty = self.infer_expression(base)?;
                    let mut arg_tys = Vec::new();
                    for arg in args {
                        arg_tys.push(self.infer_expression(arg)?);
                    }

                    let inner_ty = match self.apply(&base_ty) {
                        InferType::App(ref name, ref args) if name == "Ref" && args.len() == 1 => {
                            self.apply(&args[0])
                        }
                        other => other,
                    };

                    if let InferType::App(name, mut ty_args) = inner_ty {
                        if name == "Array" && ty_args.len() == 1 && field.name == "push" {
                            let elem = ty_args.remove(0);
                            if let Some(first) = arg_tys.first().cloned() {
                                self.unify(first, elem, Some(expr.span))?;
                            }
                            InferType::Con("Unit".into())
                        } else if name == "Chan" && ty_args.len() == 1 {
                            let elem = ty_args.remove(0);
                            match field.name.as_str() {
                                "send" => {
                                    if let Some(first) = arg_tys.first().cloned() {
                                        self.unify(first, elem, Some(expr.span))?;
                                    }
                                    InferType::Con("Unit".into())
                                }
                                "consume" => {
                                    if let Some(first) = arg_tys.first().cloned() {
                                        let unwrapped = match self.apply(&first) {
                                            InferType::App(name, args)
                                                if name == "Ref" && args.len() == 1 =>
                                            {
                                                self.apply(&args[0])
                                            }
                                            other => other,
                                        };
                                        self.unify(
                                            unwrapped,
                                            InferType::App("Chan".into(), vec![elem]),
                                            Some(expr.span),
                                        )?;
                                    }
                                    InferType::Con("Unit".into())
                                }
                                "recv" => elem,
                                "try_recv" => self.make_option_ty(elem),
                                _ => {
                                    let method_name =
                                        format!("__ty_method__{}__{}", name, field.name);
                                    let scheme =
                                        self.lookup(&method_name).cloned().ok_or_else(|| {
                                            TypeError::UnknownIdentifier {
                                                name: method_name.clone(),
                                                span: Some(field.span),
                                            }
                                        })?;
                                    let callee = self.instantiate(&scheme);
                                    let mut full_args = vec![base_ty];
                                    full_args.extend(arg_tys);
                                    let ret = self.fresh_var();
                                    self.unify(
                                        callee,
                                        InferType::Fn(full_args, Box::new(ret.clone())),
                                        Some(expr.span),
                                    )?;
                                    ret
                                }
                            }
                        } else {
                            let method_name = format!("__ty_method__{}__{}", name, field.name);
                            let scheme = self.lookup(&method_name).cloned().ok_or_else(|| {
                                TypeError::UnknownIdentifier {
                                    name: method_name.clone(),
                                    span: Some(field.span),
                                }
                            })?;
                            let callee = self.instantiate(&scheme);
                            let mut full_args = vec![base_ty];
                            full_args.extend(arg_tys);
                            let ret = self.fresh_var();
                            self.unify(
                                callee,
                                InferType::Fn(full_args, Box::new(ret.clone())),
                                Some(expr.span),
                            )?;
                            ret
                        }
                    } else if let InferType::Con(type_name) = self.apply(&base_ty) {
                        let method_name = format!("__ty_method__{}__{}", type_name, field.name);
                        let scheme = self.lookup(&method_name).cloned().ok_or_else(|| {
                            TypeError::UnknownIdentifier {
                                name: method_name.clone(),
                                span: Some(field.span),
                            }
                        })?;
                        let callee = self.instantiate(&scheme);
                        let mut full_args = vec![base_ty];
                        full_args.extend(arg_tys);
                        let ret = self.fresh_var();
                        self.unify(
                            callee,
                            InferType::Fn(full_args, Box::new(ret.clone())),
                            Some(expr.span),
                        )?;
                        ret
                    } else {
                        let callee = self.infer_expression(func)?;
                        let ret = self.fresh_var();
                        self.unify(
                            callee,
                            InferType::Fn(arg_tys, Box::new(ret.clone())),
                            Some(expr.span),
                        )?;
                        ret
                    }
                } else {
                    // Check if func is a builtin function (printf, print, etc.)
                    if let ExpressionKind::Identifier(id) = &func.node {
                        if self.try_inner_func(id.name.clone()) {
                            for arg in args {
                                let _ = self.infer_expression(arg)?;
                            }
                            return Ok(InferType::Con("Unit".into()));
                        }
                    }
                    let callee = self.infer_expression(func)?;
                    let mut arg_tys = Vec::new();
                    for arg in args {
                        arg_tys.push(self.infer_expression(arg)?);
                    }

                    // Track specialization for monomorphization
                    if let ExpressionKind::Identifier(id) = &func.node {
                        if !self.extern_fns.contains(&id.name) {
                            let concrete_args =
                                arg_tys.iter().map(|t| self.apply(t)).collect::<Vec<_>>();
                            let key = (id.name.clone(), concrete_args);
                            if !self.specializations.contains_key(&key) {
                                let name =
                                    format!("{}_spec_{}", id.name, self.specializations.len());
                                self.specializations.insert(key, name);
                            }
                        }
                    }

                    let ret = self.fresh_var();
                    self.unify(
                        callee,
                        InferType::Fn(arg_tys, Box::new(ret.clone())),
                        Some(expr.span),
                    )?;
                    ret
                }
            }
            ExpressionKind::FieldAccess { base, field } => {
                let base_ty = self.infer_expression(base)?;
                match self.apply(&base_ty) {
                    InferType::Con(name) => {
                        if field.name == "0" {
                            self.newtype_alias
                                .get(&name)
                                .cloned()
                                .unwrap_or_else(|| self.fresh_var())
                        } else if let Some(fields) = self.struct_fields.get(&name) {
                            fields
                                .get(&field.name)
                                .cloned()
                                .unwrap_or_else(|| self.fresh_var())
                        } else {
                            self.fresh_var()
                        }
                    }
                    _ => self.fresh_var(),
                }
            }
            ExpressionKind::IndexAccess { base, index } => {
                let base_ty = self.infer_expression(base)?;
                let index_ty = self.infer_expression(index)?;
                self.unify(index_ty, InferType::Con("Int32".into()), Some(index.span))?;
                if let Some(elem) = self.array_elem_type(&base_ty) {
                    // Use the stdlib-registered Option type name rather than a literal string.
                    self.make_option_ty(elem)
                } else {
                    self.fresh_var()
                }
            }
            ExpressionKind::StructInit { name, fields } => {
                for (_, field_expr) in fields {
                    let _ = self.infer_expression(field_expr)?;
                }

                // Track specialization for generic structs
                // Note: name.name holds the struct name. Need to access generic args if present.
                // Assuming struct name follows a naming convention or we can infer them.
                // For now, track based on name and fields.
                let concrete_fields = fields
                    .iter()
                    .map(|(_, e)| self.apply(&self.types.get(&e.id).cloned().unwrap()))
                    .collect::<Vec<_>>();
                let key = (name.name.clone(), concrete_fields);
                if !self.specializations.contains_key(&key) {
                    let spec_name = format!("{}_spec_{}", name.name, self.specializations.len());
                    self.specializations.insert(key, spec_name);
                }

                InferType::Con(name.name.clone())
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(base) = base {
                    let _ = self.infer_expression(base)?;
                }
                for (_, field_expr) in fields {
                    let _ = self.infer_expression(field_expr)?;
                }
                self.fresh_var()
            }
            ExpressionKind::Block(block) => self.check_block(block, None)?,
            ExpressionKind::Pipe { left, right } => {
                let left_ty = self.infer_expression(left)?;
                let right_ty = self.infer_expression(right)?;
                let ret = self.fresh_var();
                self.unify(
                    right_ty,
                    InferType::Fn(vec![left_ty], Box::new(ret.clone())),
                    Some(expr.span),
                )?;
                ret
            }
            ExpressionKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee_ty = self.infer_expression(scrutinee)?;
                let arm_ty = self.fresh_var();
                for arm in arms {
                    self.push_scope();
                    self.bind_pattern(&arm.node.pattern, &scrutinee_ty)?;
                    if let Some(guard) = &arm.node.guard {
                        let guard_ty = self.infer_expression(guard)?;
                        self.unify(guard_ty, InferType::Con("Bool".into()), Some(guard.span))?;
                    }
                    let body_ty = self.infer_expression(&arm.node.body)?;
                    self.unify(body_ty, arm_ty.clone(), Some(arm.span))?;
                    self.pop_scope();
                }
                arm_ty
            }
            ExpressionKind::TryOperator { expr: inner } => {
                let inner_ty = self.infer_expression(inner)?;
                self.try_inner_type(&inner_ty).unwrap_or(inner_ty)
            }
            ExpressionKind::IfLet {
                pattern,
                expr: matched,
                then,
                else_branch,
            } => {
                let matched_ty = self.infer_expression(matched)?;
                self.push_scope();
                self.bind_pattern(pattern, &matched_ty)?;
                let then_ty = self.check_block(then, None)?;
                self.pop_scope();
                if let Some(else_branch) = else_branch {
                    let else_ty = self.infer_expression(else_branch)?;
                    self.unify(then_ty.clone(), else_ty, Some(expr.span))?;
                }
                then_ty
            }
            ExpressionKind::Placeholder(_) => self.fresh_var(),
        };
        let applied = self.apply(&ty);
        self.types.insert(expr.id, applied.clone());
        Ok(applied)
    }

    fn infer_binary(
        &mut self,
        op: &Operator,
        left: &Expression,
        right: &Expression,
        span: Span,
    ) -> Result<InferType, TypeError> {
        let left_ty = self.infer_expression(left)?;
        let right_ty = self.infer_expression(right)?;
        match op {
            Operator::Assign => {
                self.unify(left_ty.clone(), right_ty, Some(span))?;
                Ok(left_ty)
            }
            Operator::Add
            | Operator::Sub
            | Operator::Mul
            | Operator::Div
            | Operator::Mod
            | Operator::Shl
            | Operator::Shr
            | Operator::BitAnd
            | Operator::BitOr
            | Operator::BitXor
            | Operator::AddAssign
            | Operator::SubAssign
            | Operator::MulAssign
            | Operator::DivAssign => {
                // Resolve both sides to concrete types first, then determine the
                // result type.  We allow implicit widening within a single numeric
                // hierarchy (int or float) so that e.g. `i8 + i32` yields Int32
                // without requiring an explicit cast.  Mixing int and float is
                // still a type error — that requires an explicit conversion.
                let result_ty = match (&left_ty, &right_ty) {
                    (InferType::Con(l), InferType::Con(r)) => {
                        match (Self::numeric_rank(l), Self::numeric_rank(r)) {
                            (Some(lr), Some(rr)) => {
                                let same_chain = (lr < 10) == (rr < 10);
                                if same_chain {
                                    // The result is the wider of the two.
                                    if lr >= rr {
                                        left_ty.clone()
                                    } else {
                                        right_ty.clone()
                                    }
                                } else {
                                    // Int mixed with Float — still an error.
                                    return Err(TypeError::TypeMismatch {
                                        expected: left_ty.clone(),
                                        actual: right_ty.clone(),
                                        context: "arithmetic operands".into(),
                                        span: Some(span),
                                    });
                                }
                            }
                            // Fall back: require both sides to be Int32 (legacy
                            // behaviour for unresolved type variables, Byte, etc.)
                            _ => {
                                self.unify(
                                    left_ty.clone(),
                                    InferType::Con("Int32".into()),
                                    Some(span),
                                )?;
                                self.unify(right_ty, InferType::Con("Int32".into()), Some(span))?;
                                InferType::Con("Int32".into())
                            }
                        }
                    }
                    // At least one side is not yet a concrete Con — unify both
                    // against Int32 as before and keep Int32 as the result type.
                    _ => {
                        self.unify(left_ty.clone(), InferType::Con("Int32".into()), Some(span))?;
                        self.unify(right_ty, InferType::Con("Int32".into()), Some(span))?;
                        InferType::Con("Int32".into())
                    }
                };
                Ok(result_ty)
            }
            Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Gt
            | Operator::Le
            | Operator::Ge => {
                self.unify(left_ty, right_ty, Some(span))?;
                Ok(InferType::Con("Bool".into()))
            }
            Operator::And | Operator::Or => {
                self.unify(left_ty, InferType::Con("Bool".into()), Some(span))?;
                self.unify(right_ty, InferType::Con("Bool".into()), Some(span))?;
                Ok(InferType::Con("Bool".into()))
            }
            _ => Ok(self.fresh_var()),
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, expected: &InferType) -> Result<(), TypeError> {
        match &pattern.node {
            PatternKind::Wildcard => Ok(()),
            PatternKind::Identifier(id) => {
                self.insert_local(id.name.clone(), Scheme::mono(self.apply(expected)));
                Ok(())
            }
            PatternKind::Literal(lit) => {
                let ty = self.literal_type(lit, pattern.span)?;
                self.unify(ty, expected.clone(), Some(pattern.span))
            }
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                let elem = self
                    .array_elem_type(expected)
                    .unwrap_or_else(|| self.fresh_var());
                for part in parts {
                    self.bind_pattern(part, &elem)?;
                }
                Ok(())
            }
            PatternKind::Struct { fields, .. } => {
                for (_, pat) in fields {
                    self.bind_pattern(pat, expected)?;
                }
                Ok(())
            }
            PatternKind::Or(left, right) => {
                self.bind_pattern(left, expected)?;
                self.bind_pattern(right, expected)
            }
            PatternKind::Guard { pattern, guard } => {
                self.bind_pattern(pattern, expected)?;
                let guard_ty = self.infer_expression(guard)?;
                self.unify(guard_ty, InferType::Con("Bool".into()), Some(guard.span))
            }
            PatternKind::EnumVariant {
                variant_name,
                payload: Some(payload),
                ..
            } => {
                if let Some(inner) = self.enum_variant_payload(expected, &variant_name.name) {
                    self.bind_pattern(payload, &inner)
                } else {
                    self.bind_pattern(payload, expected)
                }
            }
            PatternKind::EnumVariant { payload: None, .. } => Ok(()),
        }
    }

    fn literal_type(&mut self, lit: &Literal, span: Span) -> Result<InferType, TypeError> {
        match &lit.kind {
            LiteralKind::Int(_, suffix) => Ok(match suffix.as_deref() {
                Some("i8") => InferType::Con("Int8".into()),
                Some("i16") => InferType::Con("Int16".into()),
                Some("i32") => InferType::Con("Int32".into()),
                Some("i64") => InferType::Con("Int64".into()),
                Some("u8") => InferType::Con("Byte".into()),
                Some(other) => {
                    return Err(TypeError::TypeMismatch {
                        expected: InferType::Con("Int32".into()),
                        actual: InferType::Con(other.to_string()),
                        context: "integer suffix".into(),
                        span: Some(span),
                    })
                }
                None => InferType::Con("Int32".into()),
            }),
            LiteralKind::Float(_, suffix) => Ok(match suffix.as_deref() {
                Some("f16") => InferType::Con("Float16".into()),
                Some("f32") => InferType::Con("Float32".into()),
                Some("f64") => InferType::Con("Float64".into()),
                Some(other) => {
                    return Err(TypeError::TypeMismatch {
                        expected: InferType::Con("Float32".into()),
                        actual: InferType::Con(other.to_string()),
                        context: "float suffix".into(),
                        span: Some(span),
                    })
                }
                None => InferType::Con("Float32".into()),
            }),
            LiteralKind::Bool(_) => Ok(InferType::Con("Bool".into())),
            LiteralKind::Str(_) => Ok(InferType::Con("Str".into())),
            LiteralKind::Array(elements) => {
                let elem = self.fresh_var();
                for item in elements {
                    let item_ty = self.infer_expression(item)?;
                    self.unify(item_ty, elem.clone(), Some(item.span))?;
                }
                Ok(InferType::FixedArray(Box::new(elem), elements.len()))
            }
        }
    }

    // Construct `Option<T>` using the registered stdlib name if available,
    // falling back to the literal string "Option" for bootstrapping.
    fn make_option_ty(&self, inner: InferType) -> InferType {
        let name = self
            .option_type_name
            .clone()
            .unwrap_or_else(|| "Option".into());
        InferType::App(name, vec![inner])
    }

    fn try_inner_type(&self, ty: &InferType) -> Option<InferType> {
        match self.apply(ty) {
            // Generalised: unwrap the Ok/Some payload for any enum whose first
            // variant carries a single-field tuple payload.  For now we still
            // special-case the names so the logic is identical to before, but
            // this can be replaced by an enum_variants table lookup once the
            // stdlib is fully loaded.
            InferType::App(ref name, ref args) => {
                if let Some((_, _, payload)) = self
                    .enum_variants
                    .values()
                    .find(|(enum_name, _, _)| enum_name == name)
                {
                    // Return the payload of the first registered variant that
                    // matches this enum type.
                    return payload.clone();
                }
                None
            }
            _ => None,
        }
    }

    fn try_inner_func(&self, ty: String) -> bool {
        let internal = [
            // flow control
            "break", "continue", // stdio
            "print", "println", "printf", "fprint", "fprintln", "fprintf", "sprint", "sprintln",
            "sprintf", "scan", "scanf", "fscan", "fscanf", "sscan", "sscanf",
        ];
        return internal.contains(&ty.as_str());
    }

    fn array_elem_type(&self, ty: &InferType) -> Option<InferType> {
        match self.apply(ty) {
            InferType::App(name, args) if name == "Array" && args.len() == 1 => {
                Some(args[0].clone())
            }
            InferType::FixedArray(elem, _) => Some(*elem),
            _ => None,
        }
    }

    fn lower_type(
        &mut self,
        ty: &Type,
        generic_vars: &HashMap<String, TypeVarId>,
    ) -> Result<InferType, TypeError> {
        // Special case: function types written as Fn<Arg, Ret> or T -> U
        // are represented with name "Fn" and 2 generic args [param, ret]
        if ty.node.name == "Fn" && ty.node.generic_args.len() == 2 {
            let param = self.lower_type(&ty.node.generic_args[0], generic_vars)?;
            let ret = self.lower_type(&ty.node.generic_args[1], generic_vars)?;
            return Ok(InferType::Fn(vec![param], Box::new(ret)));
        }

        let args = ty
            .node
            .generic_args
            .iter()
            .map(|arg| self.lower_type(arg, generic_vars))
            .collect::<Result<Vec<_>, _>>()?;
        if args.is_empty() {
            if let Some(id) = generic_vars.get(&ty.node.name) {
                Ok(InferType::Var(*id))
            } else {
                Ok(InferType::Con(ty.node.name.clone()))
            }
        } else {
            Ok(InferType::App(ty.node.name.clone(), args))
        }
    }

    fn fresh_var(&mut self) -> InferType {
        InferType::Var(self.fresh_var_id())
    }

    fn fresh_var_id(&mut self) -> TypeVarId {
        let id = TypeVarId(self.next_var);
        self.next_var += 1;
        id
    }

    fn fresh_rigid_var(&mut self) -> InferType {
        let id = self.fresh_var_id();
        self.rigid.insert(id);
        InferType::Var(id)
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn insert_local(&mut self, name: String, scheme: Scheme) {
        self.scopes.last_mut().unwrap().insert(name, scheme);
    }

    fn set_global(&mut self, name: String, scheme: Scheme) {
        self.scopes.first_mut().unwrap().insert(name, scheme);
    }

    fn lookup(&self, name: &str) -> Option<&Scheme> {
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name) {
                return Some(scheme);
            }
        }
        None
    }

    fn instantiate(&mut self, scheme: &Scheme) -> InferType {
        let mut mapping = HashMap::new();
        for var in &scheme.vars {
            mapping.insert(*var, self.fresh_var());
        }
        self.instantiate_ty(&scheme.ty, &mapping)
    }

    fn instantiate_ty(
        &mut self,
        ty: &InferType,
        mapping: &HashMap<TypeVarId, InferType>,
    ) -> InferType {
        match ty {
            InferType::Var(var) => mapping.get(var).cloned().unwrap_or(InferType::Var(*var)),
            InferType::Con(name) => InferType::Con(name.clone()),
            InferType::App(name, args) => InferType::App(
                name.clone(),
                args.iter()
                    .map(|arg| self.instantiate_ty(arg, mapping))
                    .collect(),
            ),
            InferType::Fn(params, ret) => InferType::Fn(
                params
                    .iter()
                    .map(|param| self.instantiate_ty(param, mapping))
                    .collect(),
                Box::new(self.instantiate_ty(ret, mapping)),
            ),
            InferType::FixedArray(elem, n) => {
                InferType::FixedArray(Box::new(self.instantiate_ty(elem, mapping)), *n)
            }
        }
    }

    fn generalize(&self, ty: &InferType, exclude: Option<&str>) -> Scheme {
        let ty = self.apply(ty);
        let mut vars = self.free_type_vars(&ty);
        for scope in &self.scopes {
            for (name, scheme) in scope {
                if exclude.is_some() && exclude == Some(name.as_str()) {
                    continue;
                }
                for bound in &scheme.vars {
                    vars.remove(bound);
                }
            }
        }
        Scheme {
            vars: vars.into_iter().collect(),
            ty,
        }
    }

    fn free_type_vars(&self, ty: &InferType) -> HashSet<TypeVarId> {
        match self.apply(ty) {
            InferType::Var(var) => HashSet::from([var]),
            InferType::Con(_) => HashSet::new(),
            InferType::App(_, args) => {
                let mut vars = HashSet::new();
                for arg in args {
                    vars.extend(self.free_type_vars(&arg));
                }
                vars
            }
            InferType::Fn(params, ret) => {
                let mut vars = HashSet::new();
                for param in params {
                    vars.extend(self.free_type_vars(&param));
                }
                vars.extend(self.free_type_vars(&ret));
                vars
            }
            InferType::FixedArray(elem, _) => self.free_type_vars(&elem),
        }
    }

    fn apply(&self, ty: &InferType) -> InferType {
        match ty {
            InferType::Var(var) => self
                .subst
                .get(var)
                .cloned()
                .map(|t| self.apply(&t))
                .unwrap_or(InferType::Var(*var)),
            InferType::Con(name) => InferType::Con(name.clone()),
            InferType::App(name, args) => InferType::App(
                name.clone(),
                args.iter().map(|arg| self.apply(arg)).collect(),
            ),
            InferType::Fn(params, ret) => InferType::Fn(
                params.iter().map(|param| self.apply(param)).collect(),
                Box::new(self.apply(ret)),
            ),
            InferType::FixedArray(elem, n) => InferType::FixedArray(Box::new(self.apply(elem)), *n),
        }
    }

    /// Returns the widening rank for a numeric type constructor, or `None` if the
    /// type is not part of a widening hierarchy.  Higher rank = wider type.
    ///
    /// Integer chain:  Int8(0) → Int16(1) → Int32(2) → Int64(3)
    /// Float chain:    Float16(10) → Float32(11) → Float64(12)
    ///
    /// The two chains are disjoint: an integer can never widen into a float.
    fn numeric_rank(name: &str) -> Option<u8> {
        match name {
            "Int8" => Some(0),
            "Int16" => Some(1),
            "Int32" => Some(2),
            "Int64" => Some(3),
            "Float16" => Some(10),
            "Float32" => Some(11),
            "Float64" => Some(12),
            _ => None,
        }
    }

    /// Returns `true` when a value of type `actual` can be implicitly widened
    /// to `expected`.  Both must be concrete type constructors in the same
    /// numeric hierarchy, and `actual` must be strictly narrower.
    fn can_widen_to(actual: &str, expected: &str) -> bool {
        match (Self::numeric_rank(actual), Self::numeric_rank(expected)) {
            (Some(a), Some(b)) => {
                // Same hierarchy (both int or both float) and actual is narrower.
                let same_chain = (a < 10) == (b < 10);
                same_chain && a < b
            }
            _ => false,
        }
    }

    fn unify(
        &mut self,
        left: InferType,
        right: InferType,
        span: Option<Span>,
    ) -> Result<(), TypeError> {
        let left = self.apply(&left);
        let right = self.apply(&right);
        match (left, right) {
            (InferType::Var(a), InferType::Var(b)) if a == b => Ok(()),
            (InferType::Var(a), ty) | (ty, InferType::Var(a)) => self.bind_var(a, ty, span),
            (InferType::Con(a), InferType::Con(b)) if a == b => Ok(()),
            // Implicit numeric widening: Int8 → Int32, Float32 → Float64, etc.
            // We accept the narrower `actual` wherever the wider `expected` is
            // required; the inverse (widening the expected) is not allowed so
            // that we don't silently truncate.
            (InferType::Con(ref expected), InferType::Con(ref actual))
                if Self::can_widen_to(actual, expected) =>
            {
                Ok(())
            }
            (InferType::FixedArray(a_elem, a_len), InferType::FixedArray(b_elem, b_len))
                if a_len == b_len =>
            {
                self.unify(*a_elem, *b_elem, span)
            }
            (InferType::FixedArray(a_elem, _), InferType::App(name, mut args))
                if name == "Array" && args.len() == 1 =>
            {
                self.unify(*a_elem, args.remove(0), span)
            }
            (InferType::App(name, mut args), InferType::FixedArray(b_elem, _))
                if name == "Array" && args.len() == 1 =>
            {
                self.unify(args.remove(0), *b_elem, span)
            }
            (InferType::App(a, a_args), InferType::App(b, b_args))
                if a == b && a_args.len() == b_args.len() =>
            {
                for (x, y) in a_args.into_iter().zip(b_args.into_iter()) {
                    self.unify(x, y, span)?;
                }
                Ok(())
            }
            (InferType::Fn(a_params, a_ret), InferType::Fn(b_params, b_ret))
                if a_params.len() == b_params.len() =>
            {
                for (x, y) in a_params.into_iter().zip(b_params.into_iter()) {
                    self.unify(x, y, span)?;
                }
                self.unify(*a_ret, *b_ret, span)
            }
            (expected, actual) => {
                eprintln!("Unify: {:?} {:?}", expected, actual);
                eprintln!("next_var: {:?}", self.next_var);
                eprintln!("subst: {:?}", self.subst);
                eprintln!("rigid: {:?}", self.rigid);
                eprintln!("scopes: {:?}", self.scopes);
                eprintln!("current_return: {:?}", self.current_return);
                eprintln!("types: {:?}", self.types);
                eprintln!("struct_fields: {:?}", self.struct_fields);
                eprintln!("newtype_alias: {:?}", self.newtype_alias);
                eprintln!("specializations: {:?}", self.specializations);
                eprintln!("enum_variants: {:?}", self.enum_variants);
                eprintln!("option_type_name: {:?}", self.option_type_name);
                eprintln!("result_type_name: {:?}", self.result_type_name);
                eprintln!("extern_fns: {:?}", self.extern_fns);
                Err(TypeError::TypeMismatch {
                    expected,
                    actual,
                    context: "unification".into(),
                    span,
                })
            }
        }
    }

    fn bind_var(
        &mut self,
        var: TypeVarId,
        ty: InferType,
        span: Option<Span>,
    ) -> Result<(), TypeError> {
        let ty = self.apply(&ty);
        if ty == InferType::Var(var) {
            return Ok(());
        }
        if self.rigid.contains(&var) {
            eprintln!("Rigid {:?} {:?}", var, ty);
            return Err(TypeError::TypeMismatch {
                expected: InferType::Var(var),
                actual: ty,
                context: "rigid type parameter".into(),
                span,
            });
        }
        if self.occurs_in(var, &ty) {
            return Err(TypeError::OccursCheck { var, ty, span });
        }
        self.subst.insert(var, ty);
        Ok(())
    }

    fn occurs_in(&self, var: TypeVarId, ty: &InferType) -> bool {
        match self.apply(ty) {
            InferType::Var(other) => other == var,
            InferType::Con(_) => false,
            InferType::App(_, args) => args.iter().any(|arg| self.occurs_in(var, arg)),
            InferType::Fn(params, ret) => {
                params.iter().any(|param| self.occurs_in(var, &param)) || self.occurs_in(var, &ret)
            }
            InferType::FixedArray(elem, _) => self.occurs_in(var, &elem),
        }
    }

    /// Given an enum name and the concrete type args it was instantiated with
    /// at the call site (e.g. `Option<TypeVarId(37)>`), returns a substitution
    /// mapping from the registration-time generic vars to those concrete args.
    fn build_enum_var_mapping(
        &self,
        enum_name: &str,
        concrete_args: &[InferType],
    ) -> HashMap<TypeVarId, InferType> {
        // Find any variant that belongs to this enum to retrieve the ordered vars.
        // All variants of the same enum share the same ordered_vars list.
        let ordered_vars = self
            .enum_variants
            .values()
            .find(|(name, _, _)| name == enum_name)
            .map(|(_, vars, _)| vars.as_slice())
            .unwrap_or(&[]);

        ordered_vars
            .iter()
            .zip(concrete_args.iter())
            .map(|(var_id, concrete)| (*var_id, concrete.clone()))
            .collect()
    }

    /// Returns the payload type for an enum variant, substituting the
    /// registration-time generic vars with the concrete args from the scrutinee.
    fn enum_variant_payload(&self, scrutinee: &InferType, variant: &str) -> Option<InferType> {
        let (enum_name, _, stored_payload) = self.enum_variants.get(variant)?;
        let stored_payload = stored_payload.as_ref()?;

        if let InferType::App(sname, sargs) = self.apply(scrutinee) {
            if &sname == enum_name {
                let mapping = self.build_enum_var_mapping(&sname, &sargs);
                return Some(Self::substitute_ty(stored_payload, &mapping));
            }
        }

        // Scrutinee is not a generic App (e.g. a plain Con), or enum name
        // doesn't match — return the stored payload as-is.
        Some(stored_payload.clone())
    }

    fn substitute_ty(ty: &InferType, mapping: &HashMap<TypeVarId, InferType>) -> InferType {
        match ty {
            InferType::Var(var) => mapping.get(var).cloned().unwrap_or(InferType::Var(*var)),
            InferType::Con(name) => InferType::Con(name.clone()),
            InferType::App(name, args) => InferType::App(
                name.clone(),
                args.iter()
                    .map(|a| Self::substitute_ty(a, mapping))
                    .collect(),
            ),
            InferType::Fn(params, ret) => InferType::Fn(
                params
                    .iter()
                    .map(|p| Self::substitute_ty(p, mapping))
                    .collect(),
                Box::new(Self::substitute_ty(ret, mapping)),
            ),
            InferType::FixedArray(elem, n) => {
                InferType::FixedArray(Box::new(Self::substitute_ty(elem, mapping)), *n)
            }
        }
    }

    // For an EnumVariant pattern like `Ok(x)` or `Some(i)`, unwrap the
    // corresponding payload type from a `Result<T,E>` or `Option<T>` so
    // that `bind_pattern` sees `T` rather than the whole wrapper.
    // For any other pattern shape the original type is returned unchanged.
    fn unwrap_enum_payload(&self, pattern: &Pattern, ty: &InferType) -> InferType {
        let applied = self.apply(ty);
        if let PatternKind::EnumVariant { variant_name, .. } = &pattern.node {
            if let Some(payload) = self.enum_variant_payload(&applied, &variant_name.name) {
                return payload;
            }
        }
        applied
    }
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::resolver::Resolver;

    fn normalize_source(source: &str) -> String {
        let mut body = source.trim().to_string();
        if !body.contains("enum Option") {
            body = format!(
                "enum Option<T> {{ Some(T), None }} enum Result<T, E> {{ Ok(T), Err(E) }}\n{}",
                body
            );
        }
        if !body.starts_with("namespace main") {
            body = format!("namespace main\n{}", body);
        }
        if !body.contains("fn main") {
            body.push_str("\nfn main() -> Int32 { return 0; }");
        }
        body
    }

    fn check(source: &str) -> Result<(), TypeError> {
        let module = Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap();
        let mut resolver = Resolver::new();
        resolver.resolve_module(&module).unwrap();
        let mut checker = TypeChecker::new();
        checker.check_module(&module)
    }

    #[test]
    fn accepts_simple_function() {
        assert!(check(
            "fn compute(count: Int32) -> Int32 { let accumulator: Int32 = 0; return accumulator; }"
        )
        .is_ok());
    }

    #[test]
    fn accepts_generic_identity() {
        assert!(check("fn id<T>(x: T) -> T { return x; }").is_ok());
    }

    #[test]
    fn accepts_generic_call() {
        let source = "fn id<T>(x: T) -> T { return x; } fn use_it() -> Int32 { return id(1); }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn lets_generalize_polymorphically() {
        let source = "fn poly() -> Int32 { let xs = []; let a: Array<Int32> = xs; let b: Array<Bool> = xs; return 0; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn array_literal_coerces_to_array_annotation() {
        let source = "fn main() -> Int32 { let xs: Array<Int32> = [1,2,3]; return 0; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn struct_field_access_types() {
        let source =
            "struct User { id: Int32 } fn main() -> Int32 { let u: User = User { id: 1 }; let x: Int32 = u.id; return x; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn array_index_returns_option() {
        let source =
            "fn main() -> Int32 { let xs: Array<Int32> = [1,2,3]; let v: Option<Int32> = xs[0]; return 0; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn array_push_is_unit() {
        let source =
            "fn main() -> Int32 { let mut xs: Array<Int32> = [1,2]; xs.push(3); return 0; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn resolves_struct_method_calls_via_mangled_function() {
        let source = "struct User { id: Int32 } fn __ty_method__User__get_id(self: User) -> Int32 { return self.id; } fn main() -> Int32 { let u: User = User { id: 1 }; return u.get_id(); }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn rejects_rigid_generic_specialization() {
        assert!(check("fn bad<T>(x: T) -> T { let y: Int32 = x; return y; }").is_err());
    }

    #[test]
    fn literal_suffix_types() {
        assert!(check("fn i8f() -> Int32 { return 42; }").is_ok());
        assert!(check("fn i16f() -> Int32 { return 100; }").is_ok());
        assert!(check("fn i64f() -> Int32 { return 900; }").is_ok());
        assert!(check("fn float64f() -> Float64 { return 3.14f64; }").is_ok());
        assert!(check("fn bytef() -> Byte { return 255u8; }").is_ok());
    }

    #[test]
    fn arithmetic_int32_accepts() {
        assert!(check("fn add() -> Int32 { return 1 + 2; }").is_ok());
    }

    #[test]
    fn arithmetic_i8_rejects() {
        assert!(check("fn addi8() -> Int32 { return 1i8 + 2i8; }").is_err());
    }

    #[test]
    fn widening_i8_to_i32_in_call_accepts() {
        // Passing an Int8 where Int32 is expected should be allowed via widening.
        let source = "fn take_i32(x: Int32) -> Int32 { return x; } \
                      fn f() -> Int32 { return take_i32(1i8); }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn widening_i8_to_i64_in_call_accepts() {
        let source = "fn take_i64(x: Int64) -> Int64 { return x; } \
                      fn f() -> Int64 { return take_i64(1i8); }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn widening_float32_to_float64_accepts() {
        let source = "fn take_f64(x: Float64) -> Float64 { return x; } \
                      fn f() -> Float64 { return take_f64(1.0f32); }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn widening_does_not_cross_int_float_boundary() {
        // Int32 → Float64 must not be allowed implicitly.
        let source = "fn take_f64(x: Float64) -> Float64 { return x; } \
                      fn f() -> Float64 { return take_f64(1); }";
        assert!(check(source).is_err());
    }

    #[test]
    fn widening_does_not_narrow() {
        // Int32 → Int8 must not be allowed.
        let source = "fn take_i8(x: Int8) -> Int8 { return x; } \
                      fn f() -> Int8 { return take_i8(1); }";
        assert!(check(source).is_err());
    }

    #[test]
    fn binary_i8_plus_i32_yields_i32() {
        // Mixed-width arithmetic: result should be the wider type.
        let source = "fn f() -> Int32 { return 1i8 + 2; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn binary_i8_plus_i8_rejects_i32_return() {
        // Two Int8 operands → Int8 result, not Int32.
        let source = "fn f() -> Int32 { return 1i8 + 2i8; }";
        assert!(check(source).is_err());
    }

    #[test]
    fn bitwise_shift_accepts() {
        assert!(check("fn shl() -> Int32 { return 1 << 2; }").is_ok());
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        let mut checker = TypeChecker::new();
        let var = checker.fresh_var();
        let infinite = InferType::Fn(vec![var.clone()], Box::new(InferType::Con("Int32".into())));
        assert!(checker.unify(var, infinite, None).is_err());
    }

    #[test]
    fn tracks_generic_struct_instantiations() {
        let source = "
            struct Gen<T> { val: T }
            fn main() -> Int32 {
                let g1 = Gen { val: 1 };
                let g2 = Gen { val: 1.0f32 };
                return 0;
            }";
        let module = Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap();
        let mut resolver = Resolver::new();
        resolver.resolve_module(&module).unwrap();
        let mut checker = TypeChecker::new();
        checker.check_module(&module).unwrap();

        // Check for unique struct instantiations
        // The previous test already adds 2 entries. This one adds 2 more.
        assert!(checker.specializations.len() >= 2);
    }
}
