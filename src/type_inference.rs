use crate::ast::*;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeVarId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
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

pub struct TypeChecker {
    next_var: usize,
    subst: HashMap<TypeVarId, InferType>,
    rigid: HashSet<TypeVarId>,
    scopes: Vec<HashMap<String, Scheme>>,
    current_return: Option<InferType>,
    types: HashMap<NodeId, InferType>,
    struct_fields: HashMap<String, HashMap<String, InferType>>,
    newtype_alias: HashMap<String, InferType>,
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
        }
    }

    pub fn check_module(&mut self, module: &Module) -> Result<(), TypeError> {
        self.reset();
        self.collect_type_info(module)?;
        self.predeclare_functions(module)?;
        for decl in &module.declarations {
            if let DeclarationKind::Function { name, .. } = &decl.node {
                self.check_function(name, decl)?;
            }
        }
        // Freeze inferred expression types by applying final substitutions.
        // Codegen reads this map directly and expects concrete types.
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

        let t = self.fresh_var_id();
        let e = self.fresh_var_id();
        self.set_global(
            "__ty_result_err".into(),
            Scheme {
                vars: vec![t, e],
                ty: InferType::Fn(
                    vec![InferType::Var(e)],
                    Box::new(InferType::App(
                        "Result".into(),
                        vec![InferType::Var(t), InferType::Var(e)],
                    )),
                ),
            },
        );

        // ── Networking (runtime-provided methods) ───────────────────────────
        // net.listen(addr: Str) -> Result<Listener, Int32>
        self.set_global(
            "__ty_method__Network__listen".into(),
            Scheme::mono(InferType::Fn(
                vec![
                    InferType::Con("Network".into()),
                    InferType::Con("Str".into()),
                ],
                Box::new(InferType::App(
                    "Result".into(),
                    vec![
                        InferType::Con("Listener".into()),
                        InferType::Con("Int32".into()),
                    ],
                )),
            )),
        );
        // listener.accept() -> Result<Socket, Int32>
        self.set_global(
            "__ty_method__Listener__accept".into(),
            Scheme::mono(InferType::Fn(
                vec![InferType::Con("Listener".into())],
                Box::new(InferType::App(
                    "Result".into(),
                    vec![
                        InferType::Con("Socket".into()),
                        InferType::Con("Int32".into()),
                    ],
                )),
            )),
        );
        // socket.consume(ch: chan) -> Unit
        self.set_global(
            "__ty_method__Socket__consume".into(),
            Scheme::mono(InferType::Fn(
                vec![
                    InferType::Con("Socket".into()),
                    InferType::App(
                        "Ref".into(),
                        vec![InferType::App(
                            "Chan".into(),
                            vec![InferType::Con("Int8".into())],
                        )],
                    ),
                ],
                Box::new(InferType::Con("Unit".into())),
            )),
        );
        // socket.close() -> Unit
        self.set_global(
            "__ty_method__Socket__close".into(),
            Scheme::mono(InferType::Fn(
                vec![InferType::Con("Socket".into())],
                Box::new(InferType::Con("Unit".into())),
            )),
        );

        let t2 = self.fresh_var_id();
        let e2 = self.fresh_var_id();
        self.set_global(
            "__ty_result_ok".into(),
            Scheme {
                vars: vec![t2, e2],
                ty: InferType::Fn(
                    vec![InferType::Var(t2)],
                    Box::new(InferType::App(
                        "Result".into(),
                        vec![InferType::Var(t2), InferType::Var(e2)],
                    )),
                ),
            },
        );

        // User-facing constructors (used by desugaring and future parsing)
        let t3 = self.fresh_var_id();
        let e3 = self.fresh_var_id();
        self.set_global(
            "Ok".into(),
            Scheme {
                vars: vec![t3, e3],
                ty: InferType::Fn(
                    vec![InferType::Var(t3)],
                    Box::new(InferType::App(
                        "Result".into(),
                        vec![InferType::Var(t3), InferType::Var(e3)],
                    )),
                ),
            },
        );
        let t4 = self.fresh_var_id();
        let e4 = self.fresh_var_id();
        self.set_global(
            "Err".into(),
            Scheme {
                vars: vec![t4, e4],
                ty: InferType::Fn(
                    vec![InferType::Var(e4)],
                    Box::new(InferType::App(
                        "Result".into(),
                        vec![InferType::Var(t4), InferType::Var(e4)],
                    )),
                ),
            },
        );
        let t5 = self.fresh_var_id();
        self.set_global(
            "Some".into(),
            Scheme {
                vars: vec![t5],
                ty: InferType::Fn(
                    vec![InferType::Var(t5)],
                    Box::new(InferType::App("Option".into(), vec![InferType::Var(t5)])),
                ),
            },
        );
        let t6 = self.fresh_var_id();
        self.set_global(
            "None".into(),
            Scheme {
                vars: vec![t6],
                ty: InferType::App("Option".into(), vec![InferType::Var(t6)]),
            },
        );
    }

    fn predeclare_functions(&mut self, module: &Module) -> Result<(), TypeError> {
        for decl in &module.declarations {
            if let DeclarationKind::Function { name, .. } = &decl.node {
                let (scheme, _, _, _) = self.lower_function_signature(decl)?;
                self.set_global(name.name.clone(), scheme);
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
                        if let InferType::Var(id) = self.fresh_rigid_var() {
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
                _ => {}
            }
        }
        Ok(())
    }

    fn check_function(&mut self, name: &Identifier, decl: &Declaration) -> Result<(), TypeError> {
        let (scheme, param_tys, ret_ty, fn_ty) = self.lower_function_signature(decl)?;
        self.set_global(name.name.clone(), scheme);

        let body = match &decl.node {
            DeclarationKind::Function { params, body, .. } => {
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
        for generic in generics {
            if let InferType::Var(id) = self.fresh_rigid_var() {
                generic_vars.insert(generic.node.name.name.clone(), id);
            }
        }

        let mut param_tys = Vec::new();
        for param in params {
            param_tys.push(self.lower_type(&param.type_annotation, &generic_vars)?);
        }

        let ret_ty = match return_type {
            Some(ty) => self.lower_type(ty, &generic_vars)?,
            None => self.fresh_var(),
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
        match &stmt.node {
            StatementKind::LetBinding {
                name,
                type_annotation,
                initializer,
                mutable,
            } => {
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
                let scheme = if *mutable {
                    Scheme::mono(self.apply(&ty))
                } else {
                    self.generalize(&ty, None)
                };
                self.insert_local(name.name.clone(), scheme);
                Ok(())
            }
            StatementKind::Expression(expr) => {
                let _ = self.infer_expression(expr)?;
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
            StatementKind::Empty | StatementKind::UseDeclaration(_) => Ok(()),
        }
    }

    fn infer_expression(&mut self, expr: &Expression) -> Result<InferType, TypeError> {
        let ty = match &expr.node {
            ExpressionKind::Literal(lit) => self.literal_type(lit, expr.span)?,
            ExpressionKind::Identifier(id) => {
                if id.name.as_str() == "chan" {
                    // chan has a generic return type that depends on context
                    return Ok(InferType::Con("Chan".into()));
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
                        // chan() always produces a shared channel — wrap in Ref
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
                                "recv" => elem,
                                "try_recv" => InferType::App("Option".into(), vec![elem]),
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
                    InferType::App("Option".into(), vec![elem])
                } else {
                    self.fresh_var()
                }
            }
            ExpressionKind::StructInit { name, fields } => {
                for (_, field_expr) in fields {
                    let _ = self.infer_expression(field_expr)?;
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
                if let Some(inner) = result_variant_payload(expected, &variant_name.name) {
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

    fn try_inner_type(&self, ty: &InferType) -> Option<InferType> {
        match self.apply(ty) {
            InferType::App(name, args) if name == "Result" && args.len() == 2 => {
                Some(args[0].clone())
            }
            InferType::App(name, args) if name == "Option" && args.len() == 1 => {
                Some(args[0].clone())
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
        if let Some(var) = generic_vars.get(&ty.node.name) {
            if !ty.node.generic_args.is_empty() {
                return Err(TypeError::TypeMismatch {
                    expected: InferType::Var(*var),
                    actual: InferType::App(ty.node.name.clone(), Vec::new()),
                    context: "generic type application".into(),
                    span: Some(ty.span),
                });
            }
            return Ok(InferType::Var(*var));
        }

        let args = ty
            .node
            .generic_args
            .iter()
            .map(|arg| self.lower_type(arg, generic_vars))
            .collect::<Result<Vec<_>, _>>()?;
        if args.is_empty() {
            Ok(InferType::Con(ty.node.name.clone()))
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
            (expected, actual) => Err(TypeError::TypeMismatch {
                expected,
                actual,
                context: "unification".into(),
                span,
            }),
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
}

fn result_variant_payload(expected: &InferType, variant: &str) -> Option<InferType> {
    match expected {
        InferType::App(name, args) if name == "Result" && args.len() == 2 => match variant {
            "Ok" => Some(args[0].clone()),
            "Err" => Some(args[1].clone()),
            _ => None,
        },
        InferType::App(name, args) if name == "Option" && args.len() == 1 => match variant {
            "Some" => Some(args[0].clone()),
            "None" => None,
            _ => None,
        },
        _ => None,
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
}
