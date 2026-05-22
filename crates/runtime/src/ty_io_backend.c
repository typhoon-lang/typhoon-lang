#include "ty_io_backend.h"
#include "io_driver.h"
#include "scheduler.h"

int ty_io_submit(const TyIoOp* op) {
    (void)op;
    /* Transitional Phase 4 shim: existing runtime paths still submit
     * through ty_io_read/ty_io_write directly. */
    return 0;
}

int ty_io_poll(void) {
    /* Transitional shim: current driver owns its own poll strategy.
     * Keep this callable from scheduler idle path. */
    return 0;
}

