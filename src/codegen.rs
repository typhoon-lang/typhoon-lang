use crate::ast::*;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
    pub preamble: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrFunction {
    pub name: String,
    pub body: String,
    pub ret_type: String,
    pub params: Vec<(String, String)>,
}

pub struct Codegen;

impl Codegen {
    pub fn lower_module(module: &Module) -> IrModule {
        let mut functions = Vec::new();
        let mut builder = IrBuilder::new();
        let preamble = builder.collect_types(module);
        for decl in &module.declarations {
            if let Declaration::Function { name, return_type, body, params, .. } = decl {
                let ret_ty = return_type
                    .as_ref()
                    .map(|ty| builder.lower_type(ty))
                    .unwrap_or_else(|| "void".to_string());
                let body_ir = builder.emit_function(name, params, &ret_ty, body);
                let param_list = params
                    .iter()
                    .map(|p| (p.name.name.clone(), builder.lower_type(&p.type_annotation)))
                    .collect();
                functions.push(IrFunction {
                    name: name.name.clone(),
                    body: body_ir,
                    ret_type: ret_ty,
                    params: param_list,
                });
            }
        }
        IrModule { functions, preamble }
    }
}

struct IrBuilder {
    lines: Vec<String>,
    next_tmp: usize,
    last_value: Option<String>,
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    type_decls: Vec<String>,
    next_label: usize,
    struct_fields: HashMap<String, Vec<(String, String)>>,
    func_sigs: HashMap<String, (String, Vec<String>)>,
}

