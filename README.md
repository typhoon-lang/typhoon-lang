# typhoon-lang

Minimal Typhoon compiler pipeline (lexer → parser → resolver → type checker → liveness → LLVM IR).

**Requirements**
- Rust toolchain
- `clang` in PATH (used to turn `.ll` into a native binary)

**Build**
```bash
cargo build
```

**Compile a Typhoon file**
```bash
cargo run -- path\to\program.ty output.exe
```

This writes `output.ll` and `output.exe`. If `output` is omitted, it defaults to `a.out` (`a.ll` for IR).
