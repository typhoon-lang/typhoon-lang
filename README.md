# typhoon-lang
![Test](https://github.com/koneko096/typhoon-lang/actions/workflows/test.yml/badge.svg)

A minimal Typhoon compiler pipeline built in Rust. The project exposes a modular internal API, allowing each stage of the compilation process to be tested and utilized independently within the crate.

## Pipeline Architecture
The compiler logic is organized into discrete, public modules. Each stage consumes the output of the previous one to transform Typhoon source code into executable machine code:

1.  **Lexer**: Tokenizes the raw source text.
2.  **Parser**: Constructs an Abstract Syntax Tree (AST).
3.  **Resolver**: Handles name resolution and scoping.
4.  **Type Checker**: Validates static types and ensures type safety.
5.  **Liveness**: Performs data-flow analysis for variable lifetimes.
6.  **Codegen**: Generates LLVM Intermediate Representation for optimization and binary emission.
7.  **Linker**: Link the LLVM IR and internal runtime to generate complete executable binary.

## Requirements
- **Rust Toolchain**: (Stable or Nightly)
- **Clang**: Must be in your `PATH` (used to link `.ll` files into native binaries).

## Getting Started

### Build
To compile the compiler itself:
```bash
cargo build --release
```


### Usage

1. To compile a Typhoon (.ty) source file into a native executable:

```bash
tyc build path/to/program.ty [output_name]
```

_Default Behavior_: If output_name is omitted, it defaults to a.out (and a.ll for the IR).
_Artifacts_: The compiler generates both a human-readable .ll (LLVM IR) file and a functional native binary.

2. To directly run a Typhoon (.ty) single source file:

```bash
tyc run path/to/program.ty
```

### Development
To use the internal compiler functions in your own modules (e.g., for integration testing), ensure you reference them via the crate root:

```rust
use crate::lexer::tokenize;
use crate::parser::parse;
// Stages are exposed via pub(crate) to maintain project encapsulation
```