impl IrBuilder {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            next_tmp: 0,
            last_value: None,
            locals: HashMap::new(),
            locals_type: HashMap::new(),
            type_decls: Vec::new(),
            next_label: 0,
            struct_fields: HashMap::new(),
            func_sigs: HashMap::new(),
        }
    }

    fn collect_types(&mut self, module: &Module) -> Vec<String> {
        self.type_decls.clear();
        self.struct_fields.clear();
        self.func_sigs.clear();
        for decl in &module.declarations {
            match decl {
                Declaration::Struct { name, fields, .. } => {
                    let mut field_types = Vec::new();
                    let mut field_map = Vec::new();
                    for (field, ty) in fields {
                        let lowered = self.lower_type(ty);
                        field_types.push(lowered.clone());
                        field_map.push((field.name.clone(), lowered));
                    }
                    let body = field_types.join(", ");
                    self.type_decls
                        .push(format!("%struct.{} = type {{ {} }}", name.name, body));
                    self.struct_fields.insert(name.name.clone(), field_map);
                }
                Declaration::Enum { name, .. } => {
                    self.type_decls
                        .push(format!("%enum.{} = type opaque", name.name));
                }
                Declaration::Newtype { name, type_alias } => {
                    let alias = self.lower_type(type_alias);
                    self.type_decls
                        .push(format!("%newtype.{} = type {}", name.name, alias));
                }
                Declaration::Function { name, return_type, params, .. } => {
                    let ret_ty = return_type
                        .as_ref()
                        .map(|ty| self.lower_type(ty))
                        .unwrap_or_else(|| "void".to_string());
                    let param_types = params
                        .iter()
                        .map(|p| self.lower_type(&p.type_annotation))
                        .collect();
                    self.func_sigs
                        .insert(name.name.clone(), (ret_ty, param_types));
                }
                _ => {}
            }
        }
        self.type_decls.clone()
    }

    fn emit_function(
        &mut self,
        _name: &Identifier,
        params: &[Parameter],
        ret_ty: &str,
        body: &Block,
    ) -> String {
        self.lines.clear();
        self.locals.clear();
        self.last_value = None;
        self.lines.push("entry:".to_string());

        for param in params {
            let param_ty = self.lower_type(&param.type_annotation);
            let slot = self.next_register();
            self.lines.push(format!("  {} = alloca {}", slot, param_ty));
            self.lines.push(format!(
                "  store {} %{}, {}* {}",
                param_ty, param.name.name, param_ty, slot
            ));
            self.locals.insert(param.name.name.clone(), slot);
            self.locals_type
                .insert(param.name.name.clone(), param_ty);
        }

        self.emit_block(body);
        self.finish(ret_ty)
    }

    fn finish(&mut self, return_type: &str) -> String {
        if let Some(value) = self.last_value.take() {
            if return_type != "void" {
                self.lines.push(format!("  ret {} {}", return_type, value));
            } else {
                self.lines.push("  ret void".to_string());
            }
        } else {
            self.lines.push("  ret void".to_string());
        }
        self.lines.join("\n")
    }

    fn emit_block(&mut self, block: &Block) {
        for stmt in &block.statements {
            match stmt {
                Statement::Return(Some(expr)) => {
                    let value = self.emit_expr(expr);
                    self.last_value = Some(value);
                    return;
                }
                Statement::Return(None) => {
                    self.last_value = None;
                    return;
                }
                Statement::LetBinding { name, initializer, type_annotation, .. } => {
                    let value = self.emit_expr(initializer);
                    let ty = type_annotation
                        .as_ref()
                        .map(|ty| self.lower_type(ty))
                        .unwrap_or_else(|| "i32".to_string());
                    let alloca = self.next_register();
                    self.lines.push(format!("  {} = alloca {}", alloca, ty));
                    self.lines.push(format!("  store {} {}, {}* {}", ty, value, ty, alloca));
                    self.locals.insert(name.name.clone(), alloca);
                    self.locals_type.insert(name.name.clone(), ty);
                }
                Statement::Expression(expr) => {
                    let _ = self.emit_expr(expr);
                }
                Statement::If { condition, then_branch, else_branch } => {
                    let cond_val = self.emit_expr(condition);
                    let then_label = self.next_block("then");
                    let else_label = self.next_block("else");
                    let merge_label = self.next_block("if_merge");
                    self.lines.push(format!(
                        "  br i1 {}, label %{}, label %{}",
                        cond_val, then_label, else_label
                    ));

                    self.lines.push(format!("{}:", then_label));
                    self.emit_block(then_branch);
                    self.lines.push(format!("  br label %{}", merge_label));

                    self.lines.push(format!("{}:", else_label));
                    if let Some(stmt) = else_branch {
                        self.emit_statement(stmt);
                    }
                    self.lines.push(format!("  br label %{}", merge_label));

                    self.lines.push(format!("{}:", merge_label));
                }
                Statement::Match { expr, arms } => {
                    let discr = self.emit_expr(expr);
                    let merge_label = self.next_block("match_merge");
                    for (idx, arm) in arms.iter().enumerate() {
                        let arm_label = self.next_block(&format!("match_arm_{}", idx));
                        self.lines.push(format!("  br label %{}", arm_label));
                        self.lines.push(format!("{}:", arm_label));
                        let _ = self.emit_expr(&arm.body);
                        self.lines.push(format!("  br label %{}", merge_label));
                    }
                    self.lines.push(format!("{}:", merge_label));
                    let _ = discr;
                }
                Statement::Loop { kind, body } => {
                    let loop_label = self.next_block("loop");
                    let loop_body = self.next_block("loop_body");
                    let loop_end = self.next_block("loop_end");
                    self.lines.push(format!("  br label %{}", loop_label));
                    self.lines.push(format!("{}:", loop_label));
                    match kind {
                        LoopKind::While { condition, .. } => {
                            let cond_val = self.emit_expr(condition);
                            self.lines.push(format!(
                                "  br i1 {}, label %{}, label %{}",
                                cond_val, loop_body, loop_end
                            ));
                        }
                        _ => {
                            self.lines.push(format!("  br label %{}", loop_body));
                        }
                    }
                    self.lines.push(format!("{}:", loop_body));
                    self.emit_block(body);
                    self.lines.push(format!("  br label %{}", loop_label));
                    self.lines.push(format!("{}:", loop_end));
                }
                Statement::Conc { body } => {
                    self.emit_block(body);
                }
                _ => {}
            }
        }
        if let Some(expr) = &block.trailing_expression {
            let value = self.emit_expr(expr);
            self.last_value = Some(value);
        }
    }

    fn emit_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Return(Some(expr)) => {
                let value = self.emit_expr(expr);
                self.last_value = Some(value);
            }
            Statement::Return(None) => {
                self.last_value = None;
            }
            Statement::Expression(expr) => {
                let _ = self.emit_expr(expr);
            }
            Statement::If { .. } | Statement::Match { .. } | Statement::Loop { .. } => {
                self.emit_block(&Block {
                    statements: vec![stmt.clone()],
                    trailing_expression: None,
                });
            }
            _ => {}
        }
    }

    fn emit_expr(&mut self, expr: &Expression) -> String {
        match expr {
            Expression::Literal(Literal::Int(value)) => value.to_string(),
            Expression::Literal(Literal::Bool(value)) => {
                if *value {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            Expression::Identifier(id) => {
                if let Some(slot) = self.locals.get(&id.name).cloned() {
                    let ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
                    let tmp = self.next_register();
                    self.lines
                        .push(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
                    tmp
                } else {
                    id.name.clone()
                }
            }
            Expression::BinaryOp { op, left, right } => {
                let lhs = self.emit_expr(left);
                let rhs = self.emit_expr(right);
                let tmp = self.next_register();
                let instr = match op {
                    Operator::Add => format!("  {} = add i32 {}, {}", tmp, lhs, rhs),
                    Operator::Sub => format!("  {} = sub i32 {}, {}", tmp, lhs, rhs),
                    Operator::Mul => format!("  {} = mul i32 {}, {}", tmp, lhs, rhs),
                    Operator::Div => format!("  {} = sdiv i32 {}, {}", tmp, lhs, rhs),
                    _ => format!("  {} = add i32 {}, {}", tmp, lhs, rhs),
                };
                self.lines.push(instr);
                tmp
            }
            Expression::StructInit { name, fields } => {
                let struct_ty = format!("%struct.{}", name.name);
                let tmp = self.next_register();
                self.lines.push(format!("  {} = alloca {}", tmp, struct_ty));
                for (field_name, field_expr) in fields {
                    let field_value = self.emit_expr(field_expr);
                    let field_index = self
                        .struct_fields
                        .get(&name.name)
                        .and_then(|fields| fields.iter().position(|f| f.0 == field_name.name))
                        .unwrap_or(0);
                    let field_type = self
                        .struct_fields
                        .get(&name.name)
                        .and_then(|fields| fields.get(field_index))
                        .map(|(_, ty)| ty.clone())
                        .unwrap_or_else(|| "i32".to_string());
                    let gep = self.next_register();
                    self.lines.push(format!(
                        "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                        gep, struct_ty, struct_ty, tmp, field_index
                    ));
                    self.lines
                        .push(format!("  store {} {}, {}* {}", field_type, field_value, field_type, gep));
                }
                tmp
            }
            Expression::MergeExpression { base, fields } => {
                let (base_ptr, base_ty) = if let Some(Expression::Identifier(id)) = base.as_deref() {
                    let ptr = self
                        .locals
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| id.name.clone());
                    let ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| "%struct.?".to_string());
                    (ptr, ty)
                } else if let Some(base_expr) = base {
                    (self.emit_expr(base_expr), "%struct.?".to_string())
                } else {
                    ("0".to_string(), "%struct.?".to_string())
                };

                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_expr) in fields {
                    let value = self.emit_expr(field_expr);
                    let field_index = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.iter().position(|f| f.0 == field_name.name))
                        .unwrap_or(0);
                    let field_type = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.get(field_index))
                        .map(|(_, ty)| ty.clone())
                        .unwrap_or_else(|| "i32".to_string());
                    let gep = self.next_register();
                    self.lines.push(format!(
                        "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                        gep, base_ty, base_ty, base_ptr, field_index
                    ));
                    self.lines
                        .push(format!("  store {} {}, {}* {}", field_type, value, field_type, gep));
                }
                base_ptr
            }
            Expression::Call { func, args } => {
                if let Expression::Identifier(id) = func.as_ref() {
                    let (ret_ty, param_types) =
                        match self.func_sigs.get(&id.name) {
                            Some(sig) => sig.clone(),
                            None => ("i32".to_string(), vec![]),
                        };
                    let mut arg_pairs = Vec::new();
                    for (idx, arg) in args.iter().enumerate() {
                        let val = self.emit_expr(arg);
                        let ty = param_types.get(idx).cloned().unwrap_or_else(|| "i32".to_string());
                        arg_pairs.push(format!("{} {}", ty, val));
                    }
                    let tmp = self.next_register();
                    self.lines.push(format!(
                        "  {} = call {} @{}({})",
                        tmp,
                        ret_ty,
                        id.name,
                        arg_pairs.join(", ")
                    ));
                    return tmp;
                }
                "0".to_string()
            }
            Expression::Match { expr, arms } => {
                let _ = self.emit_expr(expr);
                for arm in arms {
                    let _ = self.emit_expr(&arm.body);
                }
                "0".to_string()
            }
            _ => "0".to_string(),
        }
    }

    fn next_register(&mut self) -> String {
        let name = format!("%t{}", self.next_tmp);
        self.next_tmp += 1;
        name
    }

    fn lower_type(&self, ty: &Type) -> String {
        match ty.name.as_str() {
            "Int32" => "i32".to_string(),
            "Bool" => "i1".to_string(),
            "Str" => "i8*".to_string(),
            "Buf" => "%struct.Buf*".to_string(),
            "ref" => "i8*".to_string(),
            name => format!("%struct.{}", name),
        }
    }

    fn next_block(&mut self, prefix: &str) -> String {
        let label = format!("{}_{}", prefix, self.next_label);
        self.next_label += 1;
        label
    }
}

