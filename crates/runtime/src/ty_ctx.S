/*
 * ty_ctx.S — coroutine context switch, compiled once and linked once.
 *
 * Two functions:
 *   ty_ctx_swap(TyCtx* from, TyCtx* to)
 *   ty__coro_entry()
 *
 * Supported targets:
 *   x86-64  (Linux + macOS)
 *   AArch64 (Linux + macOS / Apple Silicon)
 */

/* macOS prefixes C symbols with '_'; Linux does not.
 * SYM(name)       — the platform-canonical label (e.g. _foo on macOS, foo on Linux)
 * SYM_ALIAS(name) — the unprefixed alias, emitted ONLY on macOS where it differs
 *                   from SYM(name).  On Linux the two would be identical, which
 *                   causes "symbol already defined" assembler errors. */
#if defined(__APPLE__)
#  define SYM(name)        _ ## name
#  define SYM_ALIAS(name)  name:
#else
#  define SYM(name)        name
#  define SYM_ALIAS(name)  /* nothing — SYM(name) already is 'name' */
#endif

/* ── x86-64 ──────────────────────────────────────────────────────────── */
#if defined(__x86_64__) || defined(__amd64__)

.data
.globl SYM(ty__entry_rsp)
SYM_ALIAS(ty__entry_rsp)
SYM(ty__entry_rsp):
    .quad 0

.globl SYM(ty__entry_top)
SYM_ALIAS(ty__entry_top)
SYM(ty__entry_top):
    .quad 0

.text

/*
 * ty_ctx_swap(TyCtx* from [rdi], TyCtx* to [rsi])
 *
 * TyCtx.regs layout:
 *   [0]  rsp   [8]  r15   [16] r14   [24] r13
 *   [32] r12   [40] rbx   [48] rbp   [56] rip
 */
.globl SYM(ty_ctx_swap)
SYM_ALIAS(ty_ctx_swap)
SYM(ty_ctx_swap):
    movq (%rsp), %rax      /* read return addr pushed by 'call ty_ctx_swap' */
    movq %rax,  56(%rdi)   /* save it as the true resume RIP                */
    leaq 8(%rsp), %rax    /* compute post-call RSP (past the return addr)   */
    movq %rax,   0(%rdi)  /* save adjusted RSP                             */
    movq %r15,   8(%rdi)
    movq %r14,  16(%rdi)
    movq %r13,  24(%rdi)
    movq %r12,  32(%rdi)
    movq %rbx,  40(%rdi)
    movq %rbp,  48(%rdi)

    /* restore block unchanged */
    movq  0(%rsi), %rsp
    movq  8(%rsi), %r15
    movq 16(%rsi), %r14
    movq 24(%rsi), %r13
    movq 32(%rsi), %r12
    movq 40(%rsi), %rbx
    movq 48(%rsi), %rbp
    jmpq *56(%rsi)

/*
 * ty__coro_entry — first entry point for a new coroutine.
 *
 * Stack on entry (set up by ty_ctx_init):
 *   [rsp+0]   ty__coro_entry  ← consumed as return addr by jmpq *56(%rsi)
 *   [rsp+0]   tramp           ← now rsp points here after the jmpq
 *   [rsp+8]   hi
 *   [rsp+16]  lo
 */
.globl SYM(ty__coro_entry)
SYM_ALIAS(ty__coro_entry)
SYM(ty__coro_entry):
    popq %rax                   /* tramp → scratch register    */
    popq %rdi                   /* hi   → first  arg (uint32_t) */
    popq %rsi                   /* lo   → second arg (uint32_t) */
    jmpq *%rax                  /* tail-call tramp(hi, lo)      */

/* ── AArch64 ─────────────────────────────────────────────────────────── */
#elif defined(__aarch64__)

.text

/*
 * ty_ctx_swap(TyCtx* from [x0], TyCtx* to [x1])
 *
 * TyCtx.regs layout (uint64_t[22]):
 *   [0]  x19  [8]  x20  [16] x21  [24] x22
 *   [32] x23  [40] x24  [48] x25  [56] x26
 *   [64] x27  [72] x28  [80] x29(fp) [88] x30(lr)
 *   [96] sp   [104] d8   [112] d9   [120] d10  [128] d11
 *   [136] d12  [144] d13  [152] d14  [160] d15
 *   regs[]=22 fits save (12 GPR + sp + 8 SIMD = 21 uint64_t = 168 bytes)
 *   in the 176-byte struct; TyCtx is 16-byte-aligned for stp/ldp.
 */
.globl SYM(ty_ctx_swap)
SYM_ALIAS(ty_ctx_swap)
SYM(ty_ctx_swap):
    stp x19, x20, [x0,  #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    mov x9,  sp
    str x9,  [x0, #96]
    /* Save callee-saved SIMD registers (lower 64 bits) */
    stp d8,  d9,  [x0, #104]
    stp d10, d11, [x0, #120]
    stp d12, d13, [x0, #136]
    stp d14, d15, [x0, #152]

    ldp x19, x20, [x1,  #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    ldr x9,  [x1, #96]
    mov sp,  x9
    /* Restore callee-saved SIMD registers */
    ldp d8,  d9,  [x1, #104]
    ldp d10, d11, [x1, #120]
    ldp d12, d13, [x1, #136]
    ldp d14, d15, [x1, #152]
    ret

/*
 * ty__coro_entry — first entry point for a new coroutine.
 *
 * On entry, callee-saved regs hold the payload set by ty_ctx_init:
 *   x19 = tramp   x20 = hi (uint32_t)   x21 = lo (uint32_t)
 */
.globl SYM(ty__coro_entry)
SYM_ALIAS(ty__coro_entry)
SYM(ty__coro_entry):
    mov  x9,  x19               /* tramp → scratch (x19 is callee-saved) */
    mov  w0,  w20               /* hi → first  arg                     */
    mov  w1,  w21               /* lo → second arg                     */
    blr  x9                     /* call tramp(hi, lo)                  */
    brk  #0                     /* unreachable                         */

#endif /* arch */

#if defined(__linux__) && defined(__ELF__)
.section .note.GNU-stack,"",@progbits
#endif
