%struct.Buf = type { i8*, i64, i64 }
%struct.TyArray = type { i8*, i64, i64, i64, i64 }
%struct.Network  = type { i8* }
%struct.Listener = type { i8* }
%struct.Socket   = type { i8* }
%struct.Array = type opaque
%enum.Option<T> = type { Some(T), None }
%enum.Result<T, E> = type { Ok(T), Err(E) }
declare void @ty_sched_init     ()
declare void @ty_sched_run      ()
declare void @ty_sched_shutdown ()
declare i8*  @ty_spawn          (i8*, i8*, i8*)
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
declare void @ty_net_init              ()
declare void @ty_net_shutdown          ()
declare %struct.Network* @ty_net_global()

; @ty_sig: fn consume(self, ch: ref chan<Int8>)
declare void @__ty_rt__Socket__consume(%struct.Socket*, i8*)

; @ty_sig: fn close(self)
declare void @__ty_rt__Socket__close(%struct.Socket*)

; @ty_sig: fn listen(self, addr: Str) -> Result<Listener, Int32>
declare void @__ty_rt__Network__listen(i8* %task, %struct.Network* %self, i8* %addr, %struct.Result__struct_Listener__i32* %out)

; @ty_sig: fn accept(self) -> Result<Socket, Int32>
declare void @__ty_rt__Listener__accept(i8* %task, %struct.Listener* %self, %struct.Result__struct_Socket__i32* %out)

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