impl IrModule {
    pub fn to_llvm_ir(&self) -> String {
        let mut out = Vec::new();
        out.extend(self.preamble.iter().cloned());
        for func in &self.functions {
            let params = if func.params.is_empty() {
                "".to_string()
            } else {
                func.params
                    .iter()
                    .map(|(name, ty)| format!("{} %{}", ty, name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push(format!(
                "define {} @{}({}) {{",
                func.ret_type, func.name, params
            ));
            out.push(func.body.clone());
            out.push("}".to_string());
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

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

    fn parse_module(source: &str) -> Module {
        Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap()
    }

    #[test]
    fn lowers_function_declarations() {
        let source = "fn main(a: Int32) -> Int32 { return a; }";
        let module = parse_module(source);
        let ir = Codegen::lower_module(&module);
        assert_eq!(ir.functions.len(), 1);
        assert_eq!(ir.functions[0].name, "main");
        assert_eq!(ir.functions[0].ret_type, "i32");
        assert_eq!(ir.functions[0].params.len(), 1);
    }

    #[test]
    fn emits_basic_llvm_ir() {
        let source = "fn main() -> Int32 { return 0; }";
        let module = parse_module(source);
        let ir = Codegen::lower_module(&module);
        let text = ir.to_llvm_ir();
        assert!(text.contains("define i32 @main()"));
        assert!(text.contains("ret i32 0"));
    }

    #[test]
    fn lowers_let_bindings() {
        let source = "fn main() -> Int32 { let x: Int32 = 3; return x; }";
        let module = parse_module(source);
        let ir = Codegen::lower_module(&module);
        let text = ir.to_llvm_ir();
        assert!(text.contains("alloca i32"));
        assert!(text.contains("store i32 3"));
        assert!(text.contains("load i32"));
    }

    #[test]
    fn emits_if_branches() {
        let source = "fn main(flag: Bool) -> Int32 { if flag { return 1; } else { return 2; } }";
        let module = parse_module(source);
        let ir = Codegen::lower_module(&module);
        let text = ir.to_llvm_ir();
        assert!(text.contains("br i1"));
        assert!(text.contains("if_merge"));
    }

    #[test]
    fn lowers_struct_init_and_merge() {
        let source = "struct User { id: Int32, age: Int32 } fn main() -> Int32 { let user: User = User { id: 1, age: 2 }; let updated: User = { ...user, age: 3 }; return 0; }";
        let module = parse_module(source);
        let ir = Codegen::lower_module(&module);
        let text = ir.to_llvm_ir();
        assert!(text.contains("%struct.User = type"));
        assert!(text.contains("getelementptr"));
        assert!(text.contains("store i32 1"));
        assert!(text.contains("store i32 3"));
    }
}
