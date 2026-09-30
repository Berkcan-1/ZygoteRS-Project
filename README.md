Two-Stage Startup Architecture:

Stage 1 (Reversible & Side-Effect Free): Command-line arguments, environment parameters, and system properties are validated in a completely safe, read-only phase. Any parsing error, unknown configuration, or edge-case safely returns APP_PROCESS_RS_DECLINED to fall back to the legacy C++ execution path without touching system state.

Stage 2 (Irreversible Runtime Init): Once fully validated, control is handed over to AndroidRuntime through thin C-ABI shims to launch ZygoteInit or RuntimeInit.

Zero-Panic Guarantee & Memory Safety:

Enforced #![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] lint rules ensure no runtime panics or panic=abort crashes during early process startup.

Checked arithmetic (checked_add, checked_sub) is used for process name argument block allocation calculations, preventing buffer overflows and dangling pointer issues when setArgv0 overwrites argv[0].

Single-Threaded Fork Safety:

Strictly avoids creating threads prior to process fork, preserving Android Zygote's core invariant that pre-fork processes remain single-threaded.

Isolated & Testable CLI Parser (args.rs):

Pure Rust parsing logic isolated from C++ runtime state, allowing direct host-side unit testing (rust_test_host) against real Android boot command-line configurations (init.zygote64.rc, am, pm, etc.).

Zero-Downtime Feature Flag Control:

Dynamic toggle capability via system property (debug.aerolon.zygote_rs) and build-time configuration (APP_PROCESS_RS_DEFAULT) for seamless runtime switching and A/B testing on target devices.
