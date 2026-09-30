/*
 * Main entry of app process.
 *
 * Starts the interpreted runtime, then starts up the application.
 *
 * Rust-assisted variant (Android 13):
 *   - AppRuntime (a C++ subclass of AndroidRuntime) stays here, unchanged.
 *   - Command line parsing, dalvik-cache setup, ABI list lookup and start-up
 *     sequencing live in Rust (rs/lib.rs, rs/args.rs).
 *   - The ORIGINAL C++ main() body is kept verbatim as legacy_main(). It runs
 *     when the Rust path is switched off, and whenever the Rust path declines
 *     (before it has changed anything) via APP_PROCESS_RS_DECLINED.
 */

#define LOG_TAG "appproc"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <android-base/macros.h>
#include <binder/IPCThreadState.h>
#include <hwbinder/IPCThreadState.h>
#include <utils/Log.h>
#include <cutils/memory.h>
#include <cutils/properties.h>
#include <cutils/trace.h>
#include <android_runtime/AndroidRuntime.h>
#include <private/android_filesystem_config.h>  // for AID_SYSTEM

#include "app_process_rs.h"

/*
 * Compile-time default for the Rust path. Set with
 *   cflags: ["-DAPP_PROCESS_RS_DEFAULT=1"]
 * once the Rust path has been validated. Keep 0 while bringing it up.
 */
#ifndef APP_PROCESS_RS_DEFAULT
#define APP_PROCESS_RS_DEFAULT 0
#endif

/*
 * Runtime override ("0" = force legacy, "1" = force Rust, anything else /
 * unset / unreadable = compile-time default). debug.* properties are not
 * persistent: a power cycle always returns to the compile-time default.
 */
static const char kRustSwitchProp[] = "debug.aerolon.zygote_rs";

namespace android {

static void app_usage()
{
    fprintf(stderr,
        "Usage: app_process [java-options] cmd-dir start-class-name [options]\n");
}

class AppRuntime : public AndroidRuntime
{
public:
    AppRuntime(char* argBlockStart, const size_t argBlockLength)
        : AndroidRuntime(argBlockStart, argBlockLength)
        , mClass(NULL)
    {
    }

    void setClassNameAndArgs(const String8& className, int argc, char * const *argv) {
        mClassName = className;
        for (int i = 0; i < argc; ++i) {
             mArgs.add(String8(argv[i]));
        }
    }

    virtual void onVmCreated(JNIEnv* env)
    {
        if (mClassName.isEmpty()) {
            return; // Zygote. Nothing to do here.
        }

        /*
         * This is a little awkward because the JNI FindClass call uses the
         * class loader associated with the native method we're executing in.
         * If called in onStarted (from RuntimeInit.finishInit because we're
         * launching "am", for example), FindClass would see that we're calling
         * from a boot class' native method, and so wouldn't look for the class
         * we're trying to look up in CLASSPATH. Unfortunately it needs to,
         * because the "am" classes are not boot classes.
         *
         * The easiest fix is to call FindClass here, early on before we start
         * executing boot class Java code and thereby deny ourselves access to
         * non-boot classes.
         */
        char* slashClassName = toSlashClassName(mClassName.string());
        mClass = env->FindClass(slashClassName);
        if (mClass == NULL) {
            ALOGE("ERROR: could not find class '%s'\n", mClassName.string());
        }
        free(slashClassName);

        mClass = reinterpret_cast<jclass>(env->NewGlobalRef(mClass));
    }

    virtual void onStarted()
    {
        sp<ProcessState> proc = ProcessState::self();
        ALOGV("App process: starting thread pool.\n");
        proc->startThreadPool();

        AndroidRuntime* ar = AndroidRuntime::getRuntime();
        ar->callMain(mClassName, mClass, mArgs);

        IPCThreadState::self()->stopProcess();
        hardware::IPCThreadState::self()->stopProcess();
    }

