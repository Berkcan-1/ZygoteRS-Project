/*
 * zygote_rs.h — C ABI of the Rust part of Zygote's JNI layer (see src/lib.rs).
 * Hand-written on purpose: the surface is small and cbindgen would add a host tool
 * to the build. Keep in sync with src/lib.rs (layouts are checked by static_asserts
 * in the C++ glue and by unit tests on the Rust side).
 */
#ifndef ZYGOTE_RS_H_
#define ZYGOTE_RS_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ZRS_ERR_MSG_LEN 256
#define ZRS_USAP_POOL_SIZE_MAX_LIMIT 100 /* mirror of ZygoteServer.USAP_POOL_SIZE_MAX_LIMIT */
#define ZRS_SIGCHLD_MSG_SIZE 16          /* sizeof(UnsolicitedZygoteMessageSigChld) */
#define ZRS_MAX_COMMAND_BYTES 32768
#define ZRS_NICE_NAME_BYTES 128

/* Filled by every fallible call that returns false. msg is always NUL terminated. */
typedef struct ZrsError {
    int32_t errnum;
    char msg[ZRS_ERR_MSG_LEN];
} ZrsError;

/* Opaque; lives in its own anonymous mmap (page aligned, like the old C++ buffer). */
typedef struct ZrsCmdBuf ZrsCmdBuf;

/* ── USAP table ───────────────────────────────────────────────────────────── */
bool zrs_usap_add(int32_t pid, int32_t read_pipe_fd);  /* false: table full */
bool zrs_usap_remove(int32_t pid);                     /* ASYNC-SIGNAL-SAFE */
uint32_t zrs_usap_count(void);
void zrs_usap_clear_all(void);                         /* child after fork */
void zrs_usap_empty_pool(void);                        /* SIGTERM + close + forget */
size_t zrs_usap_read_fds(int32_t* out, size_t cap);    /* returns #written */

/* ── SIGCHLD wire message ─────────────────────────────────────────────────── */
/* 3 and out[0..3] = {pid, uid, status} on success, -1 if not a SIGCHLD message. */
int32_t zrs_parse_sigchld(const uint8_t* data, size_t len, int32_t* out);

/* ── capabilities / credentials ───────────────────────────────────────────── */
bool zrs_calculate_capabilities(int32_t uid, int32_t gid, const int32_t* gids, size_t gids_len,
                                bool has_gids, bool is_child_zygote, uint64_t* out,
                                ZrsError* err);
bool zrs_keep_capabilities(ZrsError* err);
bool zrs_drop_bounding_set(ZrsError* err);
bool zrs_set_inheritable(uint64_t inheritable, ZrsError* err);
bool zrs_set_capabilities(uint64_t permitted, uint64_t effective, uint64_t inheritable,
                          ZrsError* err);
bool zrs_set_gids(const int32_t* gids, size_t gids_len, bool has_gids, bool is_child_zygote,
                  ZrsError* err);
/* triples = flat {resource, rlim_cur, rlim_max}* */
bool zrs_set_rlimits(const int32_t* triples, size_t len, ZrsError* err);

/* ── command buffer ───────────────────────────────────────────────────────── */
ZrsCmdBuf* zrs_cmdbuf_new(int32_t fd);                 /* NULL on mmap failure */
bool zrs_cmdbuf_free(ZrsCmdBuf* b);
bool zrs_cmdbuf_get_count(ZrsCmdBuf* b, int32_t* out, ZrsError* err);  /* *out==0: EOF */
/* *out_ptr is NOT NUL terminated; valid until the next call on this buffer. */
bool zrs_cmdbuf_next_arg(ZrsCmdBuf* b, const char** out_ptr, size_t* out_len, ZrsError* err);
bool zrs_cmdbuf_read_fully_and_reset(ZrsCmdBuf* b, ZrsError* err);
bool zrs_cmdbuf_insert(ZrsCmdBuf* b, const uint8_t* line, size_t len, ZrsError* err);
bool zrs_cmdbuf_is_simple_fork(ZrsCmdBuf* b, int32_t min_uid, bool* out, ZrsError* err);
void zrs_cmdbuf_reset(ZrsCmdBuf* b);
void zrs_cmdbuf_clear(ZrsCmdBuf* b);
int32_t zrs_cmdbuf_fd(const ZrsCmdBuf* b);
void zrs_cmdbuf_set_fd(ZrsCmdBuf* b, int32_t fd);
const char* zrs_cmdbuf_nice_name(const ZrsCmdBuf* b);  /* NUL terminated, may be "" */
void zrs_cmdbuf_log_state(const ZrsCmdBuf* b);

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif /* ZYGOTE_RS_H_ */
