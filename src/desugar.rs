use crate::ast::*;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::span::Span;
use std::collections::HashMap;

pub struct Desugar {
    next_tmp: usize,
    next_buf: usize,
    next_id: u32,
}

impl Desugar {
    pub fn new() -> Self {
        Self {
            next_tmp: 0,
            next_buf: 0,
            next_id: 1,
        }
    }

    pub fn rename_declaration(
        &mut self,
        decl: &mut Declaration,
        aliases: &HashMap<String, String>,
    ) {
        self.rename_decl_name(decl, aliases);
        self.rename_in_decl(decl, aliases);
    }

    pub fn desugar_declaration(&mut self, decl: &mut Declaration) -> Result<(), String> {
        self.seed_next_id_from_decl(decl);
        match &mut decl.node {
            DeclarationKind::Function {
                body,
                params,
                return_type,
                ..
            } => {
                for p in params {
                    self.desugar_type(&mut p.type_annotation)?;
                }
                if let Some(ret) = return_type {
                    self.desugar_type(ret)?;
                }
                self.desugar_block(body)?;
            }
            DeclarationKind::Struct { fields, .. } => {
                for (_, ty) in fields {
                    self.desugar_type(ty)?;
                }
            }
            DeclarationKind::Enum { variants, .. } => {
                for v in variants {
                    if let Some(payload) = &mut v.node.payload {
                        match &mut payload.node {
                            EnumVariantPayloadKind::Unit(ty) => self.desugar_type(ty)?,
                            EnumVariantPayloadKind::Tuple(types) => {
                                for ty in types {
                                    self.desugar_type(ty)?;
                                }
                            }
                            EnumVariantPayloadKind::Struct(fields) => {
                                for (_, ty) in fields {
                                    self.desugar_type(ty)?;
                                }
                            }
                        }
                    }
                }
            }
            DeclarationKind::Newtype { type_alias, .. } => self.desugar_type(type_alias)?,
            DeclarationKind::Interface { methods, .. } => {
                for sig in methods {
                    for p in &mut sig.node.params {
                        self.desugar_type(&mut p.type_annotation)?;
                    }
                    if let Some(ret) = &mut sig.node.return_type {
                        self.desugar_type(ret)?;
                    }
                }
            }
            DeclarationKind::Impl {
                trait_name,
                type_name,
                methods,
                ..
            } => {
                self.desugar_type(trait_name)?;
                self.desugar_type(type_name)?;
                for m in methods {
                    self.desugar_declaration(m)?;
                }
            }
            DeclarationKind::Extension {
                type_constraint,
                methods,
                ..
            } => {
                self.desugar_type(type_constraint)?;
                for m in methods {
                    self.desugar_declaration(m)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn seed_next_id_from_decl(&mut self, decl: &Declaration) {
        fn max_id_expr(expr: &Expression) -> u32 {
            let mut max_id = expr.id.0;
            match &expr.node {
                ExpressionKind::BinaryOp { left, right, .. } => {
                    max_id = max_id.max(max_id_expr(left));
                    max_id = max_id.max(max_id_expr(right));
                }
                ExpressionKind::UnaryOp { expr, .. } => max_id = max_id.max(max_id_expr(expr)),
                ExpressionKind::Call { func, args } => {
                    max_id = max_id.max(max_id_expr(func));
                    for a in args {
                        max_id = max_id.max(max_id_expr(a));
                    }
                }
                ExpressionKind::FieldAccess { base, .. } => max_id = max_id.max(max_id_expr(base)),
                ExpressionKind::IndexAccess { base, index } => {
                    max_id = max_id.max(max_id_expr(base));
                    max_id = max_id.max(max_id_expr(index));
                }
                ExpressionKind::StructInit { fields, .. } => {
                    for (_id, e) in fields {
                        max_id = max_id.max(max_id_expr(e));
                    }
                }
                ExpressionKind::MergeExpression { base, fields } => {
                    if let Some(b) = base {
                        max_id = max_id.max(max_id_expr(b));
                    }
                    for (_id, e) in fields {
                        max_id = max_id.max(max_id_expr(e));
                    }
                }
                ExpressionKind::Block(b) => {
                    for s in &b.statements {
                        max_id = max_id.max(max_id_stmt(s));
                    }
                    if let Some(t) = &b.trailing_expression {
                        max_id = max_id.max(max_id_expr(t));
                    }
                }
                ExpressionKind::Match { expr, arms } => {
                    max_id = max_id.max(max_id_expr(expr));
                    for a in arms {
                        max_id = max_id.max(a.id.0);
                        max_id = max_id.max(max_id_expr(&a.node.body));
                        if let Some(g) = &a.node.guard {
                            max_id = max_id.max(max_id_expr(g));
                        }
                    }
                }
                ExpressionKind::TryOperator { expr } => max_id = max_id.max(max_id_expr(expr)),
                ExpressionKind::IfLet {
                    expr,
                    then,
                    else_branch,
                    ..
                } => {
                    max_id = max_id.max(max_id_expr(expr));
                    for s in &then.statements {
                        max_id = max_id.max(max_id_stmt(s));
                    }
                    if let Some(t) = &then.trailing_expression {
                        max_id = max_id.max(max_id_expr(t));
                    }
                    if let Some(e) = else_branch {
                        max_id = max_id.max(max_id_expr(e));
                    }
                }
                ExpressionKind::Literal(Literal {
                    kind: LiteralKind::Array(items),
                    ..
                }) => {
                    for e in items {
                        max_id = max_id.max(max_id_expr(e));
                    }
                }
                _ => {}
            }
            max_id
        }

        fn max_id_stmt(stmt: &Statement) -> u32 {
            let mut max_id = stmt.id.0;
            match &stmt.node {
                StatementKind::LetBinding { initializer, .. } => {
                    max_id = max_id.max(max_id_expr(initializer));
                }
                StatementKind::Expression(e) => max_id = max_id.max(max_id_expr(e)),
                StatementKind::Return(Some(e)) => max_id = max_id.max(max_id_expr(e)),
                StatementKind::Conc { body } => {
                    for s in &body.statements {
                        max_id = max_id.max(max_id_stmt(s));
                    }
                    if let Some(t) = &body.trailing_expression {
                        max_id = max_id.max(max_id_expr(t));
                    }
                }
                StatementKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    max_id = max_id.max(max_id_expr(condition));
                    for s in &then_branch.statements {
                        max_id = max_id.max(max_id_stmt(s));
                    }
                    if let Some(eb) = else_branch {
                        match &eb.node {
                            ElseBranchKind::Block(b) => {
                                for s in &b.statements {
                                    max_id = max_id.max(max_id_stmt(s));
                                }
                                if let Some(t) = &b.trailing_expression {
                                    max_id = max_id.max(max_id_expr(t));
                                }
                            }
                            ElseBranchKind::If(s) => max_id = max_id.max(max_id_stmt(s)),
                        }
                    }
                }
                StatementKind::Loop { kind, body } => {
                    match &kind.node {
                        LoopKindKind::For {
                            iterator,
                            body: inner,
                            ..
                        } => {
                            max_id = max_id.max(max_id_expr(iterator));
                            for s in &inner.statements {
                                max_id = max_id.max(max_id_stmt(s));
                            }
                            if let Some(t) = &inner.trailing_expression {
                                max_id = max_id.max(max_id_expr(t));
                            }
                        }
                        LoopKindKind::While {
                            condition,
                            body: inner,
                        } => {
                            max_id = max_id.max(max_id_expr(condition));
                            for s in &inner.statements {
                                max_id = max_id.max(max_id_stmt(s));
                            }
                            if let Some(t) = &inner.trailing_expression {
                                max_id = max_id.max(max_id_expr(t));
                            }
                        }
                        LoopKindKind::Block(b) => {
                            for s in &b.statements {
                                max_id = max_id.max(max_id_stmt(s));
                            }
                            if let Some(t) = &b.trailing_expression {
                                max_id = max_id.max(max_id_expr(t));
                            }
                        }
                    }
                    for s in &body.statements {
                        max_id = max_id.max(max_id_stmt(s));
                    }
                    if let Some(t) = &body.trailing_expression {
                        max_id = max_id.max(max_id_expr(t));
                    }
                }
                StatementKind::Match { expr, arms } => {
                    max_id = max_id.max(max_id_expr(expr));
                    for a in arms {
                        max_id = max_id.max(a.id.0);
                        max_id = max_id.max(max_id_expr(&a.node.body));
                        if let Some(g) = &a.node.guard {
                            max_id = max_id.max(max_id_expr(g));
                        }
                    }
                }
                _ => {}
            }
            max_id
        }

        let mut max_id = decl.id.0;
        if let DeclarationKind::Function { body, .. } = &decl.node {
            for s in &body.statements {
                max_id = max_id.max(max_id_stmt(s));
            }
            if let Some(t) = &body.trailing_expression {
                max_id = max_id.max(max_id_expr(t));
            }
        }
        self.next_id = max_id.saturating_add(1).max(self.next_id);
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    fn mk_expr(&mut self, kind: ExpressionKind, span: Span) -> Expression {
        Spanned::new(kind, span, self.alloc_id())
    }

    fn mk_pat(&mut self, kind: PatternKind, span: Span) -> Pattern {
        Spanned::new(kind, span, self.alloc_id())
    }

    fn mk_arm(&mut self, kind: MatchArmKind, span: Span) -> MatchArm {
        Spanned::new(kind, span, self.alloc_id())
    }

    fn mk_stmt(&mut self, kind: StatementKind, span: Span) -> Statement {
        Spanned::new(kind, span, self.alloc_id())
    }

    fn rename_decl_name(&self, decl: &mut Declaration, aliases: &HashMap<String, String>) {
        let rename = |id: &mut Identifier| {
            if let Some(n) = aliases.get(&id.name) {
                id.name = n.clone();
            }
        };
        match &mut decl.node {
            DeclarationKind::Function { name, .. } => rename(name),
            DeclarationKind::Struct { name, .. } => rename(name),
            DeclarationKind::Enum { name, .. } => rename(name),
            DeclarationKind::Newtype { name, .. } => rename(name),
            DeclarationKind::Interface { name, .. } => rename(name),
            _ => {}
        }
    }

    fn rename_in_decl(&self, decl: &mut Declaration, aliases: &HashMap<String, String>) {
        match &mut decl.node {
            DeclarationKind::Function {
                params,
                return_type,
                body,
                ..
            } => {
                for p in params {
                    self.rename_type(&mut p.type_annotation, aliases);
                }
                if let Some(ret) = return_type {
                    self.rename_type(ret, aliases);
                }
                self.rename_block(body, aliases);
            }
            DeclarationKind::Struct { fields, .. } => {
                for (_id, ty) in fields {
                    self.rename_type(ty, aliases);
                }
            }
            DeclarationKind::Enum { variants, .. } => {
                for v in variants {
                    if let Some(payload) = &mut v.node.payload {
                        self.rename_enum_payload(payload, aliases);
                    }
                }
            }
            DeclarationKind::Newtype { type_alias, .. } => self.rename_type(type_alias, aliases),
            DeclarationKind::Interface { methods, .. } => {
                for sig in methods {
                    for p in &mut sig.node.params {
                        self.rename_type(&mut p.type_annotation, aliases);
                    }
                    if let Some(ret) = &mut sig.node.return_type {
                        self.rename_type(ret, aliases);
                    }
                }
            }
            DeclarationKind::Impl {
                trait_name,
                type_name,
                methods,
                ..
            } => {
                self.rename_type(trait_name, aliases);
                self.rename_type(type_name, aliases);
                for m in methods {
                    self.rename_decl_name(m, aliases);
                    self.rename_in_decl(m, aliases);
                }
            }
            DeclarationKind::Extension {
                type_constraint,
                methods,
                ..
            } => {
                self.rename_type(type_constraint, aliases);
                for m in methods {
                    self.rename_decl_name(m, aliases);
                    self.rename_in_decl(m, aliases);
                }
            }
            _ => {}
        }
    }

    fn rename_enum_payload(
        &self,
        payload: &mut EnumVariantPayload,
        aliases: &HashMap<String, String>,
    ) {
        match &mut payload.node {
            EnumVariantPayloadKind::Unit(ty) => self.rename_type(ty, aliases),
            EnumVariantPayloadKind::Tuple(types) => {
                for ty in types {
                    self.rename_type(ty, aliases);
                }
            }
            EnumVariantPayloadKind::Struct(fields) => {
                for (_id, ty) in fields {
                    self.rename_type(ty, aliases);
                }
            }
        }
    }

    fn rename_type(&self, ty: &mut Type, aliases: &HashMap<String, String>) {
        if let Some(n) = aliases.get(&ty.node.name) {
            ty.node.name = n.clone();
        }
        for arg in &mut ty.node.generic_args {
            self.rename_type(arg, aliases);
        }
    }

    fn rename_block(&self, block: &mut Block, aliases: &HashMap<String, String>) {
        for stmt in &mut block.statements {
            self.rename_statement(stmt, aliases);
        }
        if let Some(expr) = &mut block.trailing_expression {
            self.rename_expression(expr, aliases);
        }
    }

    fn rename_statement(&self, stmt: &mut Statement, aliases: &HashMap<String, String>) {
        match &mut stmt.node {
            StatementKind::LetBinding {
                type_annotation,
                initializer,
                ..
            } => {
                if let Some(ty) = type_annotation {
                    self.rename_type(ty, aliases);
                }
                self.rename_expression(initializer, aliases);
            }
            StatementKind::Expression(expr) => self.rename_expression(expr, aliases),
            StatementKind::Return(Some(expr)) => self.rename_expression(expr, aliases),
            StatementKind::Return(None) => {}
            StatementKind::Conc { body } => self.rename_block(body, aliases),
            StatementKind::UseDeclaration(_) => {}
            StatementKind::Loop { kind, body } => {
                self.rename_loop_kind(kind, aliases);
                self.rename_block(body, aliases);
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rename_expression(condition, aliases);
                self.rename_block(then_branch, aliases);
                if let Some(else_branch) = else_branch {
                    match &mut else_branch.node {
                        ElseBranchKind::Block(b) => self.rename_block(b, aliases),
                        ElseBranchKind::If(s) => self.rename_statement(s, aliases),
                    }
                }
            }
            StatementKind::Match { expr, arms } => {
                self.rename_expression(expr, aliases);
                for arm in arms {
                    self.rename_pattern(&mut arm.node.pattern, aliases);
                    if let Some(g) = &mut arm.node.guard {
                        self.rename_expression(g, aliases);
                    }
                    self.rename_expression(&mut arm.node.body, aliases);
                }
            }
            StatementKind::Empty => {}
        }
    }

    fn rename_loop_kind(&self, kind: &mut LoopKind, aliases: &HashMap<String, String>) {
        match &mut kind.node {
            LoopKindKind::Block(b) => self.rename_block(b, aliases),
            LoopKindKind::For {
                pattern,
                iterator,
                body,
            } => {
                self.rename_pattern(pattern, aliases);
                self.rename_expression(iterator, aliases);
                self.rename_block(body, aliases);
            }
            LoopKindKind::While { condition, body } => {
                self.rename_expression(condition, aliases);
                self.rename_block(body, aliases);
            }
        }
    }

    fn rename_pattern(&self, pattern: &mut Pattern, aliases: &HashMap<String, String>) {
        match &mut pattern.node {
            PatternKind::Identifier(_id) => {}
            PatternKind::Literal(_) => {}
            PatternKind::Wildcard => {}
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                for p in parts {
                    self.rename_pattern(p, aliases);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for (_id, p) in fields {
                    self.rename_pattern(p, aliases);
                }
            }
            PatternKind::Or(a, b) => {
                self.rename_pattern(a, aliases);
                self.rename_pattern(b, aliases);
            }
            PatternKind::Guard { pattern: p, guard } => {
                self.rename_pattern(p, aliases);
                self.rename_expression(guard, aliases);
            }
            PatternKind::EnumVariant {
                enum_name,
                variant_name,
                payload,
            } => {
                if let Some(n) = aliases.get(&enum_name.name) {
                    enum_name.name = n.clone();
                }
                if let Some(n) = aliases.get(&variant_name.name) {
                    variant_name.name = n.clone();
                }
                if let Some(p) = payload {
                    self.rename_pattern(p, aliases);
                }
            }
        }
    }

    fn rename_expression(&self, expr: &mut Expression, aliases: &HashMap<String, String>) {
        match &mut expr.node {
            ExpressionKind::Identifier(id) => {
                if let Some(n) = aliases.get(&id.name) {
                    id.name = n.clone();
                }
            }
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Array(items),
                ..
            }) => {
                for e in items {
                    self.rename_expression(e, aliases);
                }
            }
            ExpressionKind::Literal(_) => {}
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.rename_expression(left, aliases);
                self.rename_expression(right, aliases);
            }
            ExpressionKind::UnaryOp { expr: inner, .. } => self.rename_expression(inner, aliases),
            ExpressionKind::Call { func, args } => {
                self.rename_expression(func, aliases);
                for a in args {
                    self.rename_expression(a, aliases);
                }
            }
            ExpressionKind::FieldAccess { base, .. } => self.rename_expression(base, aliases),
            ExpressionKind::IndexAccess { base, index } => {
                self.rename_expression(base, aliases);
                self.rename_expression(index, aliases);
            }
            ExpressionKind::StructInit { name, fields } => {
                if let Some(n) = aliases.get(&name.name) {
                    name.name = n.clone();
                }
                for (_id, e) in fields {
                    self.rename_expression(e, aliases);
                }
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(b) = base {
                    self.rename_expression(b, aliases);
                }
                for (_id, e) in fields {
                    self.rename_expression(e, aliases);
                }
            }
            ExpressionKind::Block(b) => self.rename_block(b, aliases),
            ExpressionKind::Pipe { left, right } => {
                self.rename_expression(left, aliases);
                self.rename_expression(right, aliases);
            }
            ExpressionKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.rename_expression(scrutinee, aliases);
                for arm in arms {
                    self.rename_pattern(&mut arm.node.pattern, aliases);
                    if let Some(g) = &mut arm.node.guard {
                        self.rename_expression(g, aliases);
                    }
                    self.rename_expression(&mut arm.node.body, aliases);
                }
            }
            ExpressionKind::TryOperator { expr: inner } => self.rename_expression(inner, aliases),
            ExpressionKind::IfLet {
                pattern,
                expr: matched,
                then,
                else_branch,
            } => {
                self.rename_pattern(pattern, aliases);
                self.rename_expression(matched, aliases);
                self.rename_block(then, aliases);
                if let Some(e) = else_branch {
                    self.rename_expression(e, aliases);
                }
            }
            ExpressionKind::Placeholder(_) => {}
        }
    }

