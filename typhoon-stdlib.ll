%struct.Buf = type { i8*, i64, i64 }
%struct.TyArray = type { i8*, i64, i64, i64, i64 }
; @ty_ns: std::net
%struct.Network  = type { i8* }
; @ty_ns: std::net
%struct.Listener = type { i8* }
; @ty_ns: std::net
%struct.Socket = type { i8* }
; Result type for listen/accept operations
%struct.Result__struct_Listenerptr__i32 = type { i8, i8, i8 }
%struct.Result__struct_Socketptr__i32 = type { i8, i8, i8 }
%struct.Array = type opaque
; @ty_ns: std::option
%enum.Option<T> = type { Some(T), None }
; @ty_ns: std::result
%enum.Result<T, E> = type { Ok(T), Err(E) }
declare void @ty_sched_init     ()
declare void @ty_sched_run      ()
declare void @ty_sched_shutdown ()
declare i8*  @ty_spawn          (i8*, i8*, i8*)
declare i8*  @ty_spawn_closure  (i8*, i8*, i8*, i64)
declare void @ty_yield          ()
declare void @ty_safepoint      ()
declare void @ty_await          (i8*, i8*)
declare i8*  @ty_chan_new       (i64, i64)
declare void @ty_chan_send      (i8*, i8*, i8*)
declare void @ty_chan_recv      (i8*, i8*, i8*)
declare i32  @ty_chan_try_recv  (i8*, i8*, i8*)
declare void @ty_chan_close     (i8*)
declare %struct.Buf* @ty_buf_new      (i8* %task)
declare void         @ty_buf_push_str (i8*, %struct.Buf*, i8*)
declare i8*          @ty_buf_into_str (i8*, %struct.Buf*)
declare %struct.TyArray* @ty_array_from_fixed (i8*, i8*, i64, i64, i64)
declare void             @ty_array_push       (i8*, %struct.TyArray*, i8*)
declare i8*              @ty_array_get_ptr    (%struct.TyArray*, i64)
declare i8*  @slab_arena_new  ()
declare i8*  @slab_alloc      (i8* %task, i32 %size_class)
declare void @slab_free       (i8* %task, i8* %ptr, i32 %size_class)
declare void @slab_arena_free (i8*)
declare void @ty_io_subsystem_init     ()
declare void @ty_io_subsystem_shutdown ()
declare i32  @ty_io_open               (i8* %driver, i8* %path, i32 %flags, i32 %mode)
declare void @ty_io_close              (i8* %driver, i32 %fd)
; @ty_ns: std::net
declare void @ty_net_init              ()
; @ty_ns: std::net
declare void @ty_net_shutdown          ()
; @ty_ns: std::net
declare %struct.Network* @ty_net_global()

; @ty_ns: std::net
declare void @__ty_rt__Socket__consume(%struct.Socket*, i8*)
; @ty_ns: std::net
declare void @__ty_rt__Socket__close(%struct.Socket*)
; @ty_ns: std::net
declare void @__ty_rt__Network__listen(i8* %task, %struct.Network* %self, i8* %addr, %struct.Result__struct_Listenerptr__i32* %out)
; @ty_ns: std::net
declare void @__ty_rt__Listener__accept(i8* %task, %struct.Listener* %self, %struct.Result__struct_Socketptr__i32* %out)

; @ty_ns: std::net
; @ty_sig: fn listen(self, addr: Str) -> Result<Listener, Int32>
define %struct.Result__struct_Listenerptr__i32 @__ty_method__Network__listen(i8* %task, %struct.Network* %self, i8* %addr) {
entry:
  %t0 = alloca i8*
  %t1 = alloca %struct.Network*
  %t2 = alloca i8*
  %result = alloca %struct.Result__struct_Listenerptr__i32
  call void @ty_safepoint()
  store i8* %task, i8** %t0
  store %struct.Network* %self, %struct.Network** %t1
  store i8* %addr, i8** %t2
  call void @__ty_rt__Network__listen(i8* %task, %struct.Network* %self, i8* %addr, %struct.Result__struct_Listenerptr__i32* %result)
  %val = load %struct.Result__struct_Listenerptr__i32, %struct.Result__struct_Listenerptr__i32* %result
  ret %struct.Result__struct_Listenerptr__i32 %val
}

; @ty_ns: std::net
; @ty_sig: fn accept(self) -> Result<Socket, Int32>
define %struct.Result__struct_Socketptr__i32 @__ty_method__Listener__accept(i8* %task, %struct.Listener* %self) {
entry:
  %t0 = alloca i8*
  %t1 = alloca %struct.Listener*
  %result = alloca %struct.Result__struct_Socketptr__i32
  call void @ty_safepoint()
  store i8* %task, i8** %t0
  store %struct.Listener* %self, %struct.Listener** %t1
  call void @__ty_rt__Listener__accept(i8* %task, %struct.Listener* %self, %struct.Result__struct_Socketptr__i32* %result)
  %val = load %struct.Result__struct_Socketptr__i32, %struct.Result__struct_Socketptr__i32* %result
  ret %struct.Result__struct_Socketptr__i32 %val
}

; @ty_ns: std::net
; @ty_sig: fn consume(self, ch: ref Chan<Int8>)
define void @__ty_method__Socket__consume(i8* %task, %struct.Socket* %self, i8* %ch) {
entry:
  %t0 = alloca i8*
  %t1 = alloca %struct.Socket*
  %t2 = alloca i8*
  call void @ty_safepoint()
  store i8* %task, i8** %t0
  store %struct.Socket* %self, %struct.Socket** %t1
  store i8* %ch, i8** %t2
  ; span 2482..2484 @ 65:43
  ret void
}

; @ty_ns: std::net
; @ty_sig: fn close(self)
define void @__ty_method__Socket__close(i8* %task, %struct.Socket* %self) {
entry:
  %t0 = alloca i8*
  %t1 = alloca %struct.Socket*
  call void @ty_safepoint()
  store i8* %task, i8** %t0
  store %struct.Socket* %self, %struct.Socket** %t1
  ; span 2504..2506 @ 66:21
  ret void
}

declare void @ty_print    (i8* %task, i8* %s)
declare void @ty_println  (i8* %task, i8* %s)
declare void @ty_printf   (i8* %task, i8* %fmt, ...)
declare void @ty_fprint   (i8* %task, i32 %fd, i8* %s)
declare void @ty_fprintln (i8* %task, i32 %fd, i8* %s)
declare void @ty_fprintf  (i8* %task, i32 %fd, i8* %fmt, ...)
declare void @ty_sprint   (i8* %task, %struct.Buf* %out, i8* %s)
declare void @ty_sprintln (i8* %task, %struct.Buf* %out, i8* %s)
declare void @ty_sprintf  (i8* %task, %struct.Buf* %out, i8* %fmt, ...)
declare i8*  @ty_scan     (i8* %task)
declare i32  @ty_scanf    (i8* %task, i8* %fmt, ...)
declare i8*  @ty_fscan    (i8* %task, i32 %fd)
declare i32  @ty_fscanf   (i8* %task, i32 %fd, i8* %fmt, ...)
declare i8*  @ty_sscan    (i8* %task, i8* %src, i8** %rest_out)
declare i32  @ty_sscanf   (i8* %task, i8* %src, i8* %fmt, ...)
declare %struct.Buf* @__ty_buf_new()
declare void @__ty_buf_push_str(%struct.Buf*, i8*)
declare i8* @__ty_buf_into_str(%struct.Buf*)