    virtual void onZygoteInit()
    {
        sp<ProcessState> proc = ProcessState::self();
        ALOGV("App process: starting thread pool.\n");
        proc->startThreadPool();
    }

    virtual void onExit(int code)
    {
        if (mClassName.isEmpty()) {
            // if zygote
            IPCThreadState::self()->stopProcess();
            hardware::IPCThreadState::self()->stopProcess();
        }

        AndroidRuntime::onExit(code);
    }


    String8 mClassName;
    Vector<String8> mArgs;
    jclass mClass;
};

}

using namespace android;

/* ------------------------------------------------------------------------
 * C bridge used by the Rust start-up logic. Each function is a thin wrapper
 * around exactly one AppRuntime/AndroidRuntime call the original main() made.
 * ---------------------------------------------------------------------- */
extern "C" {

void* app_process_shim_create(char* argBlockStart, size_t argBlockLength) {
    // Intentionally never freed: the original kept it on main()'s stack, and
    // gCurRuntime / binder threads / exit handlers may touch it until exit.
    return new AppRuntime(argBlockStart, argBlockLength);
}

void app_process_shim_add_option(void* rt, const char* option) {
    // The runtime keeps the pointer, so it must outlive us (same as the
    // original strdup() in main(), which was never freed either).
    static_cast<AppRuntime*>(rt)->addOption(strdup(option));
}

void app_process_shim_set_class_and_args(void* rt, const char* className,
                                         int argc, const char* const* argv) {
    static_cast<AppRuntime*>(rt)->setClassNameAndArgs(
            String8(className), argc, const_cast<char* const*>(argv));
}

void app_process_shim_set_argv0(void* rt, const char* niceName) {
    static_cast<AppRuntime*>(rt)->setArgv0(niceName, true /* setProcName */);
}

void app_process_shim_start(void* rt, const char* className, int argc,
                            const char* const* argv, int zygote) {
    Vector<String8> args;
    for (int i = 0; i < argc; ++i) {
        args.add(String8(argv[i]));
    }
    static_cast<AppRuntime*>(rt)->start(className, args, zygote != 0);
}

}  // extern "C"

/* ------------------------------------------------------------------------
 * LEGACY PATH: the original app_main.cpp, unchanged except that main() was
 * renamed legacy_main() and got an explicit `return 0;` at the end.
 * ---------------------------------------------------------------------- */

static size_t computeArgBlockSize(int argc, char* const argv[]) {
    // TODO: This assumes that all arguments are allocated in
    // contiguous memory. There isn't any documented guarantee
    // that this is the case, but this is how the kernel does it
    // (see fs/exec.c).
    //
    // Also note that this is a constant for "normal" android apps.
    // Since they're forked from zygote, the size of their command line
    // is the size of the zygote command line.
    //
    // We change the process name of the process by over-writing
    // the start of the argument block (argv[0]) with the new name of
    // the process, so we'd mysteriously start getting truncated process
    // names if the zygote command line decreases in size.
    uintptr_t start = reinterpret_cast<uintptr_t>(argv[0]);
    uintptr_t end = reinterpret_cast<uintptr_t>(argv[argc - 1]);
    end += strlen(argv[argc - 1]) + 1;
    return (end - start);
}

