//! app_process'in Rust tarafı.
//!
//! Sorumluluk: argüman çözümleme, dalvik-cache hazırlığı, ABI listesi,
//! mod seçimi ve AndroidRuntime'ı başlatma sırası.
//! C++ tarafında sadece `AppRuntime` alt sınıfı kalır (AndroidRuntime bir C++
//! sınıfıdır; sanal metotları Rust'tan override edilemez).
//!
//! BOOTLOOP GÜVENCESİ İÇİN İKİ AŞAMA:
//!   Aşama 1 (geri döndürülebilir): sadece okur/çözümler. Herhangi bir
//!            şüphede `APP_PROCESS_RS_DECLINED` döner -> C++ legacy yolu çalışır.
//!   Aşama 2 (geri dönüşsüz): AndroidRuntime oluşturulur ve başlatılır.
//!            Buradan sonra geriye dönüş yoktur; bu yüzden bu aşama kasıtlı
//!            olarak çok kısa ve yalnızca orijinalin yaptığı çağrıları yapar.
//!
//! Kod panik-içermeyecek şekilde yazıldı (Android'de panic=abort): unwrap,
//! expect, dizi indeksleme yok. Thread OLUŞTURMAYIN: zygote fork öncesi tek
//! thread'li olmalıdır.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod args;

use args::{Mode, Plan};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

/// C++ tarafıyla paylaşılan sabit (app_process_rs.h ile AYNI olmalı).
const APP_PROCESS_RS_DECLINED: c_int = -1000;

const TAG: &[u8] = b"appproc\0";
const PROP_VALUE_MAX: usize = 92; // bionic sys/system_properties.h
const PATH_MAX: usize = 4096;
const EEXIST: c_int = 17;
const AID_ROOT: u32 = 0;
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_WARN: c_int = 5;

extern "C" {
    // ---- C++ köprüsü (app_main.cpp) ----
    fn app_process_shim_create(arg_block_start: *mut c_char, arg_block_length: usize) -> *mut c_void;
    fn app_process_shim_add_option(rt: *mut c_void, option: *const c_char);
    fn app_process_shim_set_class_and_args(
        rt: *mut c_void,
        class_name: *const c_char,
        argc: c_int,
        argv: *const *const c_char,
    );
    fn app_process_shim_set_argv0(rt: *mut c_void, name: *const c_char);
    fn app_process_shim_start(
        rt: *mut c_void,
        class_name: *const c_char,
        argc: c_int,
        argv: *const *const c_char,
        zygote: c_int,
    );

    // ---- libcutils / liblog / bionic (hepsi zaten app_process'e bağlı) ----
    fn property_get(key: *const c_char, value: *mut c_char, default_value: *const c_char) -> c_int;
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    fn __android_log_assert(cond: *const c_char, tag: *const c_char, fmt: *const c_char, ...) -> !;
    fn getenv(name: *const c_char) -> *const c_char;
    fn mkdir(path: *const c_char, mode: u32) -> c_int;
    fn chown(path: *const c_char, owner: u32, group: u32) -> c_int;
    fn chmod(path: *const c_char, mode: u32) -> c_int;
    fn __errno() -> *mut c_int;
    fn strerror(errnum: c_int) -> *const c_char;
}

// ------------------------------------------------------------------ log ---

fn cbytes(msg: &str) -> Vec<u8> {
    let mut b: Vec<u8> = msg.bytes().filter(|&c| c != 0).collect();
    b.push(0);
    b
}

fn log(prio: c_int, msg: &str) {
    let b = cbytes(msg);
    // SAFETY: TAG ve b NUL sonlandırılmış.
    unsafe {
        __android_log_write(prio, TAG.as_ptr().cast(), b.as_ptr().cast());
    }
}

/// LOG_ALWAYS_FATAL karşılığı: loglar, abort message'ı ayarlar, abort eder.
fn fatal(msg: &str) -> ! {
    let b = cbytes(msg);
    // SAFETY: NUL sonlandırılmış tag/format/argüman; fonksiyon geri dönmez.
    unsafe {
        __android_log_assert(
            std::ptr::null(),
            TAG.as_ptr().cast(),
            b"%s\0".as_ptr().cast(),
            b.as_ptr(),
        )
    }
}

// ------------------------------------------------------- yardımcılar ------

fn to_cstrings(v: &[Vec<u8>]) -> Option<Vec<CString>> {
    v.iter().map(|b| CString::new(b.as_slice()).ok()).collect()
}

fn ptrs(v: &[CString]) -> Vec<*const c_char> {
    v.iter().map(|c| c.as_ptr()).collect()
}