    fn desugar_type(&mut self, _ty: &mut Type) -> Result<(), String> {
        Ok(())
    }

    fn desugar_block(&mut self, block: &mut Block) -> Result<(), String> {
        for stmt in &mut block.statements {
            self.desugar_statement(stmt)?;
        }
        if let Some(expr) = &mut block.trailing_expression {
            self.desugar_expression(expr)?;
        }
        Ok(())
    }

    fn desugar_statement(&mut self, stmt: &mut Statement) -> Result<(), String> {
        match &mut stmt.node {
            StatementKind::LetBinding { initializer, .. } => self.desugar_expression(initializer),
            StatementKind::Expression(expr) => self.desugar_expression(expr),
            StatementKind::Return(Some(expr)) => self.desugar_expression(expr),
            StatementKind::Return(None) => Ok(()),
            StatementKind::Conc { body } => self.desugar_block(body),
            StatementKind::Loop { kind, body } => {
                match &mut kind.node {
                    LoopKindKind::Block(b) => self.desugar_block(b)?,
                    LoopKindKind::For {
                        iterator,
                        body: inner,
                        ..
                    } => {
                        self.desugar_expression(iterator)?;
                        self.desugar_block(inner)?;
                    }
                    LoopKindKind::While {
                        condition,
                        body: inner,
                    } => {
                        self.desugar_expression(condition)?;
                        self.desugar_block(inner)?;
                    }
                }
                self.desugar_block(body)
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.desugar_expression(condition)?;
                self.desugar_block(then_branch)?;
                if let Some(else_branch) = else_branch {
                    match &mut else_branch.node {
                        ElseBranchKind::Block(b) => self.desugar_block(b)?,
                        ElseBranchKind::If(s) => self.desugar_statement(s)?,
                    }
                }
                Ok(())
            }
            StatementKind::Match { expr, arms } => {
                self.desugar_expression(expr)?;
                for arm in arms {
                    if let Some(g) = &mut arm.node.guard {
                        self.desugar_expression(g)?;
                    }
                    self.desugar_expression(&mut arm.node.body)?;
                }
                Ok(())
            }
            StatementKind::Empty | StatementKind::UseDeclaration(_) => Ok(()),
        }
    }

