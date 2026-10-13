//! End-to-end tests of the library API, shaped after the typical use case:
//! merge several static libraries and keep only the API symbols global.
mod common;

use armerge::{ArMerger, ArchiveContents, ArmergeKeepOrRemove, InputLibrary, MergeError, ProcessInputError};
use common::{Target, HOST, *};
use regex::Regex;
use std::{io::Cursor, path::Path, process::Command};
use tempfile::{tempdir, TempDir};
use ArmergeKeepOrRemove::{KeepSymbols, RemoveSymbols};

fn regexes(pattern: &str) -> impl Iterator<Item = Regex> {
    std::iter::once(Regex::new(&format!("^{}{pattern}", sym(""))).unwrap())
}

/// Merges `libtest.a` keeping only the `api_` symbols and returns the output archive
fn keep_api(dir: &TempDir, target: Target) -> std::path::PathBuf {
    init();
    let lib = c_lib(dir.path(), target);
    let out = dir.path().join("out.a");
    ArMerger::new_from_paths([lib], &out)
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("api_"))
        .unwrap();
    assert_eq!(architecture(&out), target.arch());
    out
}

#[test]
fn keep_symbols_hides_everything_else() {
    let dir = tempdir().unwrap();
    let out = keep_api(&dir, HOST);
    let syms = symbols(&out);

    assert_eq!(object_count(&out), 1, "everything is merged into a single object");
    assert_eq!(exported(&syms), [sym("api_fn"), sym("api_version")]);
    for name in ["helper_fn", "helper_data", "weak_fn"] {
        assert!(!syms[&sym(name)].global, "{name} should have been localized");
    }
    assert!(
        !syms.contains_key(&sym("unused_fn")),
        "unreferenced objects are dropped"
    );
    assert!(syms[&sym("malloc")].undefined, "external references stay undefined");
}

#[cfg(target_os = "linux")]
#[test]
fn keep_symbols_aarch64_linux() {
    let dir = tempdir().unwrap();
    let out = keep_api(&dir, Target::Aarch64Linux);
    assert_eq!(exported(&symbols(&out)), [sym("api_fn"), sym("api_version")]);
}

/// Mach-O prepends an underscore to C symbols, so keep regexes must account for it
#[test]
fn macho_symbols_have_leading_underscore() {
    init();
    let dir = tempdir().unwrap();
    let lib = c_lib(dir.path(), Target::Aarch64MacOs);
    let out = dir.path().join("out.a");
    let merge = |pattern: &str| {
        ArMerger::new_from_paths([&lib], &out)
            .unwrap()
            .merge_and_localize(KeepSymbols, std::iter::once(Regex::new(pattern).unwrap()))
    };

    // Without the underscore nothing matches, so every object gets dropped
    assert!(matches!(merge("^api_").unwrap_err(), MergeError::NoObjectsLeft));

    // With it the object is kept; only Apple's ld can finish the merge
    match merge("^_api_") {
        Ok(()) => assert_eq!(exported(&symbols(&out)), ["_api_fn", "_api_version"]),
        Err(MergeError::ExternalToolError { .. }) if cfg!(not(target_os = "macos")) => {},
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn api_regex_across_libraries() {
    init();
    let dir = tempdir().unwrap();
    let d = dir.path();
    let app = compile(
        &d.join("app"),
        "app.c",
        "int parser_parse(int); int vendor_detail(int); int app_create(int x) { return parser_parse(x) + vendor_detail(x); }",
        HOST,
    );
    let parser = compile(
        &d.join("parser"),
        "util.c",
        "int parser_parse(int x) { return x; }",
        HOST,
    );
    let vendor = compile(
        &d.join("vendor"),
        "util.c",
        "int vendor_detail(int x) { return x; }",
        HOST,
    );
    let vendor_unused = compile(
        &d.join("vendor"),
        "extra.c",
        "int vendor_unused(int x) { return x; }",
        HOST,
    );
    let libs = [
        archive(d, "libapp.a", &[app]),
        archive(d, "libparser.a", &[parser]),
        archive(d, "libvendor.a", &[vendor, vendor_unused]),
    ];

    let out = d.join("out.a");
    ArMerger::new_from_paths(libs, &out)
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("(app|parser|store)_.*"))
        .unwrap();
    let syms = symbols(&out);

    assert_eq!(exported(&syms), [sym("app_create"), sym("parser_parse")]);
    assert!(
        !syms[&sym("vendor_detail")].global,
        "dependency pulled in from another library, but hidden"
    );
    assert!(!syms.contains_key(&sym("vendor_unused")));
}

#[test]
fn cxx_comdat_and_personality() {
    init();
    let dir = tempdir().unwrap();
    let d = dir.path();
    let objs = [
        compile(d, "cxx.cpp", include_str!("fixtures/cxx.cpp"), HOST),
        compile(d, "cxx2.cpp", include_str!("fixtures/cxx2.cpp"), HOST),
    ];
    let lib = archive(d, "libcxx.a", &objs);

    let out = d.join("out.a");
    ArMerger::new_from_paths([lib], &out)
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("(app|parser|store)_.*"))
        .unwrap();
    let syms = symbols(&out);

    assert_eq!(
        exported(&syms),
        [
            sym("app_create"),
            sym("parser_parse"),
            sym("store_open"),
            sym("store_size")
        ]
    );
    for name in [
        "_ZN6vendor15internal_detailEi",
        "_Z13shared_inlinei",
        "_Z15shared_templateIiET_S0_",
        "vendor_tls",
    ] {
        assert!(!syms[&sym(name)].global, "{name} should have been localized");
    }
    for name in ["__gxx_personality_v0", "__cxa_throw"] {
        assert!(
            syms[&sym(name)].undefined,
            "{name} must still resolve against the C++ runtime"
        );
    }
}