static void maybeCreateDalvikCache() {
    const char* androidRoot = getenv("ANDROID_DATA");
    LOG_ALWAYS_FATAL_IF(androidRoot == NULL, "ANDROID_DATA environment variable unset");

    char dalvikCacheDir[PATH_MAX];
    const int numChars = snprintf(dalvikCacheDir, PATH_MAX,
            "%s/dalvik-cache/" ABI_STRING, androidRoot);
    LOG_ALWAYS_FATAL_IF((numChars >= PATH_MAX || numChars < 0),
            "Error constructing dalvik cache : %s", strerror(errno));

    int result = mkdir(dalvikCacheDir, 0711);
    LOG_ALWAYS_FATAL_IF((result < 0 && errno != EEXIST),
            "Error creating cache dir %s : %s", dalvikCacheDir, strerror(errno));

    // We always perform these steps because the directory might
    // already exist, with wider permissions and a different owner
    // than we'd like.
    result = chown(dalvikCacheDir, AID_ROOT, AID_ROOT);
    LOG_ALWAYS_FATAL_IF((result < 0), "Error changing dalvik-cache ownership : %s", strerror(errno));

    result = chmod(dalvikCacheDir, 0711);
    LOG_ALWAYS_FATAL_IF((result < 0),
            "Error changing dalvik-cache permissions : %s", strerror(errno));
}

#if defined(__LP64__)
static const char ABI_LIST_PROPERTY[] = "ro.product.cpu.abilist64";
static const char ZYGOTE_NICE_NAME[] = "zygote64";
#else
static const char ABI_LIST_PROPERTY[] = "ro.product.cpu.abilist32";
static const char ZYGOTE_NICE_NAME[] = "zygote";
#endif

