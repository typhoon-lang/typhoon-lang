// src/ast.rs

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
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

#[derive(Debug, Clone, PartialEq)]
pub struct Identifier {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
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
    // Conc block
    Conc { body: Block },
    // Use declaration
    UseDeclaration(UsePath),
    // Loop (e.g. for, while)
    Loop {
        kind: LoopKind,
        body: Block,
    },
    // If statement
    If {
        condition: Expression,
        then_branch: Block,
        else_branch: Option<Box<Statement>>,
    },
    // Match statement
    Match {
        expr: Expression,
        arms: Vec<MatchArm>,
    },
    // Empty statement
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoopKind {
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

#[derive(Debug, Clone, PartialEq)]
pub struct UsePath {
    pub segments: Vec<String>,
    pub wildcard: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Type {
    pub name: String,
    pub generic_args: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: Identifier,
    pub type_annotation: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenericParam {
    pub name: Identifier,
    pub bounds: Vec<GenericBound>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenericBound {
    pub type_name: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expression>,
    pub body: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
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

#[derive(Debug, Clone, PartialEq)]
pub enum Declaration {
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

#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariant {
    pub name: Identifier,
    pub payload: Option<EnumVariantPayload>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnumVariantPayload {
    Unit(Type),
    Tuple(Vec<Type>),
    Struct(Vec<(Identifier, Type)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionSignature {
    pub name: Identifier,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Parameter>,
    pub return_type: Option<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub name: Option<String>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnsafeOrExtern {
    Extern {
        abi: String,
        declarations: Vec<FFIDeclaration>,
    },
    UnsafeBlock(Block),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FFIDeclaration {
    pub fn_name: Identifier,
    pub params: Vec<Type>,
    pub return_type: Type,
}
