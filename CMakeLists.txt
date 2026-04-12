cmake_minimum_required(VERSION 3.10)
project(GeneratedRunner C)

# Enable Assembly support
enable_language(ASM)

add_library(runtime STATIC
    runtime/platform.h
    runtime/atomic.h
    runtime/scheduler.h
    runtime/ty_ctx.S
    runtime/scheduler.c
    runtime/runtime.c)

if(NOT APPLE)
    set_target_properties(runtime PROPERTIES LINK_FLAGS "-static")
endif()

# Define a single output directory
set(OUTPUT_DIR "${CMAKE_BINARY_DIR}/output")
file(MAKE_DIRECTORY ${OUTPUT_DIR})

# Force static libraries (.a / .lib) to this directory
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY ${OUTPUT_DIR})

# Copy the artifact to the flat directory after build
add_custom_command(TARGET runtime POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy
        $<TARGET_FILE:runtime>
        "${OUTPUT_DIR}/"
    COMMENT "Copying runtime artifact to global output directory"
)
