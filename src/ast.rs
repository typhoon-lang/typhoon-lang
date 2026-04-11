// src/ast.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
    pub id: NodeId,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span, id: NodeId) -> Self {
        Spanned { node, span, id }
    }

    pub fn new_dummy(node: T, span: Span) -> Self {
        Spanned {
            node,
            span,
            id: NodeId(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Literal {
    pub kind: LiteralKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiteralKind {
    Int(i64, Option<String>),
    Float(f64, Option<String>),
    Bool(bool),
    Str(String),
    Array(Vec<Expression>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Not,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    Pipe,
    Try,
    Spread,
    ReturnType,
    MatchArm,
    PathSep,
    Borrow,
}

use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

pub type Expression = Spanned<ExpressionKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum ExpressionKind {
    Literal(Literal),
    Identifier(Identifier),
    BinaryOp {
        op: Operator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    UnaryOp {
        op: Operator,
        expr: Box<Expression>,
    },
    Call {
        func: Box<Expression>,
        args: Vec<Expression>,
    },
    FieldAccess {
        base: Box<Expression>,
        field: Identifier,
    },
    IndexAccess {
        base: Box<Expression>,
        index: Box<Expression>,
    },
    StructInit {
        name: Identifier,
        fields: Vec<(Identifier, Expression)>,
    },
    MergeExpression {
        base: Option<Box<Expression>>,
        fields: Vec<(Identifier, Expression)>,
    },
    Block(Block),
    Pipe {
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Match {
        expr: Box<Expression>,
        arms: Vec<MatchArm>,
    },
    TryOperator {
        expr: Box<Expression>,
    },
    IfLet {
        pattern: Box<Pattern>,
        expr: Box<Expression>,
        then: Block,
        else_branch: Option<Box<Expression>>,
    },
    Placeholder(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub trailing_expression: Option<Box<Expression>>,
    pub span: Span,
    pub block_id: NodeId,
}

pub type Statement = Spanned<StatementKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    // Let binding
    LetBinding {
        mutable: bool,
        name: Identifier,
        type_annotation: Option<Type>,
        initializer: Expression,
    },
    // Expression statement (e.g., function call for side effects)
    Expression(Expression),
    // Return statement
    Return(Option<Expression>),
    /// `Conc` block — body is `Block` so the scope has a NodeId.
    Conc {
        body: Block,
    },
    // Use declaration
    UseDeclaration(UsePath),
    /// Loop (`for` / `while` / bare `loop`) — body is `Block`.
    Loop {
        kind: LoopKind,
        body: Block,
    },
    /// `if` / `else` — then-branch is `Block`.
    If {
        condition: Expression,
        then_branch: Block,
        else_branch: Option<ElseBranch>,
    },
    // Match statement
    Match {
        expr: Expression,
        arms: Vec<MatchArm>,
    },
    // Empty statement
    Empty,
}

pub type ElseBranch = Spanned<ElseBranchKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum ElseBranchKind {
    Block(Block),       // else { ... }
    If(Box<Statement>), // else if ... (where Statement is If)
}

pub type LoopKind = Spanned<LoopKindKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum LoopKindKind {
    Block(Block),
    For {
        pattern: Pattern,
        iterator: Expression,
        body: Block,
    },
    While {
        condition: Expression,
        body: Block,
    },
}

pub type UsePath = Spanned<UsePathKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct UsePathKind {
    pub segments: Vec<String>,
    pub wildcard: bool,
}

pub type Type = Spanned<TypeKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct TypeKind {
    pub name: String,
    pub generic_args: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: Identifier,
    pub type_annotation: Type,
    pub span: Span,
}

pub type GenericParam = Spanned<GenericParamKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct GenericParamKind {
    pub name: Identifier,
    pub bounds: Vec<GenericBound>,
}

pub type GenericBound = Spanned<GenericBoundKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct GenericBoundKind {
    pub type_name: Type,
}

pub type MatchArm = Spanned<MatchArmKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArmKind {
    pub pattern: Pattern,
    pub guard: Option<Expression>,
    pub body: Expression,
}

pub type Pattern = Spanned<PatternKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
    Wildcard,
    Identifier(Identifier),
    Literal(Literal),
    EnumVariant {
        enum_name: Identifier,
        variant_name: Identifier,
        payload: Option<Box<Pattern>>,
    },
    Struct {
        struct_name: Identifier,
        fields: Vec<(Identifier, Pattern)>,
        ignore_rest: bool,
    },
    Tuple(Vec<Pattern>),
    Array(Vec<Pattern>),
    Or(Box<Pattern>, Box<Pattern>),
    Guard {
        pattern: Box<Pattern>,
        guard: Box<Expression>,
    },
}

pub type Declaration = Spanned<DeclarationKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum DeclarationKind {
    Function {
        name: Identifier,
        generics: Vec<GenericParam>,
        params: Vec<Parameter>,
        return_type: Option<Type>,
        body: Block,
    },
    Struct {
        name: Identifier,
        generics: Vec<GenericParam>,
        fields: Vec<(Identifier, Type)>,
    },
    Enum {
        name: Identifier,
        generics: Vec<GenericParam>,
        variants: Vec<EnumVariant>,
    },
    Newtype {
        name: Identifier,
        type_alias: Type,
    },
    Interface {
        name: Identifier,
        generics: Vec<GenericParam>,
        methods: Vec<FunctionSignature>,
    },
    Impl {
        trait_name: Type,
        type_name: Type,
        generics: Vec<GenericParam>,
        methods: Vec<Declaration>,
    },
    Extension {
        generics: Vec<GenericParam>,
        type_constraint: Type,
        methods: Vec<Declaration>,
    },
    Use(UsePath),
    UnsafeOrExtern(UnsafeOrExtern),
}

pub type EnumVariant = Spanned<EnumVariantKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariantKind {
    pub name: Identifier,
    pub payload: Option<EnumVariantPayload>,
}

pub type EnumVariantPayload = Spanned<EnumVariantPayloadKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum EnumVariantPayloadKind {
    Unit(Type),
    Tuple(Vec<Type>),
    Struct(Vec<(Identifier, Type)>),
}

pub type FunctionSignature = Spanned<FunctionSignatureKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionSignatureKind {
    pub name: Identifier,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Parameter>,
    pub return_type: Option<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub name: Option<String>,
    pub declarations: Vec<Declaration>,
    pub span: Span,
}

pub type UnsafeOrExtern = Spanned<UnsafeOrExternKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum UnsafeOrExternKind {
    Extern {
        abi: String,
        declarations: Vec<FFIDeclaration>,
    },
    UnsafeBlock(Block),
}

pub type FFIDeclaration = Spanned<FFIDeclarationKind>;

#[derive(Debug, Clone, PartialEq)]
pub struct FFIDeclarationKind {
    pub fn_name: Identifier,
    pub params: Vec<Type>,
    pub return_type: Type,
}
