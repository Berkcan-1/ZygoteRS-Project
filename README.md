# ⚡ ZygoteRS

**A High-Safety, Zero-Panic Rust Startup Engine for Android's `app_process` & Zygote**

`ZygoteRS` re-imagines the critical startup phase of Android's core application launcher (`app_process`) in **Rust**. Positioned at the very foundation of the Android Runtime (ART), `ZygoteRS` parses boot configurations, pre-configures environment states, and seamlessly hands over execution to `ZygoteInit` and `RuntimeInit` with mathematical memory safety and zero runtime panics.

---

## 🚀 Key Features

### 🛡️ Two-Stage Fallback Architecture
Designed with a strict fail-safe strategy to ensure zero bootloops under any condition:
* **Stage 1 (Reversible & Side-Effect Free):** Command-line arguments, environment parameters, and system properties are validated in a strictly read-only phase. Any parsing ambiguity, unexpected argument, or edge-case safely returns `APP_PROCESS_RS_DECLINED`[cite: 1, 6], seamlessly falling back to the legacy C++ execution path without mutating system state[cite: 5, 6].
* **Stage 2 (Irreversible Runtime Init):** Once Stage 1 guarantees complete parameter validity, control is passed to `AndroidRuntime` via ultra-thin C-ABI shims[cite: 1, 6] to jump into `ZygoteInit` or `RuntimeInit`[cite: 6].

### 🔒 Zero-Panic & Memory Safety
* **No-Panic Policy:** Enforced via strict linting rules (`#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]`)[cite: 6]. Prevents early startup process crashes (`panic=abort`) entirely[cite: 6].
* **Safe Argument Block Allocation:** Calculates process name argument block overlays (`argv[0]`) using bounds-checked arithmetic (`checked_add`, `checked_sub`)[cite: 6]. Eliminates buffer overflows and dangling pointers during `setArgv0` operations[cite: 6].

### 🧵 Single-Threaded Pre-Fork Guarantee
* Respects Android Zygote's primary invariant: **Strictly avoids spawning background threads prior to process fork**[cite: 6], preserving clean POSIX `fork()` behavior across all spawned Android processes.

### 🧪 Isolated & Host-Testable CLI Parser (`args.rs`)
* Decoupled from system C libraries and global state[cite: 7].
* Fully testable on the host machine (`rust_test_host`) against real-world Android initialization scripts (e.g., `init.zygote64.rc`, `am`, `pm`, `--application`) prior to flashing on target devices[cite: 7].

### 🎛️ Dynamic Feature Flag & A/B Testing Control
* Switch runtime execution engines dynamically via system property (`debug.aerolon.zygote_rs`) without rebuilding:
  ```bash
  # Force enable ZygoteRS path
  adb shell su -c setprop debug.aerolon.zygote_rs 1

  # Force fallback to legacy C++ path
  adb shell su -c setprop debug.aerolon.zygote_rs 0
