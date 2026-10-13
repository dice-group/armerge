//! Builds small real static libraries with clang and inspects armerge's output.
#![allow(dead_code)]

use object::{Architecture, Object, ObjectSymbol, SymbolKind};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::Once,
};

#[derive(Clone, Copy)]
pub enum Target {
    X86_64Linux,
    Aarch64Linux,
    Aarch64MacOs,
}

/// The target whose objects the host toolchain can link natively
pub const HOST: Target = if cfg!(target_os = "macos") {
    Target::Aarch64MacOs
} else {
    Target::X86_64Linux
};

impl Target {
    fn triple(self) -> &'static str {
        match self {
            Target::X86_64Linux => "x86_64-linux-gnu",
            Target::Aarch64Linux => "aarch64-linux-gnu",
            Target::Aarch64MacOs => "aarch64-apple-darwin",
        }
    }

    pub fn arch(self) -> Architecture {
        match self {
            Target::X86_64Linux => Architecture::X86_64,
            Target::Aarch64Linux | Target::Aarch64MacOs => Architecture::Aarch64,
        }
    }
}

/// C symbol name as it appears in the object file (Mach-O prepends an underscore)
pub fn sym(name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("_{name}")
    } else {
        name.to_owned()
    }
}

/// Checks the required tools once and points armerge at the LLVM tools, which handle every architecture
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let tools: &[&str] = if cfg!(target_os = "macos") {
            &["ar", "clang", "clang++", "libtool"]
        } else {
            &["ar", "clang", "clang++", "ld.lld", "llvm-objcopy", "llvm-ranlib"]
        };
        for tool in tools {
            assert!(
                Command::new(tool).arg("--version").output().is_ok(),
                "`{tool}` is required to run the armerge tests"
            );
        }
        if cfg!(target_os = "linux") {
            for (var, value) in [("ARMERGE_LD", "ld.lld"), ("RANLIB", "llvm-ranlib")] {
                if std::env::var_os(var).is_none() {
                    std::env::set_var(var, value);
                }
            }
        }
    });
}

/// Compiles a freestanding C/C++ fixture (`name` ends in `.c` or `.cpp`) into `dir/<stem>.o`
pub fn compile(dir: &Path, name: &str, src: &str, target: Target) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let src_path = dir.join(name);
    std::fs::write(&src_path, src).unwrap();
    let obj = dir.join(format!("{}.o", name.rsplit_once('.').unwrap().0));
    let compiler = if name.ends_with(".cpp") { "clang++" } else { "clang" };
    let output = Command::new(compiler)
        .args(["-c", "-ffreestanding", &format!("--target={}", target.triple())])
        .arg(&src_path)
        .arg("-o")
        .arg(&obj)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{compiler} failed on {name}:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    obj
}

/// Packs objects into a static library at `dir/name`
pub fn archive(dir: &Path, name: &str, objs: &[PathBuf]) -> PathBuf {
    let path = dir.join(name);
    let mut builder = ar::Builder::new(File::create(&path).unwrap());
    for obj in objs {
        builder.append_path(obj).unwrap();
    }
    path
}

/// Writes an archive by hand, so that special members like 64-bit symbol indexes can be included.
/// `bsd` selects the BSD long-name encoding used by macOS `libtool`.
pub fn raw_archive(dir: &Path, name: &str, members: &[(&str, &[u8])], bsd: bool) -> PathBuf {
    let mut out = b"!<arch>\n".to_vec();
    for &(member, data) in members {
        let mut ident = member.to_owned();
        let mut prefix: &[u8] = b"";
        if bsd && member.len() > 16 {
            ident = format!("#1/{}", member.len());
            prefix = member.as_bytes();
        } else if !bsd && !member.starts_with('/') {
            ident.push('/');
        }
        let size = prefix.len() + data.len();
        out.extend_from_slice(format!("{ident:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
        out.extend_from_slice(prefix);
        out.extend_from_slice(data);
        if size % 2 == 1 {
            out.push(b'\n');
        }
    }
    let path = dir.join(name);
    std::fs::write(&path, out).unwrap();
    path
}

/// The usual three-object library: `api_fn` depends on `helper.o`, `unused.o` is referenced by nothing
pub fn c_lib(dir: &Path, target: Target) -> PathBuf {
    let objs = [
        compile(dir, "api.c", include_str!("../fixtures/api.c"), target),
        compile(dir, "helper.c", include_str!("../fixtures/helper.c"), target),
        compile(dir, "unused.c", include_str!("../fixtures/unused.c"), target),
    ];
    archive(dir, "libtest.a", &objs)
}

pub struct Sym {
    pub global: bool,
    pub weak: bool,
    pub undefined: bool,
    pub address: u64,
}

/// Every object file member of an archive, parsed (archive symbol indexes are skipped)
fn objects(archive: &Path) -> Vec<Vec<u8>> {
    let mut archive = ar::Archive::new(File::open(archive).unwrap());
    let mut objs = Vec::new();
    while let Some(entry) = archive.next_entry() {
        let mut data = Vec::new();
        entry.unwrap().read_to_end(&mut data).unwrap();
        if object::File::parse(data.as_slice()).is_ok() {
            objs.push(data);
        }
    }
    objs
}

pub fn object_count(archive: &Path) -> usize {
    objects(archive).len()
}

pub fn architecture(archive: &Path) -> Architecture {
    object::File::parse(objects(archive)[0].as_slice())
        .unwrap()
        .architecture()
}

/// All symbols in the archive's objects, by name
pub fn symbols(archive: &Path) -> BTreeMap<String, Sym> {
    let mut syms = BTreeMap::new();
    for data in objects(archive) {
        let file = object::File::parse(data.as_slice()).unwrap();
        for s in file.symbols() {
            if matches!(s.kind(), SymbolKind::Section | SymbolKind::File) {
                continue;
            }
            let sym = Sym {
                global: s.is_global(),
                weak: s.is_weak(),
                undefined: s.is_undefined(),
                address: s.address(),
            };
            syms.insert(s.name().unwrap().to_owned(), sym);
        }
    }
    syms
}

/// Names of the strong global definitions, i.e. what the archive exports
pub fn exported(syms: &BTreeMap<String, Sym>) -> Vec<&str> {
    syms.iter()
        .filter(|(_, s)| s.global && !s.weak && !s.undefined)
        .map(|(n, _)| n.as_str())
        .collect()
}