#[test]
fn unwind_symbols_are_always_kept() {
    init();
    let dir = tempdir().unwrap();
    let d = dir.path();
    let objs = [
        compile(d, "api.c", include_str!("fixtures/api.c"), HOST),
        compile(d, "helper.c", include_str!("fixtures/helper.c"), HOST),
        compile(d, "unused.c", include_str!("fixtures/unused.c"), HOST),
        compile(d, "unwind.c", include_str!("fixtures/unwind.c"), HOST),
    ];
    let lib = archive(d, "libtest.a", &objs);

    let out = d.join("out.a");
    ArMerger::new_from_paths([lib], &out)
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("api_"))
        .unwrap();
    let syms = symbols(&out);

    assert_eq!(
        exported(&syms),
        [
            sym("_Unwind_GetIP"),
            sym("_Unwind_Resume"),
            sym("api_fn"),
            sym("api_version")
        ]
    );
    assert!(!syms.contains_key(&sym("unused_fn")));
}

#[test]
fn remove_symbols() {
    init();
    let dir = tempdir().unwrap();
    let lib = c_lib(dir.path(), HOST);
    let out = dir.path().join("out.a");
    ArMerger::new_from_paths([lib], &out)
        .unwrap()
        .merge_and_localize(RemoveSymbols, regexes("helper_"))
        .unwrap();
    let syms = symbols(&out);

    assert_eq!(object_count(&out), 1);
    assert_eq!(
        exported(&syms),
        [sym("api_fn"), sym("api_version"), sym("unused_data"), sym("unused_fn")]
    );
    assert!(syms[&sym("weak_fn")].global && syms[&sym("weak_fn")].weak);
    for name in ["helper_fn", "helper_data"] {
        assert!(!syms[&sym(name)].global, "{name} should have been localized");
    }
}

#[test]
fn no_objects_left_is_an_error() {
    init();
    let dir = tempdir().unwrap();
    let lib = c_lib(dir.path(), HOST);
    let err = ArMerger::new_from_paths([lib], dir.path().join("out.a"))
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("nothing_matches"))
        .unwrap_err();
    assert!(matches!(err, MergeError::NoObjectsLeft), "{err}");
}

#[test]
fn merge_simple_repacks_all_objects() {
    init();
    let dir = tempdir().unwrap();
    let lib = c_lib(dir.path(), HOST);
    let out = dir.path().join("out.a");
    ArMerger::new_from_paths([lib], &out).unwrap().merge_simple().unwrap();
    let syms = symbols(&out);

    assert_eq!(object_count(&out), 3);
    assert_eq!(
        exported(&syms),
        [
            sym("api_fn"),
            sym("api_version"),
            sym("helper_data"),
            sym("helper_fn"),
            sym("unused_data"),
            sym("unused_fn")
        ]
    );
}

