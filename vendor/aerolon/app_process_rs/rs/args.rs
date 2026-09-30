//! app_process komut satırı çözümleyici.
//!
//! `app_main.cpp` içindeki `main()` ayrıştırmasının BİREBİR portudur.
//! Bilerek SAF tutuldu: libc yok, global durum yok, panik yok, yan etki yok.
//! Bu sayede host üzerinde (`rust_test_host`) gerçek zygote/am komut
//! satırlarıyla test edilebilir; cihaza flashlamadan doğrulanır.
#![cfg_attr(test, allow(dead_code))]

/// Çözümlenmiş komut satırı. Tüm alanlar SAHİPLİDİR (argv belleğine referans
/// tutmaz): `setArgv0` argv bloğunu üzerine yazdığı için bu şarttır.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// VM'e giden seçenekler (sıra önemlidir).
    pub vm_options: Vec<Vec<u8>>,
    pub zygote: bool,
    pub start_system_server: bool,
    pub application: bool,
    /// Boş = ayarlanmamış.
    pub nice_name: Vec<u8>,
    /// Boş = sınıf adı yok (zygote modu).
    pub class_name: Vec<u8>,
    /// Sınıf adından (veya --zygote bayraklarından) sonra kalan argümanlar.
    pub rest: Vec<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    /// `--zygote` var, sınıf adı yok.
    Zygote,
    /// `--zygote` yok, sınıf adı var (am, pm, settings ...).
    Tool,
    /// Orijinal kodun tuhaf/ölü dallarına düşen durumlar. Rust bunları
    /// ÜSTLENMEZ; legacy C++ yoluna bırakır (davranış birebir aynı kalır).
    Unsupported,
}

/// `argv` = orijinal argv'nin argv[0] ATILMIŞ hali (`argc--; argv++` sonrası).
/// `zygote_nice_name` = "zygote64" / "zygote" (C++ tarafı verir).
pub fn parse(argv: &[&[u8]], zygote_nice_name: &[u8]) -> Plan {
    let mut plan = Plan::default();
    let mut i = 0usize;

    // --- VM argümanları: '--' ya da ilk '-' ile başlamayan argümana kadar ---
    // "-cp" / "-classpath" tam olarak 1 argüman daha sahiplenir.
    let mut known_command = false;
    while let Some(&arg) = argv.get(i) {
        if known_command {
            plan.vm_options.push(arg.to_vec());
            known_command = false;
            i += 1;
            continue;
        }
        if arg == b"-cp" || arg == b"-classpath" {
            known_command = true;
        }
        if arg.first() != Some(&b'-') {
            break;
        }
        if arg == b"--" {
            i += 1; // '--' atla
            break;
        }
        plan.vm_options.push(arg.to_vec());
        i += 1;
    }

    // --- kullanılmayan "parent dir" argümanı ---
    i += 1;

    // --- runtime argümanları: tanınmayan ilk seçenekte dur ---
    while let Some(&arg) = argv.get(i) {
        i += 1;
        if arg == b"--zygote" {
            plan.zygote = true;
            plan.nice_name = zygote_nice_name.to_vec();
        } else if arg == b"--start-system-server" {
            plan.start_system_server = true;
        } else if arg == b"--application" {
            plan.application = true;
        } else if let Some(name) = arg.strip_prefix(b"--nice-name=") {
            plan.nice_name = name.to_vec();
        } else if !arg.starts_with(b"--") {
            plan.class_name = arg.to_vec();
            break;
        } else {
            i -= 1; // tanınmayan "--xxx": zygote'a aynen geçecek
            break;
        }
    }

    plan.rest = argv
        .get(i..)
        .unwrap_or(&[])
        .iter()
        .map(|a| a.to_vec())
        .collect();
    plan
}

impl Plan {
    pub fn mode(&self) -> Mode {
        match (self.zygote, self.class_name.is_empty()) {
            (true, true) => Mode::Zygote,
            (false, false) => Mode::Tool,
            _ => Mode::Unsupported,
        }
    }

    /// Tool modunda RuntimeInit'e giden args.
    pub fn tool_start_args(&self) -> Vec<Vec<u8>> {
        let first: &[u8] = if self.application {
            b"application"
        } else {
            b"tool"
        };
        vec![first.to_vec()]
    }

