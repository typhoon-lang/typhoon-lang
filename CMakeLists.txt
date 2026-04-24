cmake_minimum_required(VERSION 3.15) # 3.15+ is better for MSVC runtime abstraction
project(Typhoon C)

# Enable Assembly support
enable_language(ASM)

# 1. THE CRT FIX (Windows-Specific but safe for others)
# This is the modern, "correct" way to force Static CRT in CMake 3.15+
if(MSVC)
    set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
else()
    # On Linux/macOS, we don't need to do anything special for 'static libc' usually,
    # but we can ensure position independent code for the static library.
    set(CMAKE_POSITION_INDEPENDENT_CODE ON)
endif()

# 2. DEFINE THE LIBRARY
add_library(runtime STATIC
    runtime/platform.h
    runtime/atomic.h
    runtime/scheduler.h
    runtime/io_driver.h
    runtime/ty_mem.h
    runtime/ty_net.h
    runtime/ty_ctx.S
    runtime/io_driver.c
    runtime/scheduler.c
    runtime/ty_mem.c
    runtime/ty_net.c
    runtime/ty_io.c)

target_compile_definitions(runtime PRIVATE TY_SCHED_DEBUG_ENABLED=0)

# 3. OUTPUT MANAGEMENT
set(OUTPUT_DIR "${CMAKE_BINARY_DIR}/output")

# Set the output directory for all configurations (Debug, Release, etc.)
set_target_properties(runtime PROPERTIES
    ARCHIVE_OUTPUT_DIRECTORY "${OUTPUT_DIR}"
    LIBRARY_OUTPUT_DIRECTORY "${OUTPUT_DIR}"
    RUNTIME_OUTPUT_DIRECTORY "${OUTPUT_DIR}"
)

# Ensure multi-config generators (like VS) don't add /Debug or /Release subfolders
foreach(config ${CMAKE_CONFIGURATION_TYPES})
    string(TOUPPER ${config} CONFIG_UPPER)
    set_target_properties(runtime PROPERTIES
        ARCHIVE_OUTPUT_DIRECTORY_${CONFIG_UPPER} "${OUTPUT_DIR}"
    )
endforeach()

# 4. POST-BUILD COPY (Optional, as the properties above handle most cases)
add_custom_command(TARGET runtime POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different
        $<TARGET_FILE:runtime>
        "${OUTPUT_DIR}/"
    COMMENT "Ensuring runtime artifact is in ${OUTPUT_DIR}"
)
