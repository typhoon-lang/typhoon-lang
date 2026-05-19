use crate::ast::*;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

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
pub struct Scheme {
    pub vars: Vec<TypeVarId>,
    pub ty: InferType,
}

impl Scheme {
    pub fn mono(ty: InferType) -> Self {
        Self {
            vars: Vec::new(),
            ty,
        }
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    pub struct_fields: HashMap<String, HashMap<String, InferType>>,
    pub newtype_alias: HashMap<String, InferType>,
    pub enum_variants: HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,
    pub extern_fns: HashSet<String>,
    pub option_type_name: Option<String>,
    pub result_type_name: Option<String>,
}

#[derive(Debug)]
pub struct Solver {
    next_var: usize,
    subst: HashMap<TypeVarId, InferType>,
    rigid: HashSet<TypeVarId>,
}

impl Solver {
    pub fn new() -> Self {
        Self {
            next_var: 0,
            subst: HashMap::new(),
            rigid: HashSet::new(),
        }
    }

    pub fn fresh_var(&mut self) -> InferType {
        let id = TypeVarId(self.next_var);
        self.next_var += 1;
        InferType::Var(id)
    }

    pub fn fresh_rigid_var(&mut self) -> InferType {
        let id = TypeVarId(self.next_var);
        self.next_var += 1;
        self.rigid.insert(id);
        InferType::Var(id)
    }

