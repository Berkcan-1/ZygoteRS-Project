/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "com_android_internal_os_Zygote.h"
#include "zygote_rs.h"

#include <algorithm>
#include <android-base/logging.h>
#include <async_safe/log.h>
#include <cctype>
#include <chrono>
#include <core_jni_helpers.h>
#include <errno.h>
#include <fcntl.h>
#include <jni.h>
#include <nativehelper/JNIHelp.h>
#include <optional>
#include <poll.h>
#include <string>
#include <unistd.h>
#include <utility>
#include <utils/misc.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/socket.h>
#include <vector>

namespace android {

using android::base::StringPrintf;
using android::zygote::ZygoteFailure;

// WARNING: Knows a little about the wire protocol used to communicate with Zygote.
//
// The command buffer itself (line reader, argument counting, the "simple fork command"
// classifier) now lives in Rust: zygote_rs, src/cmdbuf.rs. This file keeps only the JNI
// marshalling and nativeForkRepeatedly(), which owns fork()/poll()/accept() and therefore
// stays in C++ for now.

static inline ZrsCmdBuf* AsBuf(jlong j_buffer) {
  return reinterpret_cast<ZrsCmdBuf*>(j_buffer);
}

// Get a new command buffer. Can only be called once between freeNativeBuffer calls,
// so that only one buffer exists at a time (enforced in Rust).
jlong com_android_internal_os_ZygoteCommandBuffer_getNativeBuffer(JNIEnv* env, jclass, jint fd) {
  ZrsCmdBuf* buffer = zrs_cmdbuf_new(fd);
  if (buffer == nullptr) {
    ZygoteFailure(env, nullptr, nullptr, "Failed to map argument buffer");
  }
  return reinterpret_cast<jlong>(buffer);
}

// Delete native command buffer.
void com_android_internal_os_ZygoteCommandBuffer_freeNativeBuffer(JNIEnv* env, jclass,
                                                                  jlong j_buffer) {
  if (!zrs_cmdbuf_free(AsBuf(j_buffer))) {
    ZygoteFailure(env, nullptr, nullptr, "Failed to unmap argument buffer");
  }
}

// Clear the buffer, read the line containing the count, and return the count.
jint com_android_internal_os_ZygoteCommandBuffer_nativeGetCount(JNIEnv* env, jclass,
                                                                jlong j_buffer) {
  ZrsError err;
  int32_t count = 0;
  if (!zrs_cmdbuf_get_count(AsBuf(j_buffer), &count, &err)) {
    ZygoteFailure(env, nullptr, nullptr, err.msg);
  }
  return count;
}

// Explicitly insert a string as the last line (argument) of the buffer.
void com_android_internal_os_ZygoteCommandBuffer_insert(JNIEnv* env, jclass, jlong j_buffer,
                                                        jstring line) {
  size_t lineLen = static_cast<size_t>(env->GetStringUTFLength(line));
  const char* cstring = env->GetStringUTFChars(line, NULL);
  ZrsError err;
  bool ok = zrs_cmdbuf_insert(AsBuf(j_buffer), reinterpret_cast<const uint8_t*>(cstring), lineLen,
                              &err);
  env->ReleaseStringUTFChars(line, cstring);
  if (!ok) {
    ZygoteFailure(env, nullptr, nullptr, err.msg);
  }
}

// Read a line from the buffer, refilling as necessary.
jstring com_android_internal_os_ZygoteCommandBuffer_nativeNextArg(JNIEnv* env, jclass,
                                                                  jlong j_buffer) {
  ZrsCmdBuf* buffer = AsBuf(j_buffer);
  const char* arg = nullptr;
  size_t arg_len = 0;
  ZrsError err;
  if (!zrs_cmdbuf_next_arg(buffer, &arg, &arg_len, &err)) {
    ZygoteFailure(env, zrs_cmdbuf_nice_name(buffer), nullptr, err.msg);
  }
  // The line is not NUL terminated inside the buffer; copy it out (args are short).
  std::string copy(arg, arg_len);
  return env->NewStringUTF(copy.c_str());
}

// Read all lines from the current command into the buffer, and then reset the buffer, so
// we will start reading again at the beginning of the command, starting with the argument
// count. And we don't need access to the fd to do so.
void com_android_internal_os_ZygoteCommandBuffer_nativeReadFullyAndReset(JNIEnv* env, jclass,
                                                                         jlong j_buffer) {
  ZrsCmdBuf* buffer = AsBuf(j_buffer);
  ZrsError err;
  if (!zrs_cmdbuf_read_fully_and_reset(buffer, &err)) {
    ZygoteFailure(env, zrs_cmdbuf_nice_name(buffer), nullptr, err.msg);
  }
}

// Fork a child as specified by the current command buffer, and refill the command
// buffer from the given socket. So long as the result is another simple fork command,
// repeat this process.
// It must contain a fork command, which is currently restricted not to fork another
// zygote or involve a wrapper process.
// The initial buffer should be partially or entirely read; we read it fully and reset it.
// When we return, the buffer contains the command we couldn't handle, and has been reset().
// We return false in the parent when we see a command we didn't understand, and thus the
// command in the buffer still needs to be executed.
// We return true in each child.
// We only process fork commands if the peer uid matches expected_uid.
// For every fork command after the first, we check that the requested uid is at
// least minUid.
jboolean com_android_internal_os_ZygoteCommandBuffer_nativeForkRepeatedly(
            JNIEnv* env,
            jclass,
            jlong j_buffer,
            jint zygote_socket_fd,
            jint expected_uid,
            jint minUid,
            jstring managed_nice_name) {

  ALOGI("Entering forkRepeatedly native zygote loop");
  ZrsCmdBuf* n_buffer = AsBuf(j_buffer);
  int session_socket = zrs_cmdbuf_fd(n_buffer);
  std::vector<int> session_socket_fds {session_socket};

  // Failure reporters. The original used three std::bind objects; a flag keeps this one type.
  //  - first_time == true : name comes from the Java-side nice name (only known then)
  //  - first_time == false: name is read from the buffer *at failure time*, because
  //    is_simple_fork updates it for every new command (the original bound its address).
  auto fail_buf = [&](bool first_time, const std::string& msg) {
    if (first_time) {
      ZygoteFailure(env, static_cast<const char*>(nullptr), managed_nice_name, msg);
    } else {
      ZygoteFailure(env, zrs_cmdbuf_nice_name(n_buffer), static_cast<jstring>(nullptr), msg);
    }
  };
  auto fail_fn_z = [&](const std::string& msg) {
    ZygoteFailure(env, "zygote", nullptr, msg);
  };

  struct pollfd fd_structs[2];
  static const int ZYGOTE_IDX = 0;
  static const int SESSION_IDX = 1;
  fd_structs[ZYGOTE_IDX].fd = zygote_socket_fd;
  fd_structs[ZYGOTE_IDX].events = POLLIN;
  fd_structs[SESSION_IDX].fd = session_socket;
  fd_structs[SESSION_IDX].events = POLLIN;

  struct timeval timeout;
  socklen_t timeout_size = sizeof timeout;
  if (getsockopt(session_socket, SOL_SOCKET, SO_RCVTIMEO, &timeout, &timeout_size) != 0) {
    fail_fn_z("Failed to retrieve session socket timeout");
  }

  struct ucred credentials;
  socklen_t cred_size = sizeof credentials;
  if (getsockopt(zrs_cmdbuf_fd(n_buffer), SOL_SOCKET, SO_PEERCRED, &credentials, &cred_size) == -1
      || cred_size != sizeof credentials) {
    fail_buf(true, CREATE_ERROR("ForkRepeatedly failed to get initial credentials, %s",
                                strerror(errno)));
  }

  bool first_time = true;
  do {
    if (credentials.uid != expected_uid) {
      return JNI_FALSE;
    }
    {
      ZrsError err;
      if (!zrs_cmdbuf_read_fully_and_reset(n_buffer, &err)) {
        fail_buf(first_time, err.msg);
      }
    }
    int pid = zygote::forkApp(env, /* no pipe FDs */ -1, -1, session_socket_fds,
                              /*args_known=*/ true, /*is_priority_fork=*/ true,
                              /*purge=*/ first_time);
    if (pid == 0) {
      return JNI_TRUE;
    }
    // We're in the parent. Write big-endian pid, followed by a boolean.
    char pid_buf[5];
    int tmp_pid = pid;
    for (int i = 3; i >= 0; --i) {
      pid_buf[i] = tmp_pid & 0xff;
      tmp_pid >>= 8;
    }
    pid_buf[4] = 0;  // Process is not wrapped.
    int res = TEMP_FAILURE_RETRY(write(session_socket, pid_buf, 5));
    if (res != 5) {
      if (res == -1) {
        fail_buf(first_time, CREATE_ERROR("Pid write error %d: %s", errno, strerror(errno)));
      } else {
        fail_buf(first_time, CREATE_ERROR("Write unexpectedly returned short: %d < 5", res));
      }
    }
    // Clear buffer and get count from next command.
    zrs_cmdbuf_clear(n_buffer);
    for (;;) {
      // Poll isn't strictly necessary for now. But without it, disconnect is hard to detect.
      int poll_res = TEMP_FAILURE_RETRY(poll(fd_structs, 2, -1 /* infinite timeout */));
      if ((fd_structs[SESSION_IDX].revents & POLLIN) != 0) {
        ZrsError err;
        int32_t count = 0;
        if (!zrs_cmdbuf_get_count(n_buffer, &count, &err)) {
          fail_fn_z(err.msg);
        }
        if (count != 0) {
          break;
        }  // else disconnected;
      } else if (poll_res == 0 || (fd_structs[ZYGOTE_IDX].revents & POLLIN) == 0) {
        fail_fn_z(
            CREATE_ERROR("Poll returned with no descriptors ready! Poll returned %d", poll_res));
      }
      // We've now seen either a disconnect or connect request.
      close(session_socket);
      int new_fd = TEMP_FAILURE_RETRY(accept(zygote_socket_fd, nullptr, nullptr));
      if (new_fd == -1) {
        fail_fn_z(CREATE_ERROR("Accept(%d) failed: %s", zygote_socket_fd, strerror(errno)));
      }
      if (new_fd != session_socket) {
          // Move new_fd back to the old value, so that we don't have to change Java-level data
          // structures to reflect a change. This implicitly closes the old one.
          if (TEMP_FAILURE_RETRY(dup2(new_fd, session_socket)) != session_socket) {
            fail_fn_z(CREATE_ERROR("Failed to move fd %d to %d: %s",
                                   new_fd, session_socket, strerror(errno)));
          }
          close(new_fd);  //  On Linux, fd is closed even if EINTR is returned.
      }
      // If we ever return, we effectively reuse the old Java ZygoteConnection.
      // None of its state needs to change.
      if (setsockopt(session_socket, SOL_SOCKET, SO_RCVTIMEO, &timeout, timeout_size) != 0) {
        fail_fn_z(CREATE_ERROR("Failed to set receive timeout for socket %d: %s",
                               session_socket, strerror(errno)));
      }
      if (setsockopt(session_socket, SOL_SOCKET, SO_SNDTIMEO, &timeout, timeout_size) != 0) {
        fail_fn_z(CREATE_ERROR("Failed to set send timeout for socket %d: %s",
                               session_socket, strerror(errno)));
      }
      if (getsockopt(session_socket, SOL_SOCKET, SO_PEERCRED, &credentials, &cred_size) == -1) {
        fail_fn_z(CREATE_ERROR("ForkMany failed to get credentials: %s", strerror(errno)));
      }
      if (cred_size != sizeof credentials) {
        fail_fn_z(CREATE_ERROR("ForkMany credential size = %d, should be %d",
                               cred_size, static_cast<int>(sizeof credentials)));
      }
    }
    first_time = false;

    // Is the next command another simple fork command? (Also refreshes the nice name.)
    bool simple = false;
    {
      ZrsError err;
      if (!zrs_cmdbuf_is_simple_fork(n_buffer, minUid, &simple, &err)) {
        fail_buf(false, err.msg);
      }
    }
    if (!simple) {
      break;
    }
  } while (true);
  ALOGW("forkRepeatedly terminated due to non-simple command");
  zrs_cmdbuf_log_state(n_buffer);
  zrs_cmdbuf_reset(n_buffer);
  return JNI_FALSE;
}

#define METHOD_NAME(m) com_android_internal_os_ZygoteCommandBuffer_ ## m

static const JNINativeMethod gMethods[] = {
        {"getNativeBuffer", "(I)J", (void *) METHOD_NAME(getNativeBuffer)},
        {"freeNativeBuffer", "(J)V", (void *) METHOD_NAME(freeNativeBuffer)},
        {"insert", "(JLjava/lang/String;)V", (void *) METHOD_NAME(insert)},
        {"nativeNextArg", "(J)Ljava/lang/String;", (void *) METHOD_NAME(nativeNextArg)},
        {"nativeReadFullyAndReset", "(J)V", (void *) METHOD_NAME(nativeReadFullyAndReset)},
        {"nativeGetCount", "(J)I", (void *) METHOD_NAME(nativeGetCount)},
        {"nativeForkRepeatedly", "(JIIILjava/lang/String;)Z",
          (void *) METHOD_NAME(nativeForkRepeatedly)},
};

int register_com_android_internal_os_ZygoteCommandBuffer(JNIEnv* env) {
  return RegisterMethodsOrDie(env, "com/android/internal/os/ZygoteCommandBuffer", gMethods,
                              NELEM(gMethods));
}

}  // namespace android