/// property_get(prop, buf, NULL) == 0 ise None (orijinalde fatal; biz
/// Aşama 1'de olduğumuz için legacy'ye bırakırız, o fatal'i kendisi üretir).
fn read_abi_list(prop_name: &CStr) -> Option<Vec<u8>> {
    let mut buf = [0u8; PROP_VALUE_MAX];
    // SAFETY: buf PROP_VALUE_MAX bayt; property_get NUL ile sonlandırır.
    let n = unsafe {
        property_get(prop_name.as_ptr(), buf.as_mut_ptr().cast(), std::ptr::null())
    };
    if n <= 0 {
        return None;
    }
    // SAFETY: buf, property_get tarafından NUL sonlandırıldı.
    let s = unsafe { CStr::from_ptr(buf.as_ptr().cast()) };
    Some(s.to_bytes().to_vec())
}

fn errno_string() -> String {
    // SAFETY: __errno() geçerli thread-local işaretçi döner; strerror NUL'lu döner.
    unsafe {
        let e = *__errno();
        let p = strerror(e);
        if p.is_null() {
            return format!("errno {}", e);
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// maybeCreateDalvikCache() — orijinalle AYNI adımlar, AYNI fatal davranışı.
fn maybe_create_dalvik_cache(abi_string: &[u8]) {
    // SAFETY: sabit NUL'lu isim.
    let root = unsafe { getenv(b"ANDROID_DATA\0".as_ptr().cast()) };
    if root.is_null() {
        fatal("ANDROID_DATA environment variable unset");
    }
    // SAFETY: getenv NUL'lu string döner.
    let root = unsafe { CStr::from_ptr(root) }.to_bytes();

    let mut path: Vec<u8> = Vec::with_capacity(root.len() + abi_string.len() + 16);
    path.extend_from_slice(root);
    path.extend_from_slice(b"/dalvik-cache/");
    path.extend_from_slice(abi_string);
    if path.len() >= PATH_MAX {
        fatal("Error constructing dalvik cache : path too long");
    }
    let path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => fatal("Error constructing dalvik cache : embedded NUL"),
    };
    let disp = path.to_string_lossy().into_owned();

    // SAFETY: path NUL'lu.
    let result = unsafe { mkdir(path.as_ptr(), 0o711) };
    if result < 0 {
        // SAFETY: __errno geçerli.
        let e = unsafe { *__errno() };
        if e != EEXIST {
            fatal(&format!("Error creating cache dir {} : {}", disp, errno_string()));
        }
    }

    // Dizin daha geniş izinle/başka sahiple var olabilir: her zaman düzelt.
    // SAFETY: path NUL'lu.
    if unsafe { chown(path.as_ptr(), AID_ROOT, AID_ROOT) } < 0 {
        fatal(&format!("Error changing dalvik-cache ownership : {}", errno_string()));
    }
    // SAFETY: path NUL'lu.
    if unsafe { chmod(path.as_ptr(), 0o711) } < 0 {
        fatal(&format!("Error changing dalvik-cache permissions : {}", errno_string()));
    }
}

/// Aşama 1 çıktısı: aşama 2'nin ihtiyaç duyduğu HER ŞEY hazır ve sahipli.
struct Prepared {
    plan: Plan,
    mode: Mode,
    vm_options: Vec<CString>,
    nice_name: Option<CString>,
    class_name: Option<CString>, // Tool modunda
    rest: Vec<CString>,          // Tool modunda sınıfa giden argümanlar
    start_args: Vec<CString>,    // RuntimeInit / ZygoteInit args
    arg_block_start: *mut c_char,
    arg_block_len: usize,
}

/// Aşama 1. Yan etki YOK. None => legacy'ye bırak.
///
/// # Safety
/// `argv`, `argc` adet geçerli NUL-sonlandırılmış C string işaretçisi içermeli;
/// diğer işaretçiler geçerli C string olmalı.
unsafe fn prepare(
    argc: c_int,
    argv: *const *const c_char,
    abi_list_property: *const c_char,
    zygote_nice_name: *const c_char,
) -> Option<Prepared> {
    if argc < 1 || argv.is_null() || abi_list_property.is_null() || zygote_nice_name.is_null() {
        return None;
    }
    let argc = argc as usize;

    let mut raw: Vec<&[u8]> = Vec::with_capacity(argc);
    for k in 0..argc {
        // SAFETY: k < argc.
        let p = unsafe { *argv.add(k) };
        if p.is_null() {
            return None;
        }
        // SAFETY: p geçerli C string.
        raw.push(unsafe { CStr::from_ptr(p) }.to_bytes());
    }

    // computeArgBlockSize(): argv[0]'ın başlangıcından son argümanın sonuna
    // (NUL dahil). setArgv0() process adını bu bloğun üstüne yazar.
    // SAFETY: argc >= 1.
    let first_ptr = unsafe { *argv };
    let last_ptr = unsafe { *argv.add(argc - 1) };
    let last_len = raw.last().map(|s| s.len()).unwrap_or(0);
    let end = (last_ptr as usize).checked_add(last_len)?.checked_add(1)?;
    let block_len = end.checked_sub(first_ptr as usize)?;
    if block_len == 0 {
        return None;
    }

    // SAFETY: doğrulandı.
    let nice = unsafe { CStr::from_ptr(zygote_nice_name) }.to_bytes();
    let plan = args::parse(raw.get(1..).unwrap_or(&[]), nice);
    let mode = plan.mode();
    if mode == Mode::Unsupported {
        return None;
    }

    let vm_options = to_cstrings(&plan.vm_options)?;
    let nice_name = if plan.nice_name.is_empty() {
        None
    } else {
        Some(CString::new(plan.nice_name.as_slice()).ok()?)
    };

    let (class_name, rest, start_args) = match mode {
        Mode::Tool => (
            Some(CString::new(plan.class_name.as_slice()).ok()?),
            to_cstrings(&plan.rest)?,
            to_cstrings(&plan.tool_start_args())?,
        ),
        Mode::Zygote => {
            // SAFETY: doğrulandı.
            let prop = unsafe { CStr::from_ptr(abi_list_property) };
            let abi_list = read_abi_list(prop)?;
            (None, Vec::new(), to_cstrings(&plan.zygote_start_args(&abi_list))?)
        }
        Mode::Unsupported => return None,
    };

    Some(Prepared {
        plan,
        mode,
        vm_options,
        nice_name,
        class_name,
        rest,
        start_args,
        arg_block_start: first_ptr as *mut c_char,
        arg_block_len: block_len,
    })
}

/// Rust giriş noktası. Geri dönüş:
///  * `APP_PROCESS_RS_DECLINED` -> hiçbir yan etki olmadı, C++ legacy yolunu çalıştır
///  * diğer        -> process çıkış kodu (runtime.start bittiğinde 0)
///
/// # Safety
/// `argv/argc`: C `main`'e gelen orijinal argv. Diğer üç işaretçi geçerli,
/// NUL sonlandırılmış C string olmalı.
#[no_mangle]
pub unsafe extern "C" fn app_process_rs_main(
    argc: c_int,
    argv: *const *const c_char,
    abi_string: *const c_char,
    abi_list_property: *const c_char,
    zygote_nice_name: *const c_char,
) -> c_int {
    if abi_string.is_null() {
        return APP_PROCESS_RS_DECLINED;
    }

    // ============ AŞAMA 1: geri döndürülebilir ============
    // SAFETY: çağıran sözleşmesi.
    let prep = match unsafe { prepare(argc, argv, abi_list_property, zygote_nice_name) } {
        Some(p) => p,
        None => {
            log(ANDROID_LOG_WARN, "rust path: unsupported/odd command line, declining to legacy");
            return APP_PROCESS_RS_DECLINED;
        }
    };
    // SAFETY: doğrulandı.
    let abi_string: Vec<u8> = unsafe { CStr::from_ptr(abi_string) }.to_bytes().to_vec();

    // ============ AŞAMA 2: geri dönüşsüz (orijinalle aynı sıra) ============
    // 1) AppRuntime runtime(argv[0], computeArgBlockSize(...))
    // SAFETY: arg bloğu C main'in orijinal argv'sidir ve process boyunca yaşar.
    let rt = unsafe { app_process_shim_create(prep.arg_block_start, prep.arg_block_len) };
    if rt.is_null() {
        fatal("app_process: failed to create AppRuntime");
    }

    // 2) VM seçenekleri (strdup'u C++ tarafı yapar)
    for opt in &prep.vm_options {
        // SAFETY: rt geçerli, opt NUL'lu.
        unsafe { app_process_shim_add_option(rt, opt.as_ptr()) };
    }

    // 3) mod'a özgü hazırlık
    match prep.mode {
        Mode::Tool => {
            if let Some(cls) = &prep.class_name {
                let rest = ptrs(&prep.rest);
                // SAFETY: rest.len() adet geçerli C string.
                unsafe {
                    app_process_shim_set_class_and_args(
                        rt,
                        cls.as_ptr(),
                        rest.len() as c_int,
                        rest.as_ptr(),
                    )
                };
            }
        }
        Mode::Zygote => maybe_create_dalvik_cache(&abi_string),
        Mode::Unsupported => {}
    }

    // 4) process adı
    if let Some(n) = &prep.nice_name {
        // SAFETY: rt geçerli.
        unsafe { app_process_shim_set_argv0(rt, n.as_ptr()) };
    }

    // 5) başlat
    log(
        ANDROID_LOG_INFO,
        if prep.plan.zygote { "rust path: starting ZygoteInit" } else { "rust path: starting RuntimeInit" },
    );
    let start_args = ptrs(&prep.start_args);
    let (entry, zygote): (&[u8], c_int) = match prep.mode {
        Mode::Zygote => (b"com.android.internal.os.ZygoteInit\0", 1),
        _ => (b"com.android.internal.os.RuntimeInit\0", 0),
    };
    // SAFETY: rt geçerli; start_args.len() adet geçerli C string.
    unsafe {
        app_process_shim_start(
            rt,
            entry.as_ptr().cast(),
            start_args.len() as c_int,
            start_args.as_ptr(),
            zygote,
        )
    };
    // rt KASITLI olarak sızdırılır (orijinalde yığın nesnesi; gCurRuntime,
    // binder thread'leri ve exit handler'ları process sonuna kadar kullanabilir).
    0
}