    pub fn apply(&self, ty: &InferType) -> InferType {
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
                params.iter().map(|p| self.apply(p)).collect(),
                Box::new(self.apply(ret)),
            ),
            InferType::FixedArray(elem, n) => InferType::FixedArray(Box::new(self.apply(elem)), *n),
        }
    }

    pub fn unify(
        &mut self,
        left: &InferType,
        right: &InferType,
        span: Option<Span>,
    ) -> Result<(), TypeError> {
        let left = self.apply(left);
        let right = self.apply(right);
        match (left, right) {
            (InferType::Var(a), InferType::Var(b)) if a == b => Ok(()),
            (InferType::Var(a), ty) | (ty, InferType::Var(a)) => self.bind_var(a, ty, span),
            (InferType::Con(a), InferType::Con(b)) if a == b => Ok(()),
            (InferType::FixedArray(a_elem, a_len), InferType::FixedArray(b_elem, b_len))
                if a_len == b_len =>
            {
                self.unify(&a_elem, &b_elem, span)
            }
            (InferType::FixedArray(a_elem, _), InferType::App(name, args))
                if name == "Array" && args.len() == 1 =>
            {
                self.unify(&a_elem, &args[0], span)
            }
            (InferType::App(name, args), InferType::FixedArray(b_elem, _))
                if name == "Array" && args.len() == 1 =>
            {
                self.unify(&args[0], &b_elem, span)
            }
            (InferType::App(a, a_args), InferType::App(b, b_args))
                if a == b && a_args.len() == b_args.len() =>
            {
                for (x, y) in a_args.iter().zip(b_args.iter()) {
                    self.unify(x, y, span)?;
                }
                Ok(())
            }
            (InferType::Fn(a_params, a_ret), InferType::Fn(b_params, b_ret))
                if a_params.len() == b_params.len() =>
            {
                for (x, y) in a_params.iter().zip(b_params.iter()) {
                    self.unify(x, y, span)?;
                }
                self.unify(&*a_ret, &*b_ret, span)
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

#[derive(Debug)]
pub struct TypeChecker {
    pub solver: Solver,
    pub registry: Registry,
    pub scopes: Vec<HashMap<String, Scheme>>,
    pub types: HashMap<NodeId, InferType>,
    pub specializations: HashMap<(String, Vec<InferType>), String>,
    pub current_return: Option<InferType>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self {
            solver: Solver::new(),
            registry: Registry::default(),
            scopes: vec![HashMap::new()],
            types: HashMap::new(),
            specializations: HashMap::new(),
            current_return: None,
        }
    }

    pub fn insert_local(&mut self, name: String, scheme: Scheme) {
        self.scopes.last_mut().unwrap().insert(name, scheme);
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn check_block(
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
        Ok(self.solver.apply(&result))
    }

    pub fn reset(&mut self) {
        self.solver = Solver::new();
        self.registry = Registry::default();
        self.scopes = vec![HashMap::new()];
        self.current_return = None;
        self.types.clear();
        self.specializations.clear();
        self.seed_builtins();
    }

    pub fn types(&self) -> &HashMap<NodeId, InferType> {
        &self.types
    }

    pub fn seed_builtins(&mut self) {
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

    pub fn check_module(&mut self, module: &Module) -> Result<(), TypeError> {
        self.reset();
        self.collect_type_info(module)?;
        self.predeclare_functions(module)?;
        for decl in &module.declarations {
            if let DeclarationKind::Function { name, .. } = &decl.node {
                self.check_function(name, decl)?;
            }
        }
        self.finalize_types();
        Ok(())
    }

    fn finalize_types(&mut self) {
        let keys: Vec<NodeId> = self.types.keys().cloned().collect();
        for k in keys {
            if let Some(ty) = self.types.get(&k).cloned() {
                self.types.insert(k, self.solver.apply(&ty));
            }
        }
    }

    pub fn lower_function_signature(
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
            generic_vars.insert(generic.node.name.name.clone(), {
                let var = self.solver.fresh_var();
                if let InferType::Var(id) = var {
                    id
                } else {
                    unreachable!()
                }
            });
        }

        let mut param_tys = Vec::new();
        for param in params {
            param_tys.push(self.lower_type(&param.type_annotation, &generic_vars)?);
        }

        let ret_ty = match return_type {
            Some(ty) => self.lower_type(ty, &generic_vars)?,
            None => InferType::Con("Unit".into()),
        };

        let fn_ty = InferType::Fn(param_tys.clone(), Box::new(ret_ty.clone()));
        let scheme = self.generalize(&fn_ty, None);
        Ok((scheme, param_tys, ret_ty, fn_ty))
    }

    pub fn check_function(
        &mut self,
        name: &Identifier,
        decl: &Declaration,
    ) -> Result<(), TypeError> {
        let (scheme, param_tys, ret_ty, fn_ty) = self.lower_function_signature(decl)?;
        self.set_global(name.name.clone(), scheme);

        match &decl.node {
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
                    self.solver.unify(&body_ty, &ret_ty, Some(body.span))?;
                }
            }
            _ => unreachable!(),
        };

        let final_ty = self.solver.apply(&fn_ty);
        let final_scheme = self.generalize(&final_ty, Some(&name.name));
        self.set_global(name.name.clone(), final_scheme);
        Ok(())
    }

    pub fn predeclare_functions(&mut self, module: &Module) -> Result<(), TypeError> {
        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Function { name, .. } => {
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
                                generic_vars.insert(g.node.name.name.clone(), {
                                    let var = self.solver.fresh_rigid_var();
                                    if let InferType::Var(id) = var {
                                        id
                                    } else {
                                        unreachable!()
                                    }
                                });
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
                            self.registry.extern_fns.insert(name.name.clone());
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
                        generic_vars.insert(g.node.name.name.clone(), {
                            let var = self.solver.fresh_var();
                            if let InferType::Var(id) = var {
                                id
                            } else {
                                unreachable!()
                            }
                        });
                    }
                    let mut map = HashMap::new();
                    for (field_id, field_ty) in fields {
                        let lowered = self.lower_type(field_ty, &generic_vars)?;
                        map.insert(field_id.name.clone(), lowered);
                    }
                    self.registry.struct_fields.insert(name.name.clone(), map);
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    let alias = self.lower_type(type_alias, &HashMap::new())?;
                    self.registry.newtype_alias.insert(name.name.clone(), alias);
                }
                DeclarationKind::Enum {
                    name,
                    generics,
                    variants,
                } => {
                    let mut generic_vars = HashMap::new();
                    for g in generics {
                        generic_vars.insert(g.node.name.name.clone(), {
                            let var = self.solver.fresh_var();
                            if let InferType::Var(id) = var {
                                id
                            } else {
                                unreachable!()
                            }
                        });
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

                    let variant_names = variants
                        .iter()
                        .map(|v| v.node.name.name.as_str())
                        .collect::<Vec<_>>();
                    if variant_names.iter().any(|&v| v == "Some")
                        && variant_names.iter().any(|&v| v == "None")
                    {
                        self.registry.option_type_name = Some(name.name.clone());
                    }
                    if variant_names.iter().any(|&v| v == "Ok")
                        && variant_names.iter().any(|&v| v == "Err")
                    {
                        self.registry.result_type_name = Some(name.name.clone());
                    }

                    for variant in variants {
                        let variant_name = variant.node.name.name.clone();
                        let payload_ty = match &variant.node.payload {
                            None => None,
                            Some(p) => match &p.node {
                                EnumVariantPayloadKind::Tuple(types) if types.len() == 1 => {
                                    Some(self.lower_type(&types[0], &generic_vars)?)
                                }
                                EnumVariantPayloadKind::Tuple(_)
                                | EnumVariantPayloadKind::Struct(_)
                                | EnumVariantPayloadKind::Unit(_) => Some(self.solver.fresh_var()),
                            },
                        };
                        self.registry.enum_variants.insert(
                            variant_name.clone(),
                            (name.name.clone(), ordered_vars.clone(), payload_ty.clone()),
                        );
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
                            self.set_global(variant_name.clone(), Scheme::mono(enum_ty.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn set_global(&mut self, name: String, scheme: Scheme) {
        self.scopes.first_mut().unwrap().insert(name, scheme);
    }

    pub fn lookup(&self, name: &str) -> Option<&Scheme> {
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name) {
                return Some(scheme);
            }
        }
        None
    }

    fn try_inner_func(&self, ty: String) -> bool {
        [
            "break", "continue", "print", "println", "printf", "fprint", "fprintln", "fprintf",
            "sprint", "sprintln", "sprintf", "scan", "scanf", "fscan", "fscanf", "sscan", "sscanf",
        ]
        .contains(&ty.as_str())
    }

    fn try_inner_type(&self, ty: &InferType) -> Option<InferType> {
        match self.solver.apply(ty) {
            InferType::App(ref name, _) => self
                .registry
                .enum_variants
                .values()
                .find(|(enum_name, _, _)| enum_name == name)
                .and_then(|(_, _, p)| p.clone()),
            _ => None,
        }
    }

    fn make_option_ty(&self, inner: InferType) -> InferType {
        InferType::App(
            self.registry
                .option_type_name
                .clone()
                .unwrap_or_else(|| "Option".into()),
            vec![inner],
        )
    }

    fn array_elem_type(&self, ty: &InferType) -> Option<InferType> {
        match self.solver.apply(ty) {
            InferType::App(name, args) if name == "Array" && args.len() == 1 => {
                Some(args[0].clone())
            }
            InferType::FixedArray(elem, _) => Some(*elem),
            _ => None,
        }
    }

    pub fn bind_pattern(
        &mut self,
        pattern: &Pattern,
        expected: &InferType,
    ) -> Result<(), TypeError> {
        match &pattern.node {
            PatternKind::Wildcard => Ok(()),
            PatternKind::Identifier(id) => {
                self.insert_local(id.name.clone(), Scheme::mono(self.solver.apply(expected)));
                Ok(())
            }
            PatternKind::Literal(lit) => {
                let ty = self.literal_type(lit, pattern.span)?;
                self.solver.unify(&ty, expected, Some(pattern.span))
            }
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                let elem = self
                    .array_elem_type(expected)
                    .unwrap_or_else(|| self.solver.fresh_var());
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
                self.solver
                    .unify(&guard_ty, &InferType::Con("Bool".into()), Some(guard.span))
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

    fn literal_type(&mut self, lit: &Literal, _span: Span) -> Result<InferType, TypeError> {
        match &lit.kind {
            LiteralKind::Int(_, suffix) => Ok(match suffix.as_deref() {
                Some("i8") => InferType::Con("Int8".into()),
                Some("i16") => InferType::Con("Int16".into()),
                Some("i32") => InferType::Con("Int32".into()),
                Some("i64") => InferType::Con("Int64".into()),
                Some("u8") => InferType::Con("Byte".into()),
                _ => InferType::Con("Int32".into()),
            }),
            LiteralKind::Float(_, suffix) => Ok(match suffix.as_deref() {
                Some("f16") => InferType::Con("Float16".into()),
                Some("f32") => InferType::Con("Float32".into()),
                Some("f64") => InferType::Con("Float64".into()),
                _ => InferType::Con("Float32".into()),
            }),
            LiteralKind::Bool(_) => Ok(InferType::Con("Bool".into())),
            LiteralKind::Str(_) => Ok(InferType::Con("Str".into())),
            LiteralKind::Array(elements) => {
                let elem = self.solver.fresh_var();
                for item in elements {
                    let item_ty = self.infer_expression(item)?;
                    self.solver.unify(&item_ty, &elem, Some(item.span))?;
                }
                Ok(InferType::FixedArray(Box::new(elem), elements.len()))
            }
        }
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
                self.solver.unify(&left_ty, &right_ty, Some(span))?;
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
                let result_ty = match (&left_ty, &right_ty) {
                    (InferType::Con(l), InferType::Con(r)) => {
                        match (Self::numeric_rank(l), Self::numeric_rank(r)) {
                            (Some(lr), Some(rr)) if (lr < 10) == (rr < 10) => {
                                if lr >= rr {
                                    left_ty.clone()
                                } else {
                                    right_ty.clone()
                                }
                            }
                            _ => {
                                self.solver.unify(
                                    &left_ty,
                                    &InferType::Con("Int32".into()),
                                    Some(span),
                                )?;
                                self.solver.unify(
                                    &right_ty,
                                    &InferType::Con("Int32".into()),
                                    Some(span),
                                )?;
                                InferType::Con("Int32".into())
                            }
                        }
                    }
                    _ => {
                        self.solver
                            .unify(&left_ty, &InferType::Con("Int32".into()), Some(span))?;
                        self.solver.unify(
                            &right_ty,
                            &InferType::Con("Int32".into()),
                            Some(span),
                        )?;
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
                self.solver.unify(&left_ty, &right_ty, Some(span))?;
                Ok(InferType::Con("Bool".into()))
            }
            Operator::And | Operator::Or => {
                self.solver
                    .unify(&left_ty, &InferType::Con("Bool".into()), Some(span))?;
                self.solver
                    .unify(&right_ty, &InferType::Con("Bool".into()), Some(span))?;
                Ok(InferType::Con("Bool".into()))
            }
            _ => Ok(self.solver.fresh_var()),
        }
    }

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

    fn instantiate(&mut self, scheme: &Scheme) -> InferType {
        let mut mapping = HashMap::new();
        for var in &scheme.vars {
            mapping.insert(*var, self.solver.fresh_var());
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
                    .map(|a| self.instantiate_ty(a, mapping))
                    .collect(),
            ),
            InferType::Fn(params, ret) => InferType::Fn(
                params
                    .iter()
                    .map(|p| self.instantiate_ty(p, mapping))
                    .collect(),
                Box::new(self.instantiate_ty(ret, mapping)),
            ),
            InferType::FixedArray(elem, n) => {
                InferType::FixedArray(Box::new(self.instantiate_ty(elem, mapping)), *n)
            }
        }
    }

    fn enum_variant_payload(&self, scrutinee: &InferType, variant: &str) -> Option<InferType> {
        let (enum_name, _, stored_payload) = self.registry.enum_variants.get(variant)?;
        let stored_payload = stored_payload.as_ref()?;
        if let InferType::App(sname, sargs) = self.solver.apply(scrutinee) {
            if &sname == enum_name {
                let mapping = self.build_enum_var_mapping(&sname, &sargs);
                return Some(Self::substitute_ty(stored_payload, &mapping));
            }
        }
        Some(stored_payload.clone())
    }

    fn build_enum_var_mapping(
        &self,
        enum_name: &str,
        concrete_args: &[InferType],
    ) -> HashMap<TypeVarId, InferType> {
        let ordered_vars = self
            .registry
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

    fn check_statement(
        &mut self,
        stmt: &Statement,
        expected_return: Option<&InferType>,
    ) -> Result<(), TypeError> {
        match &stmt.node {
            StatementKind::LetBinding {
                pattern,
                type_annotation,
                initializer,
                else_block,
                mutable,
            } => {
                let init_ty = self.infer_expression(initializer)?;
                let mut ty = if let Some(annotation) = type_annotation {
                    let annotated = self.lower_type(annotation, &HashMap::new())?;
                    self.solver
                        .unify(&init_ty, &annotated, Some(initializer.span))?;
                    annotated
                } else {
                    init_ty
                };
                if *mutable {
                    if let InferType::FixedArray(elem, _) = self.solver.apply(&ty) {
                        ty = InferType::App("Array".into(), vec![*elem]);
                    }
                }
                if let Some(id) = pattern.get_identifier() {
                    let scheme = if *mutable {
                        Scheme::mono(self.solver.apply(&ty))
                    } else {
                        self.generalize(&ty, None)
                    };
                    self.insert_local(id.name.clone(), scheme);
                } else {
                    self.bind_pattern(pattern, &ty)?;
                }
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk, expected_return)?;
                }
            }
            StatementKind::Expression(expr) => {
                self.infer_expression(expr)?;
            }
            StatementKind::Const {
                name,
                type_annotation,
                initializer,
            } => {
                let init_ty = self.infer_expression(initializer)?;
                let ty = if let Some(annotation) = type_annotation {
                    let annotated = self.lower_type(annotation, &HashMap::new())?;
                    self.solver
                        .unify(&init_ty, &annotated, Some(initializer.span))?;
                    annotated
                } else {
                    init_ty
                };
                self.insert_local(name.name.clone(), Scheme::mono(self.solver.apply(&ty)));
            }
            StatementKind::Return(Some(expr)) => {
                let ty = self.infer_expression(expr)?;
                let expected = expected_return
                    .cloned()
                    .or_else(|| self.current_return.clone());
                if let Some(expected) = expected {
                    self.solver.unify(&ty, &expected, Some(expr.span))?;
                }
            }
            StatementKind::Return(None) => {
                let expected = expected_return
                    .cloned()
                    .or_else(|| self.current_return.clone());
                if let Some(expected) = expected {
                    self.solver.unify(
                        &InferType::Con("Unit".into()),
                        &expected,
                        Some(stmt.span),
                    )?;
                }
            }
            StatementKind::Conc { body } => {
                self.check_block(body, expected_return)?;
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.infer_expression(condition)?;
                self.solver
                    .unify(&cond, &InferType::Con("Bool".into()), Some(condition.span))?;
                self.check_block(then_branch, expected_return)?;
                if let Some(else_branch) = else_branch {
                    match &else_branch.node {
                        ElseBranchKind::Block(block) => {
                            self.check_block(block, expected_return)?;
                        }
                        ElseBranchKind::If(stmt) => {
                            self.check_statement(stmt, expected_return)?;
                        }
                    }
                }
            }
            StatementKind::Match { expr, arms } => {
                let scrutinee = self.infer_expression(expr)?;
                for arm in arms {
                    self.push_scope();
                    self.bind_pattern(&arm.node.pattern, &scrutinee)?;
                    if let Some(guard) = &arm.node.guard {
                        let guard_ty = self.infer_expression(guard)?;
                        self.solver.unify(
                            &guard_ty,
                            &InferType::Con("Bool".into()),
                            Some(guard.span),
                        )?;
                    }
                    self.infer_expression(&arm.node.body)?; // ← just infer, don't constrain to Unit
                    self.pop_scope();
                }
            }
            StatementKind::Loop { kind, body } => match &kind.node {
                LoopKindKind::For {
                    pattern, iterator, ..
                } => {
                    let iter_ty = self.infer_expression(iterator)?;
                    let elem_ty = self
                        .array_elem_type(&iter_ty)
                        .unwrap_or_else(|| self.solver.fresh_var());
                    self.push_scope();
                    self.bind_pattern(pattern, &elem_ty)?;
                    self.check_block(body, expected_return)?;
                    self.pop_scope();
                }
                LoopKindKind::While { condition, .. } => {
                    let cond = self.infer_expression(condition)?;
                    self.solver.unify(
                        &cond,
                        &InferType::Con("Bool".into()),
                        Some(condition.span),
                    )?;
                    self.check_block(body, expected_return)?;
                }
                LoopKindKind::Block(block) => {
                    self.check_block(block, expected_return)?;
                }
            },
            StatementKind::Break
            | StatementKind::Continue
            | StatementKind::Empty
            | StatementKind::UseDeclaration(_) => {}
        }
        Ok(())
    }

    fn infer_expression(&mut self, expr: &Expression) -> Result<InferType, TypeError> {
        let ty = match &expr.node {
            ExpressionKind::Literal(lit) => self.literal_type(lit, expr.span)?,
            ExpressionKind::Identifier(id) => {
                if id.name.as_str() == "chan" {
                    return Ok(InferType::App("Chan".into(), vec![self.solver.fresh_var()]));
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
                        self.solver.unify(
                            &ty,
                            &InferType::Con("Int32".into()),
                            Some(inner.span),
                        )?;
                        InferType::Con("Int32".into())
                    }
                    Operator::Not => {
                        self.solver
                            .unify(&ty, &InferType::Con("Bool".into()), Some(inner.span))?;
                        InferType::Con("Bool".into())
                    }
                    _ => ty,
                }
            }
            ExpressionKind::BinaryOp { op, left, right } => {
                self.infer_binary(op, left, right, expr.span)?
            }
            ExpressionKind::Call { func, args } => {
                // Handle builtins that are variadic/untyped at call site
                if let ExpressionKind::Identifier(id) = &func.node {
                    if id.name == "chan" {
                        return Ok(InferType::App(
                            "Ref".into(),
                            vec![InferType::App("Chan".into(), vec![self.solver.fresh_var()])],
                        ));
                    }
                    if self.try_inner_func(id.name.clone()) {
                        for arg in args {
                            self.infer_expression(arg)?;
                        } // still typecheck args
                        return Ok(InferType::Con("Unit".into()));
                    }
                }

                // ── Method call: base.method(args) ──────────────────────────────
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_ty = self.infer_expression(base)?;
                    let mut arg_tys = Vec::new();
                    for arg in args {
                        arg_tys.push(self.infer_expression(arg)?);
                    }

                    let inner_ty = match self.solver.apply(&base_ty) {
                        InferType::App(ref name, ref a) if name == "Ref" && a.len() == 1 => {
                            self.solver.apply(&a[0])
                        }
                        other => other,
                    };

                    return Ok(match inner_ty {
                        InferType::App(name, mut ty_args) => {
                            if name == "Array" && ty_args.len() == 1 && field.name == "push" {
                                let elem = ty_args.remove(0);
                                if let Some(first) = arg_tys.first().cloned() {
                                    self.solver.unify(&first, &elem, Some(expr.span))?;
                                }
                                InferType::Con("Unit".into())
                            } else if name == "Chan" && ty_args.len() == 1 {
                                let elem = ty_args.remove(0);
                                match field.name.as_str() {
                                    "send" => {
                                        if let Some(first) = arg_tys.first().cloned() {
                                            self.solver.unify(&first, &elem, Some(expr.span))?;
                                        }
                                        InferType::Con("Unit".into())
                                    }
                                    "consume" => {
                                        if let Some(first) = arg_tys.first().cloned() {
                                            let unwrapped = match self.solver.apply(&first) {
                                                InferType::App(n, a)
                                                    if n == "Ref" && a.len() == 1 =>
                                                {
                                                    self.solver.apply(&a[0])
                                                }
                                                other => other,
                                            };
                                            self.solver.unify(
                                                &unwrapped,
                                                &InferType::App("Chan".into(), vec![elem]),
                                                Some(expr.span),
                                            )?;
                                        }
                                        InferType::Con("Unit".into())
                                    }
                                    "recv" => self.make_option_ty(elem),
                                    "try_recv" => self.make_option_ty(elem),
                                    _ => self.infer_method_call(
                                        &name,
                                        &field.name,
                                        base_ty,
                                        arg_tys,
                                        expr.span,
                                    )?,
                                }
                            } else {
                                self.infer_method_call(
                                    &name,
                                    &field.name,
                                    base_ty,
                                    arg_tys,
                                    expr.span,
                                )?
                            }
                        }
                        InferType::Con(type_name) => self.infer_method_call(
                            &type_name,
                            &field.name,
                            base_ty,
                            arg_tys,
                            expr.span,
                        )?,
                        _ => {
                            // Unknown base type, fall through to generic call path
                            let callee = self.infer_expression(func)?;
                            let ret = self.solver.fresh_var();
                            self.solver.unify(
                                &callee,
                                &InferType::Fn(arg_tys, Box::new(ret.clone())),
                                Some(expr.span),
                            )?;
                            ret
                        }
                    });
                }

                // ── Generic call ────────────────────────────────────────────────
                let mut arg_tys = Vec::new();
                for arg in args {
                    arg_tys.push(self.infer_expression(arg)?);
                }
                let callee = self.infer_expression(func)?;
                let ret = self.solver.fresh_var();
                self.solver.unify(
                    &callee,
                    &InferType::Fn(arg_tys, Box::new(ret.clone())),
                    Some(expr.span),
                )?;
                ret
            }
            ExpressionKind::FieldAccess { base, field } => {
                let base_ty = self.infer_expression(base)?;
                match self.solver.apply(&base_ty) {
                    InferType::Con(name) => {
                        if field.name == "0" {
                            self.registry
                                .newtype_alias
                                .get(&name)
                                .cloned()
                                .unwrap_or_else(|| self.solver.fresh_var())
                        } else if let Some(fields) = self.registry.struct_fields.get(&name) {
                            fields
                                .get(&field.name)
                                .cloned()
                                .unwrap_or_else(|| self.solver.fresh_var())
                        } else {
                            self.solver.fresh_var()
                        }
                    }
                    _ => self.solver.fresh_var(),
                }
            }
            ExpressionKind::IndexAccess { base, index } => {
                let base_ty = self.infer_expression(base)?;
                let index_ty = self.infer_expression(index)?;
                self.solver
                    .unify(&index_ty, &InferType::Con("Int32".into()), Some(index.span))?;
                if let Some(elem) = self.array_elem_type(&base_ty) {
                    self.make_option_ty(elem)
                } else {
                    self.solver.fresh_var()
                }
            }
            ExpressionKind::StructInit { name, fields } => {
                for (_, field_expr) in fields {
                    self.infer_expression(field_expr)?;
                }
                InferType::Con(name.name.clone())
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(base) = base {
                    self.infer_expression(base)?;
                }
                for (_, field_expr) in fields {
                    self.infer_expression(field_expr)?;
                }
                self.solver.fresh_var()
            }
            ExpressionKind::Block(block) => self.check_block(block, None)?,
            ExpressionKind::Pipe { left, right } => {
                let left_ty = self.infer_expression(left)?;
                let right_ty = self.infer_expression(right)?;
                let ret = self.solver.fresh_var();
                self.solver.unify(
                    &right_ty,
                    &InferType::Fn(vec![left_ty], Box::new(ret.clone())),
                    Some(expr.span),
                )?;
                ret
            }
            ExpressionKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee_ty = self.infer_expression(scrutinee)?;
                let arm_ty = self.solver.fresh_var();
                for arm in arms {
                    self.push_scope();
                    self.bind_pattern(&arm.node.pattern, &scrutinee_ty)?;
                    if let Some(guard) = &arm.node.guard {
                        let guard_ty = self.infer_expression(guard)?;
                        self.solver.unify(
                            &guard_ty,
                            &InferType::Con("Bool".into()),
                            Some(guard.span),
                        )?;
                    }
                    let body_ty = self.infer_expression(&arm.node.body)?;
                    self.solver.unify(&body_ty, &arm_ty, Some(arm.span))?;
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
                    self.solver.unify(&then_ty, &else_ty, Some(expr.span))?;
                }
                then_ty
            }
            ExpressionKind::Placeholder(_) => self.solver.fresh_var(),
        };
        let applied = self.solver.apply(&ty);
        self.types.insert(expr.id, applied.clone());
        Ok(applied)
    }

    fn infer_method_call(
        &mut self,
        type_name: &str,
        method: &str,
        base_ty: InferType,
        arg_tys: Vec<InferType>,
        span: Span,
    ) -> Result<InferType, TypeError> {
        let rt_name = format!("__ty_rt__{}__{}", type_name, method);
        let local_name = format!("__ty_method__{}__{}", type_name, method);
        let scheme = if let Some(s) = self.lookup(&local_name).cloned() {
            s
        } else if let Some(s) = self.lookup(&rt_name).cloned() {
            s
        } else {
            return Err(TypeError::UnknownIdentifier {
                name: local_name,
                span: Some(span),
            });
        };
        let callee = self.instantiate(&scheme);
        let full_args = match self.solver.apply(&callee) {
            InferType::Fn(params, _) if params.len() == arg_tys.len() => arg_tys,
            _ => {
                let mut args = vec![base_ty];
                args.extend(arg_tys);
                args
            }
        };
        let ret = self.solver.fresh_var();
        self.solver.unify(
            &callee,
            &InferType::Fn(full_args, Box::new(ret.clone())),
            Some(span),
        )?;
        Ok(ret)
    }

    pub fn lower_type(
        &mut self,
        ty: &Type,
        generic_vars: &HashMap<String, TypeVarId>,
    ) -> Result<InferType, TypeError> {
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

    pub fn generalize(&self, ty: &InferType, exclude: Option<&str>) -> Scheme {
        let ty = self.solver.apply(ty);
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
        match self.solver.apply(ty) {
            InferType::Var(var) => HashSet::from([var]),
            InferType::Con(_) => HashSet::new(),
            InferType::App(_, args) => {
                args.iter()
                    .map(|a| self.free_type_vars(a))
                    .fold(HashSet::new(), |mut acc, x| {
                        acc.extend(x);
                        acc
                    })
            }
            InferType::Fn(params, ret) => {
                let mut vars = params.iter().map(|p| self.free_type_vars(p)).fold(
                    HashSet::new(),
                    |mut acc, x| {
                        acc.extend(x);
                        acc
                    },
                );
                vars.extend(self.free_type_vars(&ret));
                vars
            }
            InferType::FixedArray(elem, _) => self.free_type_vars(&elem),
        }
    }
}