    fn desugar_expression(&mut self, expr: &mut Expression) -> Result<(), String> {
        match &mut expr.node {
            ExpressionKind::BinaryOp {
                op: Operator::Pipe,
                left,
                right,
            } => {
                self.desugar_expression(left)?;
                self.desugar_expression(right)?;
                let call = match &mut right.node {
                    ExpressionKind::Call { func, args } => {
                        let mut new_args = Vec::new();
                        new_args.push(*left.clone());
                        new_args.extend(args.clone());
                        ExpressionKind::Call {
                            func: func.clone(),
                            args: new_args,
                        }
                    }
                    _ => ExpressionKind::Call {
                        func: right.clone(),
                        args: vec![*left.clone()],
                    },
                };
                expr.node = call;
                Ok(())
            }
            ExpressionKind::TryOperator { expr: inner } => {
                self.desugar_expression(inner)?;
                let ok_name = self.next_tmp_name("__ty_ok");
                let err_name = self.next_tmp_name("__ty_err");

                let ok_id = Identifier {
                    name: ok_name.clone(),
                    span: inner.span,
                };
                let err_id = Identifier {
                    name: err_name.clone(),
                    span: inner.span,
                };

                let ok_pat = PatternKind::EnumVariant {
                    enum_name: Identifier {
                        name: "Result".to_string(),
                        span: inner.span,
                    },
                    variant_name: Identifier {
                        name: "Ok".to_string(),
                        span: inner.span,
                    },
                    payload: Some(Box::new(
                        self.mk_pat(PatternKind::Identifier(ok_id.clone()), inner.span),
                    )),
                };
                let err_pat = PatternKind::EnumVariant {
                    enum_name: Identifier {
                        name: "Result".to_string(),
                        span: inner.span,
                    },
                    variant_name: Identifier {
                        name: "Err".to_string(),
                        span: inner.span,
                    },
                    payload: Some(Box::new(
                        self.mk_pat(PatternKind::Identifier(err_id.clone()), inner.span),
                    )),
                };

                let ok_pat = self.mk_pat(ok_pat, inner.span);
                let ok_body = self.mk_expr(ExpressionKind::Identifier(ok_id.clone()), inner.span);
                let ok_arm = self.mk_arm(
                    MatchArmKind {
                        pattern: ok_pat,
                        guard: None,
                        body: ok_body,
                    },
                    inner.span,
                );

                let err_ctor = self.mk_expr(
                    ExpressionKind::Identifier(Identifier {
                        name: "Err".to_string(),
                        span: inner.span,
                    }),
                    inner.span,
                );
                let err_arg = self.mk_expr(ExpressionKind::Identifier(err_id.clone()), inner.span);
                let err_call = self.mk_expr(
                    ExpressionKind::Call {
                        func: Box::new(err_ctor),
                        args: vec![err_arg],
                    },
                    inner.span,
                );

                let err_block = Block {
                    statements: vec![
                        self.mk_stmt(StatementKind::Return(Some(err_call)), inner.span)
                    ],
                    trailing_expression: Some(Box::new(self.mk_expr(
                        ExpressionKind::Placeholder("__ty_unreachable".to_string()),
                        inner.span,
                    ))),
                    span: inner.span,
                    block_id: self.alloc_id(),
                };

                let err_pat = self.mk_pat(err_pat, inner.span);
                let err_body = self.mk_expr(ExpressionKind::Block(err_block), inner.span);
                let err_arm = self.mk_arm(
                    MatchArmKind {
                        pattern: err_pat,
                        guard: None,
                        body: err_body,
                    },
                    inner.span,
                );

                expr.node = ExpressionKind::Match {
                    expr: Box::new(*inner.clone()),
                    arms: vec![ok_arm, err_arm],
                };
                Ok(())
            }
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(s),
                span,
            }) => {
                let segs = split_interpolated(s);
                if segs.len() <= 1 {
                    return Ok(());
                }

                let buf_name = format!("__ty_buf{}", self.next_buf);
                self.next_buf += 1;
                let buf_id = Identifier {
                    name: buf_name.clone(),
                    span: *span,
                };

                let mut statements = Vec::new();
                let buf_new_fn = self.mk_expr(
                    ExpressionKind::Identifier(Identifier {
                        name: "__ty_buf_new".to_string(),
                        span: *span,
                    }),
                    *span,
                );
                let buf_new = self.mk_expr(
                    ExpressionKind::Call {
                        func: Box::new(buf_new_fn),
                        args: Vec::new(),
                    },
                    *span,
                );
                statements.push(self.mk_stmt(
                    StatementKind::LetBinding {
                        mutable: false,
                        name: buf_id.clone(),
                        type_annotation: None,
                        initializer: buf_new,
                    },
                    *span,
                ));

                for seg in segs {
                    match seg {
                        InterpSeg::Lit(text) => {
                            if text.is_empty() {
                                continue;
                            }
                            let push_fn = self.mk_expr(
                                ExpressionKind::Identifier(Identifier {
                                    name: "__ty_buf_push_str".to_string(),
                                    span: *span,
                                }),
                                *span,
                            );
                            let buf_arg =
                                self.mk_expr(ExpressionKind::Identifier(buf_id.clone()), *span);
                            let lit_arg = self.mk_expr(
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Str(text),
                                    span: *span,
                                }),
                                *span,
                            );
                            let call = self.mk_expr(
                                ExpressionKind::Call {
                                    func: Box::new(push_fn),
                                    args: vec![buf_arg, lit_arg],
                                },
                                *span,
                            );
                            statements.push(self.mk_stmt(StatementKind::Expression(call), *span));
                        }
                        InterpSeg::Expr(hole) => {
                            let hole_expr = parse_expr_from_str(&hole, *span)?;
                            let push_fn = self.mk_expr(
                                ExpressionKind::Identifier(Identifier {
                                    name: "__ty_buf_push_str".to_string(),
                                    span: *span,
                                }),
                                *span,
                            );
                            let buf_arg =
                                self.mk_expr(ExpressionKind::Identifier(buf_id.clone()), *span);
                            let call = self.mk_expr(
                                ExpressionKind::Call {
                                    func: Box::new(push_fn),
                                    args: vec![buf_arg, hole_expr],
                                },
                                *span,
                            );
                            statements.push(self.mk_stmt(StatementKind::Expression(call), *span));
                        }
                    }
                }

                let into_fn = self.mk_expr(
                    ExpressionKind::Identifier(Identifier {
                        name: "__ty_buf_into_str".to_string(),
                        span: *span,
                    }),
                    *span,
                );
                let into_arg = self.mk_expr(ExpressionKind::Identifier(buf_id), *span);
                let final_expr = self.mk_expr(
                    ExpressionKind::Call {
                        func: Box::new(into_fn),
                        args: vec![into_arg],
                    },
                    *span,
                );

                expr.node = ExpressionKind::Block(Block {
                    statements,
                    trailing_expression: Some(Box::new(final_expr)),
                    span: *span,
                    block_id: self.alloc_id(),
                });
                Ok(())
            }
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Array(items),
                ..
            }) => {
                for e in items {
                    self.desugar_expression(e)?;
                }
                Ok(())
            }
            ExpressionKind::Literal(_) => Ok(()),
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.desugar_expression(left)?;
                self.desugar_expression(right)
            }
            ExpressionKind::UnaryOp { expr: inner, .. } => self.desugar_expression(inner),
            ExpressionKind::Call { func, args } => {
                self.desugar_expression(func)?;
                for a in args {
                    self.desugar_expression(a)?;
                }
                Ok(())
            }
            ExpressionKind::FieldAccess { base, .. } => self.desugar_expression(base),
            ExpressionKind::IndexAccess { base, index } => {
                self.desugar_expression(base)?;
                self.desugar_expression(index)
            }
            ExpressionKind::StructInit { fields, .. } => {
                for (_id, e) in fields {
                    self.desugar_expression(e)?;
                }
                Ok(())
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(b) = base {
                    self.desugar_expression(b)?;
                }
                for (_id, e) in fields {
                    self.desugar_expression(e)?;
                }
                Ok(())
            }
            ExpressionKind::Block(b) => self.desugar_block(b),
            ExpressionKind::Pipe { left, right } => {
                self.desugar_expression(left)?;
                self.desugar_expression(right)
            }
            ExpressionKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.desugar_expression(scrutinee)?;
                for arm in arms {
                    if let Some(g) = &mut arm.node.guard {
                        self.desugar_expression(g)?;
                    }
                    self.desugar_expression(&mut arm.node.body)?;
                }
                Ok(())
            }
            ExpressionKind::IfLet {
                expr: matched,
                then,
                else_branch,
                ..
            } => {
                self.desugar_expression(matched)?;
                self.desugar_block(then)?;
                if let Some(e) = else_branch {
                    self.desugar_expression(e)?;
                }
                Ok(())
            }
            ExpressionKind::Identifier(_) | ExpressionKind::Placeholder(_) => Ok(()),
        }
    }

    fn next_tmp_name(&mut self, prefix: &str) -> String {
        let name = format!("{}{}", prefix, self.next_tmp);
        self.next_tmp += 1;
        name
    }
}

