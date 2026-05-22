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
    src/ty_ctx.S
    src/io_driver.c
    src/ty_io_backend.c
    src/scheduler.c
    src/ty_mem.c
    src/ty_net.c
    src/ty_io.c)
target_include_directories(runtime PUBLIC include/)
target_compile_definitions(runtime PRIVATE TY_DEBUG_ENABLED=0)

# 3. INSTALL PHASE (Replaces or complements POST-BUILD)
# This defines where the outputs go when you run 'make install'
install(TARGETS runtime
    ARCHIVE DESTINATION lib  # This creates the 'lib' folder
    LIBRARY DESTINATION lib
)

# 4. TEST PHASE
option(BUILD_TESTS "Build C test suite" OFF)

if(BUILD_TESTS)
    # Add each test source file here. One entry = one test binary.
    # The binary name is derived from the filename (without extension).
    set(C_TEST_SOURCES
        test/test_task01_dangling_ptr.c
        test/test_task02_sscan_mutation.c
        test/test_task03_double_close.c
        test/test_task04_overflow.c
        test/test_phase2_listener_close.c
        test/test_phase2_accept_write_close.c
        test/test_phase3_large_sprintf.c
    )

    foreach(test_src ${C_TEST_SOURCES})
        # Derive binary name from filename without extension
        get_filename_component(test_name ${test_src} NAME_WE)

        add_executable(${test_name} ${test_src})
        target_include_directories(${test_name} PUBLIC include/)
        target_link_libraries(${test_name} PRIVATE runtime)

        # Install every test binary to bin/ so build.rs can find them
        install(TARGETS ${test_name} DESTINATION bin)
    endforeach()
endif()
