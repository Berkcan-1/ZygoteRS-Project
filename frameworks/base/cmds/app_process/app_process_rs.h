/*
 * C ABI between app_main.cpp (AppRuntime + legacy path) and the Rust startup
 * logic in rs/lib.rs. Keep in sync with the `extern "C"` blocks in rs/lib.rs.
 */
#pragma once

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Returned by app_process_rs_main() when it did NOT touch anything and the
 * caller must run the legacy C++ path instead. Real exit codes are never
 * negative. Must equal APP_PROCESS_RS_DECLINED in rs/lib.rs. */
#define APP_PROCESS_RS_DECLINED (-1000)

/* ---- implemented in Rust (rs/lib.rs) ---- */
int app_process_rs_main(int argc, char* const* argv,
                        const char* abi_string,
                        const char* abi_list_property,
                        const char* zygote_nice_name);

/* ---- implemented in app_main.cpp, called from Rust ---- */
void* app_process_shim_create(char* arg_block_start, size_t arg_block_length);
void app_process_shim_add_option(void* rt, const char* option);
void app_process_shim_set_class_and_args(void* rt, const char* class_name,
                                         int argc, const char* const* argv);
void app_process_shim_set_argv0(void* rt, const char* nice_name);
void app_process_shim_start(void* rt, const char* class_name, int argc,
                            const char* const* argv, int zygote);

#ifdef __cplusplus
}
#endif