    /// Zygote modunda ZygoteInit'e giden args.
    pub fn zygote_start_args(&self, abi_list: &[u8]) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = Vec::new();
        if self.start_system_server {
            v.push(b"start-system-server".to_vec());
        }
        let mut abi = b"--abi-list=".to_vec();
        abi.extend_from_slice(abi_list);
        v.push(abi);
        v.extend(self.rest.iter().cloned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NICE: &[u8] = b"zygote64";

    fn p(argv: &[&str]) -> Plan {
        let raw: Vec<&[u8]> = argv.iter().map(|s| s.as_bytes()).collect();
        parse(&raw, NICE)
    }
    fn b(v: &[&str]) -> Vec<Vec<u8>> {
        v.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    // Android 13 init.zygote64.rc birincil zygote
    #[test]
    fn primary_zygote() {
        let plan = p(&[
            "-Xzygote",
            "/system/bin",
            "--zygote",
            "--start-system-server",
            "--socket-name=zygote",
        ]);
        assert_eq!(plan.vm_options, b(&["-Xzygote"]));
        assert!(plan.zygote && plan.start_system_server && !plan.application);
        assert_eq!(plan.nice_name, b"zygote64");
        assert!(plan.class_name.is_empty());
        assert_eq!(plan.rest, b(&["--socket-name=zygote"]));
        assert_eq!(plan.mode(), Mode::Zygote);
        assert_eq!(
            plan.zygote_start_args(b"arm64-v8a"),
            b(&[
                "start-system-server",
                "--abi-list=arm64-v8a",
                "--socket-name=zygote"
            ])
        );
    }

    // init.zygote64_32.rc ikincil zygote
    #[test]
    fn secondary_zygote() {
        let plan = p(&[
            "-Xzygote",
            "/system/bin",
            "--zygote",
            "--socket-name=zygote_secondary",
            "--enable-lazy-preload",
        ]);
        assert!(plan.zygote && !plan.start_system_server);
        assert_eq!(
            plan.rest,
            b(&["--socket-name=zygote_secondary", "--enable-lazy-preload"])
        );
        assert_eq!(
            plan.zygote_start_args(b"armeabi-v7a,armeabi"),
            b(&[
                "--abi-list=armeabi-v7a,armeabi",
                "--socket-name=zygote_secondary",
                "--enable-lazy-preload"
            ])
        );
    }

    // `am` betiği: app_process /system/bin com.android.commands.am.Am start ...
    #[test]
    fn tool_mode_am() {
        let plan = p(&[
            "/system/bin",
            "com.android.commands.am.Am",
            "start",
            "-n",
            "x/.Y",
        ]);
        assert!(plan.vm_options.is_empty() && !plan.zygote);
        assert_eq!(plan.class_name, b"com.android.commands.am.Am");
        assert_eq!(plan.rest, b(&["start", "-n", "x/.Y"]));
        assert_eq!(plan.mode(), Mode::Tool);
        assert_eq!(plan.tool_start_args(), b(&["tool"]));
    }

    #[test]
    fn application_flag() {
        let plan = p(&["/system/bin", "--application", "--nice-name=foo", "a.B", "x"]);
        assert!(plan.application);
        assert_eq!(plan.nice_name, b"foo");
        assert_eq!(plan.class_name, b"a.B");
        assert_eq!(plan.tool_start_args(), b(&["application"]));
    }

    #[test]
    fn spaced_commands_take_one_argument() {
        let plan = p(&["-cp", "/a.jar", "-Xfoo", "-classpath", "b.jar", "/system/bin", "C"]);
        assert_eq!(
            plan.vm_options,
            b(&["-cp", "/a.jar", "-Xfoo", "-classpath", "b.jar"])
        );
        assert_eq!(plan.class_name, b"C");
    }

    #[test]
    fn spaced_command_swallows_non_dash_argument() {
        // "-cp" sonrası gelen "-" ile başlamayan argüman da VM'e gider.
        let plan = p(&["-cp", "x", "/system/bin", "C"]);
        assert_eq!(plan.vm_options, b(&["-cp", "x"]));
        assert_eq!(plan.class_name, b"C");
    }

    #[test]
    fn double_dash_terminator() {
        // -Xfoo -- <parent> Cls  ("--" sonrası ilk arg = parent dir, atlanır)
        let plan = p(&["-Xfoo", "--", "/system/bin", "Cls", "z"]);
        assert_eq!(plan.vm_options, b(&["-Xfoo"]));
        assert_eq!(plan.class_name, b"Cls");
        assert_eq!(plan.rest, b(&["z"]));
    }

    #[test]
    fn single_dash_is_a_vm_option() {
        let plan = p(&["-", "/system/bin", "C"]);
        assert_eq!(plan.vm_options, b(&["-"]));
    }

    #[test]
    fn nice_name_order_matches_original() {
        // --zygote sonradan gelirse nice-name'i ezer (orijinal davranış)
        let a = p(&["/system/bin", "--nice-name=foo", "--zygote"]);
        assert_eq!(a.nice_name, b"zygote64");
        // --nice-name sonradan gelirse zygote adını ezer
        let z = p(&["/system/bin", "--zygote", "--nice-name=foo"]);
        assert_eq!(z.nice_name, b"foo");
    }

    #[test]
    fn unknown_double_dash_option_stops_parsing() {
        let plan = p(&["/system/bin", "--zygote", "--weird", "--zygote-again"]);
        assert_eq!(plan.rest, b(&["--weird", "--zygote-again"]));
    }

    #[test]
    fn unsupported_shapes_are_declined() {
        // ne --zygote ne sınıf adı
        assert_eq!(p(&["/system/bin"]).mode(), Mode::Unsupported);
        // hem --zygote hem sınıf adı
        assert_eq!(p(&["/system/bin", "--zygote", "a.B"]).mode(), Mode::Unsupported);
        // boş sınıf adı = "yok" sayılır
        assert_eq!(p(&["/system/bin", ""]).mode(), Mode::Unsupported);
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        p(&[]);
        p(&["-Xfoo"]); // parent dir yok
        p(&["--"]);
        p(&["-cp"]); // "-cp" argümansız
        p(&["", "", ""]);
    }
}
