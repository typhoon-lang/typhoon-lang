# Typhoon Language Specification
**Version 0.1 — Design Draft**

---

## Table of Contents

1. [Philosophy](#1-philosophy)
2. [Lexical Structure](#2-lexical-structure)
3. [Type System](#3-type-system)
4. [Memory Model](#4-memory-model)
5. [Declarations](#5-declarations)
6. [Statements](#6-statements)
7. [Expressions](#7-expressions)
8. [Pattern Matching](#8-pattern-matching)
9. [Error Handling](#9-error-handling)
10. [Ownership and Liveness](#10-ownership-and-liveness)
11. [Concurrency](#11-concurrency)
12. [Modules](#12-modules)
13. [FFI and Unsafe](#13-ffi-and-unsafe)
14. [Standard Library](#14-standard-library)
15. [Runtime Architecture](#15-runtime-architecture)
16. [Networking Layer](#16-networking-layer)
17. [Implementation Plan](#17-implementation-plan)
18. [Open Questions](#18-open-questions)

---

## 1. Philosophy

Typhoon is a systems language built around four principles working together:

**Correctness by construction.** Errors are values. Null does not exist. Pattern matching is exhaustive. The type system makes illegal states unrepresentable.

**Ownership without ceremony.** Memory is managed through linear types — each value has exactly one owner at a time. There are no garbage collectors, no borrow annotations, and no lifetime parameters in the common case. Region inference handles what Rust requires you to spell out.

**Mutation is explicit and local.** External state changes are expressed through merge expressions that consume and replace bindings. Internal mutation inside a function body is unrestricted. The boundary between the two is enforced by the type system.

**Performance is a first-class concern.** Stack and heap allocation are decided by the compiler, not the programmer. The runtime uses per-task slab allocation with zero global heap contention. Linear types enable LLVM alias analysis across the entire program.

---

## 2. Lexical Structure

### Keywords

```
let  mut  fn  struct  enum  interface  impl  extend  newtype
match  if  else  for  while  return  in  where  conc  select  recv
unsafe  use  true  false  as
```

### Operators

```
+   -   *   /   %                 arithmetic
==  !=  <   >   <=  >=            comparison
&&  ||  !                         logical
&   |   ^   <<  >>                bitwise
=   +=  -=  *=  /=                assignment (mut bindings only)
|>                                pipe
?                                 try / monadic bind
...                               spread (merge expressions only)
->                                return type annotation
=>                                match arm
::                                path separator
&                                 borrow (FFI only)
```

### Literals

```
42           IntLit   (Int32 default)
42i64        IntLit   (explicit Int64)
42u8         IntLit   (explicit Int8, for FFI/layout)
3.14         FloatLit (Float32 default)
3.14f64      FloatLit (explicit Float64)
true  false  BoolLit
"hello"      StrLit   (UTF-8, static — lives in rodata)
"Hi {name}"  StrLit   (interpolated — compiler expands at call site)
[1, 2, 3]   ArrayLit
```

### Comments

```
// single line comment
/* block comment */
/// doc comment — attached to the next declaration
```

### Identifiers

Identifiers begin with a letter or underscore, followed by letters, digits, or underscores. Types and variants use `PascalCase`. Functions, variables, and fields use `snake_case`. Constants use `SCREAMING_SNAKE_CASE`.

---

## 3. Type System

### Primitive Types

| Type     | Size     | Notes                                              |
|----------|----------|----------------------------------------------------|
| `Int8`   | 8-bit    | signed                                             |
| `Int16`  | 16-bit   | signed                                             |
| `Int32`  | 32-bit   | signed, **default integer type**                   |
| `Int64`  | 64-bit   | signed                                             |
| `Float16`| 16-bit   | IEEE 754 half precision                            |
| `Float32`| 32-bit   | IEEE 754 single precision, **default float type**  |
| `Float64`| 64-bit   | IEEE 754 double precision                          |
| `Bool`   | 1-bit    | `true` or `false`                                  |
| `Char`   | 32-bit   | Unicode scalar value                               |
| `Byte`   | 8-bit    | unsigned raw byte, used in IO and FFI              |

**Overflow behavior:** wraps in release builds, traps (runtime panic, delegated to Rust's panic runtime) in debug builds. Explicit methods `wrapping_add`, `saturating_add`, `checked_add` are available for precise control.

**Numeric coercion rules:**

- **Implicit widening is allowed.** A narrower integer or float type is automatically promoted to a wider type when the context requires it. This is always safe — no information is lost.
  ```typhoon
  let a: Int8  = 10
  let b: Int32 = 1000
  let c = a + b   // a implicitly widened to Int32; c is Int32
  ```
- **Implicit narrowing is a compile error.** Assigning a wider type to a narrower binding requires an explicit `as` cast. The compiler emits a warning at every narrowing cast reminding the programmer that truncation may occur.
  ```typhoon
  let big: Int32 = 1000
  let small: Int8 = big          // compile error: narrowing requires explicit cast
  let small: Int8 = big as Int8  // ok — compiler warning: possible truncation
  ```
- **Float ↔ Int conversions always require explicit `as`.** There is no implicit float-to-integer or integer-to-float promotion.
  ```typhoon
  let f: Float32 = 3.14
  let i: Int32   = f as Int32   // ok — truncates toward zero; compiler warning
  ```
- **Coercion hierarchy (widening direction):**
  ```
  Int8 → Int16 → Int32 → Int64
  Float16 → Float32 → Float64
  ```
  Cross-hierarchy (Int → Float or Float → Int) always requires `as`.

### The `Str` Type

`Str` is a **fat pointer** — a `(ptr: *const Byte, len: Int32)` pair. It does not own memory and never allocates. It is always a view into bytes that live somewhere else:

- **Static** — pointing into the binary's read-only segment (string literals).
- **Stack** — pointing into the current stack frame (local byte arrays cast to `Str`).

`Str` is immutable. There is no null terminator. Embedded null bytes are valid. String interpolation in literals (`"Hello, {name}"`) is expanded by the compiler at the call site into a `Buf` allocation when the result must be owned, or a stack-allocated `Str` when the lifetime is provably local.

`Buf` is the owned, growable string type used when text must be built dynamically:

```
Buf  =  { ptr: *mut Byte, len: Int32, cap: Int32 }  — heap allocated
```

### Collection Types

| Syntax     | Full Name      | Allocation | Notes                            |
|------------|----------------|------------|----------------------------------|
| `[T]`      | Array of T     | see §4     | grows with `push`, `prepend`     |
| `Map<K,V>` | Hash map       | heap       | open-addressing, power-of-2 size |
| `Set<T>`   | Hash set       | heap       | implemented as `Map<T, ()>`      |

### Generic Types (Built-in)

```
Option<T>      = Some(T) | None
Result<T, E>   = Ok(T) | Err(E)
ref T         = shared, reference-counted heap pointer
chan<T>        = typed channel for inter-task communication
Tuple<A, B>    = (A, B)        two-element
Tuple<A, B, C> = (A, B, C)    three-element
```

### Compound Types

```typhoon
// Struct — product type
struct User {
  name: Str,
  age:  Int32,
  tags: [Str],
}

// Enum — sum type (algebraic data type)
enum Shape {
  Circle   { radius: Float32 },
  Rect     { width: Float32, height: Float32 },
  Triangle { base: Float32, height: Float32 },
}

// Newtype — zero-cost type wrapper
newtype UserId    = Int32
newtype ProductId = Int32
// UserId(42) and ProductId(42) are distinct types — cannot be mixed
```

### Interfaces

```typhoon
interface Display {
  fn show(&self) -> Str
}

interface Ord {
  fn cmp(&self, other: &Self) -> Int32
}

// Multiple bounds use +
fn print_sorted<T: Display + Ord>(items: [T]) { ... }
```

### Type Inference

typhoon uses bidirectional Hindley-Milner inference. Type annotations are optional in most positions:

```typhoon
let xs  = [1, 2, 3]                     // inferred: [Int32]
let m   = Map::from([("a", 1)])          // inferred: Map<Str, Int32>
let res = parse("21").map(|n| n * 2)    // inferred: Result<Int32, Str>
```

Annotate when inference is ambiguous or when you want to document intent:

```typhoon
let big: [Float32] = []
let id: UserId = UserId(42)
```

---

## 4. Memory Model

### Allocation Strategy

The compiler decides allocation based on two rules. The programmer does not choose.

**Rule 1 — mutability.**
A `let` binding is immutable. A `let mut` binding is mutable.

**Rule 2 — size stability.**
A type is *size-stable* if its size is fully known at compile time and cannot change (all primitive types, structs containing only size-stable fields, fixed-length arrays).
A type is *size-dynamic* if it can grow at runtime (growable arrays `[T]` with `push`/`prepend`, `Buf`, `Map`, `Set`).

| Binding   | Type           | Allocation |
|-----------|----------------|------------|
| `let`     | size-stable    | **stack**  |
| `let`     | size-dynamic   | **heap**   |
| `let mut` | size-stable    | **heap**   |
| `let mut` | size-dynamic   | **heap**   |

The programmer never writes `new`, `Box`, `alloc`, or `free`. Deallocation is inserted by the compiler at the point a linear binding is consumed (see §10).

```typhoon
let point  = Point { x: 1.0, y: 2.0 }  // stack — immutable, size-stable
let mut p  = Point { x: 1.0, y: 2.0 }  // heap  — mutable
let nums   = [1, 2, 3]                  // stack — immutable, size-stable (fixed at construction)
let mut v  = [1, 2, 3]                  // heap  — mutable (can push)
v.push(4)                               // legal — v is mut
// nums.push(4)                         // compile error — nums is not mut
```

### The Slab Allocator

Each task owns a **private slab** — a contiguous virtual memory region reserved at task spawn time (default 4 MB, configurable). Allocation within a slab uses a bump pointer: incrementing a counter is the entire allocation cost.

Within a slab, a **size-class free list** handles mid-task deallocation from linear consumption:

- 8 size classes: 8, 16, 32, 64, 128, 256, 512 bytes, and one large-object class.
- Freed objects are returned to their size class and immediately reusable.
- Objects larger than 512 bytes are tracked individually.

At task end, the entire slab is reclaimed in a single operation — resetting the bump pointer to zero. No per-object free is required for task-scoped data.

There is **no global heap contention**. Tasks never share slab memory except through channel transfer or `ref T`.

### `ref T` — Shared Ownership

When data genuinely needs to be shared across tasks, wrap it in `ref T`:

```typhoon
let config = ref(Config::load())  // allocate a shared reference
let c2     = config.clone()       // cheap — increments reference count only
conc { worker(c2) }               // c2 moved into task; config still valid here
```

`ref T` allocates on the global allocator (not the slab) and is freed when its reference count reaches zero. The content of a `ref T` is always **immutable**. For shared mutable state, use `ref Mutex<T>` — mutability is explicit at the type level.

Use `ref T` sparingly — it is the only mechanism in Typhoon that escapes task-local memory.

---

## 5. Declarations

### Function Declaration

```typhoon
fn name<GenericParams>(params) -> ReturnType {
  body
}
```

```typhoon
// Simple function
fn add(a: Int32, b: Int32) -> Int32 {
  a + b
}

// Generic with interface bound
fn largest<T: Ord>(a: T, b: T) -> T {
  if a.cmp(b) >= 0 { a } else { b }
}
```

Parameters are always immutable copies from the caller's perspective. There are no mutable parameters and no output parameters.

The last expression in a function body is the implicit return value. `return` is available for early exit.

### Struct Declaration

```typhoon
/// A registered user account.
struct User {
  id:         UserId,
  name:       Str,
  email:      Str,
  age:        Int32,
  tags:       [Str],
  created_at: Timestamp,
}
```

Fields are ordered by the programmer. The compiler may reorder for alignment in the binary representation unless `@repr(C)` is specified.

### Enum Declaration

```typhoon
enum AppError {
  NotFound(Str),
  Unauthorized,
  Validation(Str),
  Io(IoError),
}

enum Command {
  Quit,
  Move { x: Int32, y: Int32 },
  Write(Str),
}
```

Every variant is a distinct type at the machine level. The enum's in-memory representation is a tagged union.

### Interface Declaration

```typhoon
interface Display {
  fn show(&self) -> Str
}

interface From<T> {
  fn from(value: T) -> Self
}

interface Hash {
  fn hash(&self) -> Int64
}
```

### Implementation

```typhoon
impl Display for User {
  fn show(&self) -> Str {
    "{self.name} (age {self.age})"
  }
}

impl From<IoError> for AppError {
  fn from(e: IoError) -> AppError {
    AppError::Io(e)
  }
}
```

### Extension Methods

Add methods to any type, including built-in types. Constrained by interface bounds.

```typhoon
extend [T] where T: Ord {
  fn median(&self) -> Option<&T> {
    if self.is_empty() { return None }
    let sorted = self.sorted()
    Some(&sorted[sorted.len() / 2])
  }
}
```

### Newtype Declaration

```typhoon
newtype UserId    = Int32
newtype ProductId = Int32
newtype Email     = Str

// Construction requires explicit wrapping
let uid: UserId = UserId(42)

// The inner value is accessed via .0
let raw: Int32 = uid.0

// Newtypes do not implicitly coerce
fn get_user(id: UserId) -> Option<User> { ... }
// get_user(ProductId(42))  ← compile error: type mismatch
```

### Use Declaration

```typhoon
use std::collections::Map
use std::io::{read_file, write_file}
use http::{Server, Request, Response}
```

---

## 6. Statements

### Let Binding

```typhoon
let name: Type = expr   // immutable, type annotation optional
let mut name = expr     // mutable — heap allocated, liveness not tracked
```

`let` bindings are **linear** — consumed on first use (move or function call). The compiler tracks liveness. Using a consumed binding is a compile error.

`let mut` bindings are **local** — they may be reassigned freely within their scope. They are never moved; they die at scope end. Their mutation never escapes the function.

### Assignment

Only valid on `let mut` bindings:

```typhoon
let mut count = 0
count = count + 1
count += 1
```

Assigning to a `let` binding is a compile error.

### For Loop

```typhoon
for item in collection {
  // item is a copy of each element
  // collection is consumed by the loop
}
```

Iteration is defined by the `Iter` interface. Built-in collections implement it.

### While Loop

```typhoon
while condition {
  body
}
```

### Return

```typhoon
return expr    // early return
// or
expr           // last expression is implicit return
```

### Expression Statement

Any expression can be used as a statement for its side effect:

```typhoon
println("Hello")
ch.send(result)
list.push(item)
```

---

## 7. Expressions

### Block

A block `{ stmt; ...; expr }` is an expression. Its value is the last expression. Blocks create a new scope.

```typhoon
let result = {
  let x = compute()
  let y = x * 2
  y + 1    // value of the block
}
```

### If Expression

Both branches must return the same type.

```typhoon
let label = if score >= 90 { "A" } else { "B" }

// Multi-line
let msg = if user.age >= 18 {
  "Welcome"
} else {
  "Access denied"
}
```

### Match Expression

Exhaustive by default. The compiler rejects patterns that don't cover all cases.

```typhoon
match shape {
  Shape::Circle   { radius }        => 3.14159 * radius * radius,
  Shape::Rect     { width, height } => width * height,
  Shape::Triangle { base, height }  => 0.5 * base * height,
}

// With guards
match result {
  Ok(n) if n > 0 => "positive",
  Ok(_)          => "non-positive",
  Err(e)         => "error: {e}",
}
```

### If-Let

Single-branch pattern match. Sugar for a `match` with one arm and a discard arm.

```typhoon
if let Some(admin) = find_admin(&users) {
  println("Admin: {admin.name}")
}
```

### Closure

```typhoon
let double = |x: Int32| x * 2
let greet  = |name: Str| "Hello, {name}"
let add    = |a, b| a + b       // types inferred from context
```

Closures capture bindings by **copy** (shallow). Captured bindings from the enclosing scope are consumed at the point the closure is created. Moving the closure into a `conc` block moves the captures with it.

### Pipe Expression

`|>` passes the left-hand value as the first argument to the right-hand call:

```typhoon
let result = [1, 2, 3, 4, 5]
  |> filter(|x| x % 2 == 0)
  |> map(|x| x * 10)
  |> sum()

// Equivalent to:
let result = sum(map(filter([1,2,3,4,5], |x| x%2==0), |x| x*10))
```

Pipe is purely syntactic. It is eliminated during AST lowering before type checking.

### Merge Expression

The only mechanism for producing a modified copy of a struct or array from outside a function. The source binding is **consumed** — it becomes invalid after the merge.

```typhoon
// Struct merge
let user = User { name: "Alice", age: 30, tags: [] }
let user = { ...user, age: 31 }       // user (old) consumed; user (new) is live
// println(user.name)  ← still valid — refers to the new user

// Nested merge
let company = { ...company, ceo: { ...company.ceo, age: 46 } }

// Array spread in merge
let cart = { ...cart, items: [...cart.items, new_item] }
```

Merge is not a general mutation mechanism. It produces a new value from an old one. The compiler optimizes this to in-place mutation when it can prove single ownership (which it usually can, because the source is consumed).

### Try Expression (`?`)

Unwraps `Ok(v)` / `Some(v)` and returns `v`. On `Err(e)` / `None`, short-circuits the current function with the error, applying `From` conversion if needed.

```typhoon
fn load(path: Str) -> Result<Config, AppError> {
  let text = read_file(path)?           // IoError auto-converted via From
  let data = parse_toml(text)?
  Ok(decode_config(data))
}
```

### Join Expression

Await multiple tasks concurrently. All tasks run simultaneously; the expression resolves when all complete.

```typhoon
let (user, orders) = join!(
  fetch_user(id),
  fetch_orders(id)
)
```

### Field Access

```typhoon
user.name
order.amount
company.ceo.age
```

### Index Expression

Array indexing returns `Option<T>` — out-of-bounds is not a panic, it is a `None`.

```typhoon
let first = items[0]         // Option<T>
let val   = items[i]?        // unwrap or propagate
```

### Struct Initialization

```typhoon
let user = User {
  id:    UserId(1),
  name:  "Alice",
  email: "alice@example.com",
  age:   30,
  tags:  ["admin", "beta"],
  created_at: now(),
}
```

---

## 8. Pattern Matching

Patterns appear in `match`, `let`, `for`, and `if let`.

### Pattern Types

```typhoon
_                          // wildcard — match anything, bind nothing
x                          // bind value to name x
42, "hello", true          // literal — match exact value
(x, y)                     // tuple destructure
User { name, age, .. }     // struct destructure; .. ignores remaining fields
Ok(v), Err(e)              // enum variant + payload binding
Some(n), None              // Option variants
[first, ..rest]            // array head/tail destructure
[a, b, c]                  // fixed-length array destructure
Ok(n) if n > 0             // guard — pattern with additional condition
None | Err(_)              // or-pattern — match either
```

### Exhaustiveness

The compiler rejects non-exhaustive matches at compile time. Adding `_` as a final arm is the explicit "handle all remaining cases" escape hatch.

```typhoon
match cmd {
  Command::Quit         => break,
  Command::Move { x, y }=> move_to(x, y),
  Command::Write(text)  => print(text),
  // No _ needed — all variants covered
}
```

---

## 9. Error Handling

### `Result<T, E>`

The return type of any fallible operation. Either `Ok(value)` or `Err(error)`.

```typhoon
fn parse_age(s: Str) -> Result<Int32, Str> {
  match s.to_int() {
    Some(n) if n > 0 => Ok(n),
    Some(_)          => Err("age must be positive"),
    None             => Err("not a number"),
  }
}
```

### `Option<T>`

Replaces null entirely. Either `Some(value)` or `None`.

```typhoon
fn find_user(id: UserId) -> Option<User> {
  db.users.get(id)
}
```

### Monad Operations

Both `Result` and `Option` support:

```typhoon
.map(fn)               // transform the Ok/Some value; pass Err/None through
.map_err(fn)           // transform the Err value; pass Ok through
.and_then(fn)          // chain — fn returns Result/Option (flatMap)
.unwrap_or(default)    // extract value or use fallback
.unwrap_or_else(fn)    // extract value or compute fallback lazily
.ok_or(err)            // Option<T> → Result<T, E>
.ok()                  // Result<T, E> → Option<T>
```

```typhoon
let name = find_user(UserId(42))
  .map(|u| u.name)
  .unwrap_or("anonymous")

let verified = parse_age("21")
  .and_then(|age| verify_adult(age))
  .map_err(|e| AppError::Validation(e))
```

### The `?` Operator

Monadic bind. Unwraps success or short-circuits with the error. Applies `From` conversion on the error type automatically.

```typhoon
fn process(id: Int32) -> Result<Receipt, AppError> {
  let user   = find_user(UserId(id)).ok_or(AppError::NotFound("user"))?
  let order  = db.orders.get(id)?
  let charge = payment::charge(user.card_id, order.amount)?
  Ok(Receipt { order_id: id, amount: order.amount })
}
```

### Typed Error Enums

```typhoon
enum AppError {
  NotFound(Str),
  Unauthorized,
  Validation(Str),
  Io(IoError),
  Database(DbError),
}

impl From<IoError> for AppError {
  fn from(e: IoError) -> AppError { AppError::Io(e) }
}

impl From<DbError> for AppError {
  fn from(e: DbError) -> AppError { AppError::Database(e) }
}
```

---

## 10. Ownership and Liveness

### Linear Types

Every `let` binding is **linear**: it must be used exactly once. The compiler tracks a *live set* of bindings per scope. A binding is removed from the live set when it is consumed. Using a consumed binding is a compile error.

```typhoon
let a = User { name: "Alice", age: 30, tags: [] }
let b = a                    // a is consumed — moved into b
// println(a.name)           ← compile error: a was moved

let c = b.clone()            // explicit deep copy — b still live
println(b.name)              // ok
```

### Consumption Points

A binding is consumed at:

- Assignment to another `let` binding: `let b = a`
- Function call: `greet(a)` — a shallow copy is passed; the original is consumed
- Merge source: `{ ...a, age: 31 }` — a is consumed
- `conc` block capture: the binding is consumed in the enclosing scope
- Channel send: `ch.send(a)` — ownership transfers into the channel

### Shallow Copy Semantics

Function arguments receive a **shallow copy** of the value. For size-stable types (structs of primitives, fixed arrays), this is a stack copy and is trivially cheap. For heap-containing types (growable arrays, `Buf`, `Map`), the copy copies the fat pointer — the underlying heap data is not duplicated.

This is safe because the original binding is simultaneously consumed. There is never a moment where two live bindings point to the same heap data. Aliasing is impossible by construction.

To intentionally duplicate heap data, call `.clone()`. This performs a full deep copy and is always explicit.

### Conditional and Loop Consumption

A binding consumed in one branch must be consumed in all branches:

```typhoon
let x = make_value()

// This is a compile error — x is consumed in the if branch but not the else
if cond {
  use_value(x)    // x consumed here
} else {
  // x not consumed — liveness mismatch
}

// Correct: consume in both branches
if cond {
  use_value(x)
} else {
  drop(x)         // explicit discard
}
```

In a `for` loop, the iteration variable is consumed each iteration. The collection itself is consumed by the loop.

### `let mut` Bindings and Escape

`let mut` bindings live and die with their scope. They are never moved, never passed to the liveness checker, and are freed at scope exit. They exist purely for local computation:

```typhoon
fn sum(nums: [Int32]) -> Int32 {
  let mut total = 0
  for n in nums { total += n }
  total    // pure Int32 returned — no mut escapes
}
```

The function's signature `([Int32]) -> Int32` is purely functional from the outside. The internal `mut` is an implementation detail.

### `ref T` and the Escape Hatch

When aliasing is genuinely required (shared configuration, read-only reference data), use `ref T`. Cloning an `ref T` is cheap (atomic reference count increment). The data is freed when the last `ref` is dropped.

---

## 11. Concurrency

### `conc` Blocks

`conc { }` spawns a lightweight coroutine. Bindings closed over are moved into the coroutine — they are consumed in the enclosing scope at the `conc` statement.

```typhoon
let data = fetch_data()

conc {
  // data is moved here — consumed in parent
  let result = process(data)
  results_ch.send(result)
}

// println(data)  ← compile error: data was moved into conc block
```

**The Isolation Guard Mechanism**

The core safety rule of Typhoon is that `mut` is a local-only permission that cannot cross the "Isolation Boundary" of a `conc` block.

*   **Capture Analysis:** When the compiler encounters a `conc` statement, it inspects the bindings being moved into the block.
*   **The Restriction:** Any variable marked as `mut` in the parent scope is forbidden from appearing in these move captures.
*   **The Workflow:**
    *   A user can mutate a local variable (e.g., building a list).
    *   To send it to a concurrent task, they must "freeze" it by re-binding it as an immutable linear type: `let frozen_data = my_mut_data`.
    *   Only `frozen_data` can be moved into the `conc` block.

Coroutines are **stackful** — each has its own stack segment (initial size 64 KB, grows on demand via guard page). They are multiplexed onto a fixed pool of OS threads by the M:N scheduler (one thread per hardware core by default).

Scheduling is cooperative at I/O operations (blocked `chan.recv()`, blocked `chan.send()`), and preemptive for CPU-bound coroutines that never yield.

### Channels

`chan<T>` is a typed, bounded channel. Ownership of a value transfers through the channel — sending moves the value in, receiving moves it out.

```typhoon
let ch = chan<Int32>()           // unbuffered (capacity 0)
let ch = chan<Int32>(16)         // buffered (capacity 16)

// Send — blocks if channel is full
ch.send(value)

// Receive — blocks until a value is available
let value = ch.recv()

// Non-blocking variants return Option
let maybe = ch.try_send(value)  // Option<()>
let maybe = ch.try_recv()       // Option<T>
```

`chan<T>` is itself a linear resource — it is consumed when moved into a `conc` block. The type system prevents a closed channel from being used.

### Select

Wait on multiple channels simultaneously. The first ready operation wins.

```typhoon
select {
  recv(ch_a) |> |val| handle_a(val),
  recv(ch_b) |> |val| handle_b(val),
  default    => {}    // non-blocking: proceed immediately if nothing ready
}
```

`select` without `default` blocks until at least one channel is ready.

### A Complete Concurrency Example

```typhoon
fn process_orders(orders: [Order]) -> [Result<Receipt, AppError>] {
  let ch = chan<Result<Receipt, AppError>>(orders.len())

  for order in orders {
    let ch_ref = ch.clone()    // clone the channel handle — not the buffer
    conc {
      ch_ref.send(process_single(order))
    }
  }

  // Collect results in order
  orders.map(|_| ch.recv())
}

conc process_orders([{ID: "orderID"}])?
```

### Data Race Safety

Data races are **impossible by construction**:

- `conc` blocks capture by move — no binding is simultaneously live in two coroutines.
- Channels transfer ownership — the sender loses access when the value is sent.
- `ref T` content is immutable (mutation requires `Mutex<T>` wrapping, which is a separate type).
- The liveness checker enforces all of the above at compile time.

---

## 12. Modules

### Namespace-Based, Not Path-Based

Typhoon uses **explicit namespace declarations**, not file-path-derived module names. A namespace is declared at the top of any `.ty` file. Multiple files may contribute to the same namespace. One file may declare only one namespace. The compiler collects all files with the same namespace declaration and treats them as a single logical unit.

This is the C++/C# model: the file system is a build concern, not a language concern.

```typhoon
// file: src/models/user.ty
namespace myapp::models

pub struct User { ... }
pub struct UserId = Int32
```

```typhoon
// file: src/models/order.ty — different file, same namespace
namespace myapp::models

pub struct Order { ... }
pub struct Receipt { ... }
```

```typhoon
// file: src/services/auth.ty
namespace myapp::services::auth

use myapp::models::{User, UserId}

pub fn verify(id: UserId) -> Result<User, AppError> { ... }
```

Both `User` and `Order` are accessible as `myapp::models::User` and `myapp::models::Order` — the compiler merged the two files into one namespace at build time.

### Namespace Rules

- A namespace declaration must be the first non-comment line in the file.
- Namespace names use `::` as separator. Segments are `snake_case` by convention.
- A namespace may have any depth: `myapp`, `myapp::core`, `myapp::http::routing`.
- Circular namespace dependencies are a compile error. The dependency graph must be a DAG.
- The root namespace for the standard library is `std`. Third-party packages declare their own top-level namespace.

### Visibility

Declarations are **private by default** — visible only within the same namespace. Mark public with `pub`:

```typhoon
namespace myapp::models

pub struct User {        // visible to any namespace that imports myapp::models
  pub name: Str,         // pub on fields individually
  age:      Int32,       // private — only accessible within myapp::models
}

pub fn create(name: Str, age: Int32) -> User { ... }  // public
fn validate_age(age: Int32) -> Bool { ... }           // private
```

### Imports

```typhoon
use std::io::{read_file, write_file}
use std::collections::{Map, Set}
use myapp::models::{User, Order}
use myapp::services::auth

// Import everything from a namespace (use sparingly)
use myapp::models::*
```

Imported names are available unqualified within the file. Without a `use`, the full path is always valid:

```typhoon
let user = myapp::models::User { name: "Alice", age: 30, tags: [] }
```

### Entry Point

The program entry point is the `main` function in the `main` namespace. There must be exactly one `main` namespace per binary target.

```typhoon
namespace main

use std::io::println
use myapp::services::server

fn main(net: Network) -> Result<(), AppError> {
  server::run(net)
}
```

### Project Manifest (`typhoon.toml`)

```toml
[project]
name    = "my-app"
version = "0.1.0"

[[bin]]
name      = "my-app"
namespace = "main"       // which namespace contains the entry point

[dependencies]
http = "1.2.0"
json = "0.8.0"
```

---

## 13. FFI and Unsafe

Typhoon uses Rust's FFI model verbatim.

### Declaring C Functions

```typhoon
@extern("C")
fn malloc(size: Int64) -> Ptr<Byte>

@extern("C")
fn free(ptr: Ptr<Byte>)

@extern("C")
fn memcpy(dst: Ptr<Byte>, src: Ptr<Byte>, n: Int64) -> Ptr<Byte>
```

### Raw Pointers

`Ptr<T>` is an unsafe raw pointer. The liveness checker does not track `Ptr<T>` values. Any function that takes or returns `Ptr<T>` is implicitly unsafe.

```typhoon
let p: Ptr<Byte> = unsafe { malloc(1024) }
unsafe { free(p) }
```

### Unsafe Blocks

`unsafe { }` opts out of all compiler safety guarantees within the block. It is the programmer's assertion that the contained code is correct by reasoning the compiler cannot perform.

```typhoon
unsafe {
  let raw = malloc(size_of::<User>())
  let user_ptr = raw as Ptr<User>
  ptr::write(user_ptr, User { ... })
}
```

### C-Compatible Layout

```typhoon
@repr(C)
struct CPoint {
  x: Float32,
  y: Float32,
}
```

Without `@repr(C)`, the compiler may reorder fields for alignment. `@repr(C)` preserves declaration order.

### Safety Convention

Any function containing `unsafe` or `Ptr<T>` in its body or signature must be documented with the invariants the caller must uphold. The standard library marks all such functions explicitly in their doc comments.

---

## 14. Standard Library

### Tier 1 — Core

Available in every module without import.

**`Str`**
```typhoon
.len() -> Int32
.is_empty() -> Bool
.contains(sub: Str) -> Bool
.starts_with(prefix: Str) -> Bool
.ends_with(suffix: Str) -> Bool
.split(sep: Str) -> [Str]
.trim() -> Str
.to_upper() -> Buf
.to_lower() -> Buf
.to_int() -> Option<Int32>
.to_float() -> Option<Float32>
.bytes() -> [Byte]
```

**`Buf`** (owned, growable string)
```typhoon
Buf::new() -> Buf
Buf::from(s: Str) -> Buf
.push(s: Str)
.as_str() -> Str
.len() -> Int32
```

**`[T]`** (array)
```typhoon
// Construction
[T]::new() -> [T]

// Access
.len() -> Int32
.is_empty() -> Bool
[i]  -> Option<T>

// Positional mutation (return new array, consume self)
.push(val: T) -> [T]
.prepend(val: T) -> [T]
.set_at(i: Int32, val: T) -> [T]
.map_at(i: Int32, fn: T -> T) -> [T]
.remove_at(i: Int32) -> [T]
.insert_at(i: Int32, val: T) -> [T]
.swap(i: Int32, j: Int32) -> [T]

// Search-based mutation
.update_first(pred: T -> Bool, fn: T -> T) -> [T]
.update_where(pred: T -> Bool, fn: T -> T) -> [T]
.remove_first(pred: T -> Bool) -> [T]
.remove_where(pred: T -> Bool) -> [T]

// Combinators
.map(fn: T -> U) -> [U]
.filter(fn: T -> Bool) -> [T]
.find(fn: T -> Bool) -> Option<T>
.any(fn: T -> Bool) -> Bool
.all(fn: T -> Bool) -> Bool
.count(fn: T -> Bool) -> Int32
.fold(init: U, fn: (U, T) -> U) -> U
.flat_map(fn: T -> [U]) -> [U]
.zip(other: [U]) -> [(T, U)]
.sorted() -> [T]   // requires T: Ord
.sorted_by(fn: (T, T) -> Int32) -> [T]
.reversed() -> [T]
.first() -> Option<T>
.last() -> Option<T>
.take(n: Int32) -> [T]
.drop(n: Int32) -> [T]
.chunks(n: Int32) -> [[T]]
.flatten() -> [U]  // T must be [U]
.dedup() -> [T]    // requires T: Eq
.clone() -> [T]    // deep copy
```

**`Map<K, V>`**
```typhoon
Map::new() -> Map<K, V>
Map::from(pairs: [(K, V)]) -> Map<K, V>
.get(key: K) -> Option<V>
.set(key: K, val: V) -> Map<K, V>
.remove(key: K) -> Map<K, V>
.update(key: K, fn: V -> V) -> Result<Map<K, V>, Str>
.contains(key: K) -> Bool
.keys() -> [K]
.values() -> [V]
.entries() -> [(K, V)]
.len() -> Int32
.is_empty() -> Bool
.merge(other: Map<K, V>) -> Map<K, V>           // right wins on conflict
.merge_with(other: Map<K, V>, fn: (V, V) -> V) -> Map<K, V>
.filter(fn: (K, V) -> Bool) -> Map<K, V>
.map_values(fn: V -> U) -> Map<K, U>
.clone() -> Map<K, V>
```

**`Option<T>`**
```typhoon
.map(fn: T -> U) -> Option<U>
.and_then(fn: T -> Option<U>) -> Option<U>
.unwrap_or(default: T) -> T
.unwrap_or_else(fn: () -> T) -> T
.ok_or(err: E) -> Result<T, E>
.is_some() -> Bool
.is_none() -> Bool
.filter(fn: T -> Bool) -> Option<T>
```

**`Result<T, E>`**
```typhoon
.map(fn: T -> U) -> Result<U, E>
.map_err(fn: E -> F) -> Result<T, F>
.and_then(fn: T -> Result<U, E>) -> Result<U, E>
.unwrap_or(default: T) -> T
.unwrap_or_else(fn: E -> T) -> T
.ok() -> Option<T>
.err() -> Option<E>
.is_ok() -> Bool
.is_err() -> Bool
```

### Tier 2 — Standard

Available via `use std::*`.

```typhoon
use std::io        // read_file, write_file, println, eprintln, scan
use std::math      // abs, sqrt, pow, floor, ceil, round, min, max, PI, E
use std::time      // Timestamp, Duration, now(), sleep()
use std::fmt       // format!() macro for complex string building
use std::process   // exit(), env(), args()
use std::fs        // File, Dir, path operations
```

### Tier 3 — Ecosystem

Separate packages, included via `typhoon.toml`.

```typhoon
use json           // parse, serialize, zero-copy via StrView internally
use http           // Server, Client, Request, Response, Router
use test           // assert!, assert_eq!, test runner, bench!
```

### The `@derive` Macro

Auto-generates implementations for mechanical interface patterns:

```typhoon
@derive(Display, Eq, Ord, Hash, Clone, Patch)
struct Config {
  host:    Str,
  port:    Int32,
  timeout: Int32,
  debug:   Bool,
}

// @derive(Patch) generates:
struct ConfigPatch {
  host:    Option<Str>,
  port:    Option<Int32>,
  timeout: Option<Int32>,
  debug:   Option<Bool>,
}
// And: config.apply_patch(patch: ConfigPatch) -> Config
```

---

## 15. Runtime Architecture

### Coroutine Scheduler (M:N)

```
Hardware cores (N)
    ↕
OS Threads — one per core, pinned via thread affinity
    ↕
Work-stealing run queues — per-thread deque of ready coroutines
    ↕
Coroutines — stackful, 64 KB initial stack, grows via guard page fault
```

Each `conc {}` creates a coroutine entry in the run queue of the spawning thread. Work stealing balances load — idle threads steal from busy threads' queues.

Scheduling is:
- **Cooperative** at `await`, `chan.recv()` (blocked), `chan.send()` (full channel)
- **Preemptive** via POSIX signal (`SIGPROF`) for CPU-bound coroutines exceeding a time quantum (default 10ms)

### Per-Task Slab

```
Virtual address space per task:
  [slab_start ... bump_ptr ... slab_end]
  
  bump_ptr starts at slab_start
  Allocation = bump_ptr += align_up(size); return old bump_ptr
  Linear free = return to size-class free list
  Task death = bump_ptr = slab_start (entire slab reclaimed in one op)
```

Default slab size: 4 MB. Configurable at task spawn:
```typhoon
conc(slab: 8mb) { ... }
```

### IO Driver

A thin FFI bridge to:
- **Linux**: `io_uring` — fully asynchronous, batched syscalls, zero kernel-crossing overhead for repeated operations
- **macOS**: `kqueue` — event-based, efficient for moderate concurrency
- **Windows**: `IOCP` — completion port model

The driver runs in a dedicated OS thread. It posts completed IO events to the coroutine scheduler's queue, waking the blocked coroutine.

### LLVM Backend

```
Typhoon AST
    ↓
IR Lowering      translate to LLVM IR with noalias, nonnull, readonly annotations
    ↓
LLVM Passes      inlining, vectorization, loop optimizations
    ↓
**Static Interface Resolution (Monomorphization)**

To maintain Maximum Throughput, Typhoon will not use VTables or runtime dynamic dispatch for interfaces.

*   **Compile-Time Expansion:** When a function uses a generic bound (e.g., `T: Display`), the compiler generates a unique version of that function for every concrete type used in the program.
*   **Inlining Potential:** Because the concrete type is known at the call site, LLVM can inline interface methods directly into the caller. This removes the branch-prediction penalty of a virtual call.
*   **Strict Typing:** The compiler validates that the concrete type implements all required methods of the `InterfaceDecl` before code generation.
    ↓
PGO (optional)   profile-guided optimization for hot path specialization
    ↓
Native binary / WebAssembly
```

Linear types provide LLVM with strong aliasing information. Every non-`ref` pointer gets `noalias`. The optimizer can freely reorder loads, eliminate redundant reads, and vectorize loops over arrays.

**Overflow:** In debug builds, integer arithmetic uses LLVM's `nsw`/`nuw` poison value semantics — overflow produces a trap. In release builds, wrap semantics are used.

---

## 16. Networking Layer

### Capability Model

Networking is not a global resource. The `main` function receives a `Network` capability token. All networking operations require a token to be passed explicitly.

```typhoon
fn main(net: Network) -> Result<(), AppError> {
  let listener = net.listen("0.0.0.0:8080")?
  serve(listener)
}
```

A `Network` token cannot be created by user code — it can only be passed down from `main` or forked:

```typhoon
let (net_a, net_b) = net.split()    // fork into two restricted tokens
```

This makes it impossible for library code to open network connections without explicit programmer consent.

### Linear Socket

`Socket` is a linear resource. Only one coroutine can hold it. Moving it into a `conc` block transfers ownership and revokes access from the sender.

```typhoon
let socket = listener.accept()?   // Socket is linear
conc {
  handle_connection(socket)       // socket moved here
}
// socket no longer accessible in this scope
```

### Zero-Copy HTTP Parsing

The IO driver writes incoming bytes directly into the receiving coroutine's slab. The HTTP parser creates `StrView` values — pointers into the slab buffer — for method, path, and headers. No string copies occur during parsing.

```typhoon
// Internal representation — not exposed directly to user code
struct Request {
  method:  Str,          // view into slab buffer
  path:    Str,          // view into slab buffer
  headers: [Header],     // array of (Str, Str) views into slab buffer
  body:    [Byte],       // view into slab buffer
}

struct Header {
  name:  Str,
  value: Str,
}
```

Headers are stored in a small fixed-capacity array and searched via linear scan. For the typical HTTP request with 10–20 headers, a linear scan over a cache-hot array outperforms a hash map.

### Request Lifecycle

```
1. Accept     listener.accept() → LinearSocket
2. Spawn      socket moved into conc block → new coroutine, new slab
3. Read       io_uring reads bytes directly into coroutine slab
4. Parse      resumable state machine creates StrViews into slab
5. Dispatch   user handler receives &Request (view into slab — no copy)
6. Respond    Response built in slab, moved to IO driver
7. Nuke       coroutine exits → slab reset in O(1)
```

### Example Server

```typhoon
use http::{Server, Request, Response, Router}

fn handle(req: &Request) -> Response {
  match (req.method, req.path) {
    ("GET", "/health") => Response::ok("ok"),
    ("GET", "/users")  => {
      let users = db.users.all()?
      Response::json(users)
    },
    _ => Response::not_found(),
  }
}

fn main(net: Network) -> Result<(), AppError> {
  let listener = net.listen("0.0.0.0:8080")?
  println("Listening on :8080")

  loop {
    let socket = listener.accept()?
    conc { handle_connection(socket, handle) }
  }
}
```

---

## 17. Implementation Plan

### Phase 1 — Lexer and Parser

**Goal:** parse all Typhoon syntax; produce a concrete AST with source spans.

Key decisions to resolve in this phase:
- Operator precedence table (pipe `|>` binds lowest; `?` binds highest after field access)
- Disambiguation of `{ ...x, f: v }` merge expression vs `{ stmt; expr }` block
- String interpolation: lexer tokenizes `"hello {expr} world"` as a sequence of string parts and expression spans

Output milestone: all example programs in this specification parse without error.

---

### Phase 2 — Name Resolution and Type Inference

**Goal:** resolve every identifier to its declaration; infer all types via bidirectional HM.

- Name resolution builds a scope tree; all identifiers resolve to a canonical `DeclId`
- Type inference handles generics, interface bounds, `Result`/`Option` desugaring
- `?` operator desugared here: verified the enclosing function returns a compatible `Result`/`Option`
- `|>` eliminated here: rewritten to direct calls before IR lowering

Output milestone: all example programs type-check. Invalid programs produce clear type errors.

---

### Phase 3 — Liveness Checker

**Goal:** enforce linear type rules; annotate every binding with its consumption point.

The checker maintains a live set per scope. Rules:

```
let x = expr         add x to live set
use x                if x not in live set → error "x was moved"
                     else remove x from live set
merge { ...x, .. }   x consumed; result binding added
conc { ... x ... }   x consumed in parent at conc statement
end of scope         remaining live bindings → insert compiler-generated drop
```

`let mut` bindings are exempt from liveness tracking. They are freed at scope exit unconditionally.

Conditional liveness: all branches of `if`/`match` must consume the same live bindings. Divergence is a compile error.

Output milestone: every ownership violation from the design discussion is caught with a clear error message. Test suite covers conditional moves, loop moves, closure captures, conc captures.

---

### Phase 4 — LLVM Code Generation

**Goal:** produce correct native binaries for all example programs.

- Structs lowered to LLVM structs; fields sorted by decreasing alignment unless `@repr(C)`
- Stack allocation via `alloca` for `let` bindings of size-stable types
- Slab allocation (via `malloc` placeholder in this phase) for heap types
- `match` lowered to `switch` for integer patterns; decision trees for structural patterns
- `noalias` on all non-`ref` pointers; `nonnull` on all non-optional pointers
- Overflow: `nsw`/`nuw` in debug, wrapping in release

Output milestone: all example programs compile to native binaries and produce correct output. Typhoon is a real language at this point.

---

### Phase 5 — Slab Allocator and Scheduler

**Goal:** replace `malloc`/`free` placeholder with production runtime.

- Bump allocator with size-class free list per task slab
- Virtual memory reservation at task spawn; `mmap` on Linux/macOS, `VirtualAlloc` on Windows
- M:N scheduler: work-stealing deques, one OS thread per core
- Stackful coroutines: 64 KB initial stack, `mprotect` guard page, grows on fault
- Cooperative yield at I/O operations and blocked channel operations
- Preemptive yield via `SIGPROF` timer for CPU-bound tasks
- `chan<T>` as a bounded ring buffer with coroutine waitlists

Output milestone: `conc` and `chan` examples run correctly under concurrent load. Allocator benchmarks favorably against `jemalloc` for short-lived allocation patterns.

---

### Phase 6 — IO and Networking

**Goal:** working HTTP server with zero-copy parsing and capability model.

- IO driver: `io_uring` on Linux, `kqueue` on macOS, `IOCP` on Windows — via thin Rust FFI bridge
- Read/write operations transparently yield to the coroutine scheduler
- `Network` capability token generated by the compiler's `main` entry point
- `LinearSocket` as a linear resource; liveness checker enforces single-owner semantics
- HTTP/1.1 resumable state machine parser; `StrView` pointers into slab
- Header linear scan; no `HashMap` in the hot path
- `Response` builder writes directly into the slab; IO driver reads from slab

Output milestone: HTTP server handles 10,000 concurrent connections; benchmarks against Go `net/http` and Rust `hyper`.

---

### Phase 7 — Standard Library

Priority order:

```
Tier 1 (all programs need this):
  Str, Buf, [T], Map<K,V>, Set<T>, Option<T>, Result<T,E>
  std::io  (read_file, write_file, println, scan)

Tier 2 (practical programs):
  ref T, std::math, std::time, std::fmt, std::fs, std::process

Tier 3 (ecosystem):
  json, http (client), test, @derive
```

---

## 18. Open Questions

The following concerns are identified but deferred. They do not block the implementation phases above, but must be resolved before a stable 1.0 release.

**Recursive data structures.** A `struct Node { children: [Node] }` requires the compiler to handle types of unknown size. The canonical solution is to require heap indirection at the recursive position: `children: [ref Node]`. This should be a compiler error with a helpful message ("recursive types must use ref at the recursive position") rather than undefined behavior.

**`select` ownership semantics.** `select` waits on multiple channels simultaneously. If two channels become ready simultaneously, one is chosen and the other is not. The binding for the unchosen arm was never consumed. The liveness checker needs explicit rules for `select` arms — likely: all arms must consume the same set of external live bindings, same as `match`.

**WebAssembly target differences.** Wasm has no threads in the base spec (threads require the Atomics proposal), no `mmap`, and no signal-based preemption. The scheduler and slab allocator need Wasm-specific implementations. This is a platform variant, not a language change.

**Numeric coercion.** Does `Int8 + Int32` compile? Does it require an explicit cast? Implicit widening is convenient but masks bugs. Explicit casting (`42i8 as Int32`) is safer. Recommendation: no implicit numeric coercion; explicit `as` for all cross-type arithmetic.

**Panic vs `Result`.** Some operations that cannot return `Result` (out-of-memory, stack overflow, divide by zero in release) need a panic mechanism. The design currently traps on overflow in debug. A full panic story — including panic hooks, stack unwinding vs abort — is needed before production use.

**`@derive` implementation mechanism.** `@derive` generates code at compile time. The compiler needs a macro expansion phase between parsing and type checking. Whether this is a built-in list of derivable traits or a general compile-time metaprogramming system is an open design question.