#[derive(Debug, Clone)]
enum InterpSeg {
    Lit(String),
    Expr(String),
}

fn split_interpolated(s: &str) -> Vec<InterpSeg> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if !cur.is_empty() {
                out.push(InterpSeg::Lit(std::mem::take(&mut cur)));
            }
            let mut hole = String::new();
            while let Some(n) = chars.next() {
                if n == '}' {
                    break;
                }
                hole.push(n);
            }
            out.push(InterpSeg::Expr(hole.trim().to_string()));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(InterpSeg::Lit(cur));
    }
    out
}

fn parse_expr_from_str(src: &str, span: Span) -> Result<Expression, String> {
    let tokens = Lexer::new(src.to_string()).tokenize();
    let mut parser = Parser::new(tokens);
    parser
        .parse_expression_only()
        .map_err(|e| format!("interpolation expr parse error: {}", e))
        .map(|mut e| {
            e.span = span;
            e
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse_module(source: &str) -> Module {
        let tokens = Lexer::new(source.to_string()).tokenize();
        Parser::new(tokens).parse_module().unwrap()
    }

    #[test]
    fn desugars_pipe_into_call() {
        let mut module = parse_module(
            "namespace main\nfn main() -> Int32 { let x: Int32 = 1 |> add(2); return 0; }",
        );
        let mut decl = module.declarations.remove(0);

        let mut d = Desugar::new();
        d.desugar_declaration(&mut decl).unwrap();

        let DeclarationKind::Function { body, .. } = &decl.node else {
            panic!("expected function");
        };
        let StatementKind::LetBinding { initializer, .. } = &body.statements[0].node else {
            panic!("expected let");
        };
        match &initializer.node {
            ExpressionKind::Call { func, args } => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].node, ExpressionKind::Literal(_)));
                assert!(matches!(args[1].node, ExpressionKind::Literal(_)));
                assert!(matches!(func.node, ExpressionKind::Identifier(_)));
            }
            other => panic!("expected call, got {:?}", other),
        }
    }

    #[test]
    fn desugars_try_into_match_with_early_return() {
        let mut module = parse_module(
            "namespace main\nfn foo() -> Result<Int32, Str> { return __ty_result_ok(1); }\nfn main() -> Result<Int32, Str> { let x: Int32 = foo()?; return __ty_result_ok(x); }",
        );
        let mut decl = module.declarations.remove(1);

        let mut d = Desugar::new();
        d.desugar_declaration(&mut decl).unwrap();

        let DeclarationKind::Function { body, .. } = &decl.node else {
            panic!("expected function");
        };
        let StatementKind::LetBinding { initializer, .. } = &body.statements[0].node else {
            panic!("expected let");
        };
        let ExpressionKind::Match { arms, .. } = &initializer.node else {
            panic!("expected match from `?`");
        };
        assert_eq!(arms.len(), 2);

        let err_arm = &arms[1];
        let ExpressionKind::Block(b) = &err_arm.node.body.node else {
            panic!("expected block in Err arm");
        };
        let StatementKind::Return(Some(ret)) = &b.statements[0].node else {
            panic!("expected return in Err arm");
        };
        let ExpressionKind::Call { func, .. } = &ret.node else {
            panic!("expected call in Err return");
        };
        let ExpressionKind::Identifier(id) = &func.node else {
            panic!("expected identifier callee");
        };
        assert_eq!(id.name, "Err");
    }

    #[test]
    fn desugars_string_interpolation_into_buf_builder_block() {
        let mut module = parse_module(
            "namespace main\nfn main(name: Str) -> Int32 { let s: Str = \"Hi {name}!\"; return 0; }",
        );
        let mut decl = module.declarations.remove(0);

        let mut d = Desugar::new();
        d.desugar_declaration(&mut decl).unwrap();

        let DeclarationKind::Function { body, .. } = &decl.node else {
            panic!("expected function");
        };
        let StatementKind::LetBinding { initializer, .. } = &body.statements[0].node else {
            panic!("expected let");
        };
        let ExpressionKind::Block(b) = &initializer.node else {
            panic!("expected block from interpolation");
        };

        let StatementKind::LetBinding {
            initializer: buf_init,
            ..
        } = &b.statements[0].node
        else {
            panic!("expected buf let");
        };
        let ExpressionKind::Call { func, args } = &buf_init.node else {
            panic!("expected call to buf_new");
        };
        assert!(args.is_empty());
        let ExpressionKind::Identifier(id) = &func.node else {
            panic!("expected identifier");
        };
        assert_eq!(id.name, "__ty_buf_new");

        assert!(b.statements.iter().any(|s| {
            matches!(
                &s.node,
                StatementKind::Expression(e)
                    if matches!(
                        &e.node,
                        ExpressionKind::Call { func, .. }
                            if matches!(&func.node, ExpressionKind::Identifier(i) if i.name == "__ty_buf_push_str")
                    )
            )
        }));

        let Some(t) = &b.trailing_expression else {
            panic!("expected trailing expr");
        };
        let ExpressionKind::Call { func, .. } = &t.node else {
            panic!("expected call");
        };
        let ExpressionKind::Identifier(id) = &func.node else {
            panic!("expected id");
        };
        assert_eq!(id.name, "__ty_buf_into_str");
    }
}