static int legacy_main(int argc, char* const argv[])
{
    if (!LOG_NDEBUG) {
      String8 argv_String;
      for (int i = 0; i < argc; ++i) {
        argv_String.append("\"");
        argv_String.append(argv[i]);
        argv_String.append("\" ");
      }
      ALOGV("app_process main with argv: %s", argv_String.string());
    }

    AppRuntime runtime(argv[0], computeArgBlockSize(argc, argv));
    // Process command line arguments
    // ignore argv[0]
    argc--;
    argv++;

    // Everything up to '--' or first non '-' arg goes to the vm.
    //
    // The first argument after the VM args is the "parent dir", which
    // is currently unused.
    //
    // After the parent dir, we expect one or more the following internal
    // arguments :
    //
    // --zygote : Start in zygote mode
    // --start-system-server : Start the system server.
    // --application : Start in application (stand alone, non zygote) mode.
    // --nice-name : The nice name for this process.
    //
    // For non zygote starts, these arguments will be followed by
    // the main class name. All remaining arguments are passed to
    // the main method of this class.
    //
    // For zygote starts, all remaining arguments are passed to the zygote.
    // main function.
    //
    // Note that we must copy argument string values since we will rewrite the
    // entire argument block when we apply the nice name to argv0.
    //
    // As an exception to the above rule, anything in "spaced commands"
    // goes to the vm even though it has a space in it.
    const char* spaced_commands[] = { "-cp", "-classpath" };
    // Allow "spaced commands" to be succeeded by exactly 1 argument (regardless of -s).
    bool known_command = false;

    int i;
    for (i = 0; i < argc; i++) {
        if (known_command == true) {
          runtime.addOption(strdup(argv[i]));
          // The static analyzer gets upset that we don't ever free the above
          // string. Since the allocation is from main, leaking it doesn't seem
          // problematic. NOLINTNEXTLINE
          ALOGV("app_process main add known option '%s'", argv[i]);
          known_command = false;
          continue;
        }

        for (int j = 0;
             j < static_cast<int>(sizeof(spaced_commands) / sizeof(spaced_commands[0]));
             ++j) {
          if (strcmp(argv[i], spaced_commands[j]) == 0) {
            known_command = true;
            ALOGV("app_process main found known command '%s'", argv[i]);
          }
        }

        if (argv[i][0] != '-') {
            break;
        }
        if (argv[i][1] == '-' && argv[i][2] == 0) {
            ++i; // Skip --.
            break;
        }

        runtime.addOption(strdup(argv[i]));
        // The static analyzer gets upset that we don't ever free the above
        // string. Since the allocation is from main, leaking it doesn't seem
        // problematic. NOLINTNEXTLINE
        ALOGV("app_process main add option '%s'", argv[i]);
    }

    // Parse runtime arguments.  Stop at first unrecognized option.
    bool zygote = false;
    bool startSystemServer = false;
    bool application = false;
    String8 niceName;
    String8 className;

    ++i;  // Skip unused "parent dir" argument.
    while (i < argc) {
        const char* arg = argv[i++];
        if (strcmp(arg, "--zygote") == 0) {
            zygote = true;
            niceName = ZYGOTE_NICE_NAME;
        } else if (strcmp(arg, "--start-system-server") == 0) {
            startSystemServer = true;
        } else if (strcmp(arg, "--application") == 0) {
            application = true;
        } else if (strncmp(arg, "--nice-name=", 12) == 0) {
            niceName.setTo(arg + 12);
        } else if (strncmp(arg, "--", 2) != 0) {
            className.setTo(arg);
            break;
        } else {
            --i;
            break;
        }
    }

    Vector<String8> args;
    if (!className.isEmpty()) {
        // We're not in zygote mode, the only argument we need to pass
        // to RuntimeInit is the application argument.
        //
        // The Remainder of args get passed to startup class main(). Make
        // copies of them before we overwrite them with the process name.
        args.add(application ? String8("application") : String8("tool"));
        runtime.setClassNameAndArgs(className, argc - i, argv + i);

        if (!LOG_NDEBUG) {
          String8 restOfArgs;
          char* const* argv_new = argv + i;
          int argc_new = argc - i;
          for (int k = 0; k < argc_new; ++k) {
            restOfArgs.append("\"");
            restOfArgs.append(argv_new[k]);
            restOfArgs.append("\" ");
          }
          ALOGV("Class name = %s, args = %s", className.string(), restOfArgs.string());
        }
    } else {
        // We're in zygote mode.
        maybeCreateDalvikCache();

        if (startSystemServer) {
            args.add(String8("start-system-server"));
        }

        char prop[PROP_VALUE_MAX];
        if (property_get(ABI_LIST_PROPERTY, prop, NULL) == 0) {
            LOG_ALWAYS_FATAL("app_process: Unable to determine ABI list from property %s.",
                ABI_LIST_PROPERTY);
            return 11;
        }

        String8 abiFlag("--abi-list=");
        abiFlag.append(prop);
        args.add(abiFlag);

        // In zygote mode, pass all remaining arguments to the zygote
        // main() method.
        for (; i < argc; ++i) {
            args.add(String8(argv[i]));
        }
    }

    if (!niceName.isEmpty()) {
        runtime.setArgv0(niceName.string(), true /* setProcName */);
    }

    if (zygote) {
        runtime.start("com.android.internal.os.ZygoteInit", args, zygote);
    } else if (className) {
        runtime.start("com.android.internal.os.RuntimeInit", args, zygote);
    } else {
        fprintf(stderr, "Error: no class name or --zygote supplied.\n");
        app_usage();
        LOG_ALWAYS_FATAL("app_process: no class name or --zygote supplied.");
    }

    return 0;
}

/* ------------------------------------------------------------------------
 * Path selection.
 * ---------------------------------------------------------------------- */

static bool rustPathEnabled() {
    char value[PROP_VALUE_MAX];
    if (property_get(kRustSwitchProp, value, "") > 0) {
        if (strcmp(value, "1") == 0) return true;
        if (strcmp(value, "0") == 0) return false;
    }
    return APP_PROCESS_RS_DEFAULT != 0;
}

int main(int argc, char* const argv[])
{
    if (argc > 0 && argv != nullptr && argv[0] != nullptr && rustPathEnabled()) {
        ALOGI("app_process: Rust start-up path selected");
        int rc = app_process_rs_main(argc, argv, ABI_STRING, ABI_LIST_PROPERTY,
                                     ZYGOTE_NICE_NAME);
        if (rc != APP_PROCESS_RS_DECLINED) {
            return rc;
        }
        ALOGW("app_process: Rust path declined, using legacy path");
    }
    return legacy_main(argc, argv);
}