#[test]
fn object_order_controls_link_order() {
    init();
    let dir = tempdir().unwrap();
    let lib = c_lib(dir.path(), HOST);
    let address = |name: &str, order: [&str; 2]| {
        let out = dir.path().join(format!("out-{}.a", order[0]));
        ArMerger::new_from_paths([&lib], &out)
            .unwrap()
            .merge_and_localize_ordered(KeepSymbols, regexes("api_"), order.map(|o| format!("libtest.a@{o}.o")))
            .unwrap();
        symbols(&out)[&sym(name)].address
    };

    assert!(address("helper_fn", ["helper", "api"]) < address("api_fn", ["helper", "api"]));
    assert!(address("helper_fn", ["api", "helper"]) > address("api_fn", ["api", "helper"]));
}

/// Archives from a real `ar` carry a symbol index and a long-name table, like CMake-built libraries
#[test]
fn real_ar_archive_with_long_member_names() {
    init();
    let dir = tempdir().unwrap();
    let d = dir.path();
    let objs = [
        compile(d, "api_component_long_name.c", include_str!("fixtures/api.c"), HOST),
        compile(
            d,
            "helper_component_long_name.c",
            include_str!("fixtures/helper.c"),
            HOST,
        ),
    ];
    let lib = d.join("libreal.a");
    assert!(Command::new("ar")
        .arg("rcs")
        .arg(&lib)
        .args(&objs)
        .status()
        .unwrap()
        .success());

    let out = d.join("out.a");
    let order = ["helper", "api"].map(|o| format!("libreal.a@{o}_component_long_name.o"));
    ArMerger::new_from_paths([lib], &out)
        .unwrap()
        .merge_and_localize_ordered(KeepSymbols, regexes("api_"), order)
        .unwrap();
    let syms = symbols(&out);

    assert_eq!(exported(&syms), [sym("api_fn"), sym("api_version")]);
    assert!(
        syms[&sym("helper_fn")].address < syms[&sym("api_fn")].address,
        "order file entries use the long member names"
    );
}

#[test]
fn new_from_readers() {
    init();
    let dir = tempdir().unwrap();
    let lib = std::fs::read(c_lib(dir.path(), HOST)).unwrap();
    let out = dir.path().join("out.a");
    let merger = ArMerger::new(vec![InputLibrary::new("libtest.a", Cursor::new(lib))], &out).unwrap();

    let expected = if cfg!(target_os = "macos") {
        ArchiveContents::MachO
    } else {
        ArchiveContents::Elf
    };
    assert_eq!(merger.archive_contents(), expected);
    merger.merge_and_localize(KeepSymbols, regexes("api_")).unwrap();
    assert_eq!(exported(&symbols(&out)), [sym("api_fn"), sym("api_version")]);
}

/// Archives over 4 GiB get a 64-bit symbol index, which must be skipped just like the 32-bit one
#[test]
fn archives_with_64bit_symbol_index() {
    init();
    let dir = tempdir().unwrap();
    let d = dir.path();
    let api = std::fs::read(compile(d, "api.c", include_str!("fixtures/api.c"), HOST)).unwrap();
    let helper = std::fs::read(compile(d, "helper.c", include_str!("fixtures/helper.c"), HOST)).unwrap();
    let index = [0u8; 8]; // an empty index: entry count of zero
    let libs = [
        raw_archive(
            d,
            "libgnu.a",
            &[("/SYM64/", index.as_slice()), ("api.o", api.as_slice())],
            false,
        ),
        raw_archive(
            d,
            "libbsd.a",
            &[
                ("__.SYMDEF_64 SORTED", index.as_slice()),
                ("helper.o", helper.as_slice()),
            ],
            true,
        ),
    ];

    let out = d.join("out.a");
    ArMerger::new_from_paths(libs, &out)
        .unwrap()
        .merge_and_localize(KeepSymbols, regexes("api_"))
        .unwrap();
    assert_eq!(exported(&symbols(&out)), [sym("api_fn"), sym("api_version")]);
}

#[test]
fn input_errors() {
    let dir = tempdir().unwrap();
    let empty = dir.path().join("libempty.a");
    std::fs::write(&empty, b"!<arch>\n").unwrap();
    let out = dir.path().join("out.a");

    let err = ArMerger::new_from_paths([&empty], &out).unwrap_err();
    assert!(matches!(err, ProcessInputError::Empty), "{err}");

    let err = ArMerger::new_from_paths([Path::new("/nonexistent/lib.a")], &out).unwrap_err();
    assert!(matches!(err, ProcessInputError::FileOpen { .. }), "{err}");
}
