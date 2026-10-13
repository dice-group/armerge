use crate::{
    arbuilder::ArBuilder, input_library::InputLibrary, objects::ObjectTempDir, MergeError,
    MergeError::ExternalToolLaunchError, ProcessInputError,
};
use goblin::{peek_bytes, Hint};
use object::read::archive::ArchiveFile;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use rayon::prelude::*;
use std::{
    ffi::OsString,
    fmt::{Debug, Formatter},
    io::Read,
    str::FromStr,
};
use tracing::info;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArchiveContents {
    /// Only ELF files
    Elf,
    /// Only Mach-O files
    MachO,
    /// Only unsupported files (e.g. PE/COFF)
    Other,

    /// Archives contain a mix of file types
    Mixed,
    /// No contents
    Empty,
}

impl ArchiveContents {
    pub(crate) fn merge(a: ArchiveContents, b: ArchiveContents) -> ArchiveContents {
        #[allow(clippy::if_same_then_else)] // Two of the cases return `a`, that's okay
        if a == ArchiveContents::Mixed || b == ArchiveContents::Mixed {
            ArchiveContents::Mixed
        } else if a == ArchiveContents::Empty {
            b
        } else if b == ArchiveContents::Empty {
            a
        } else if a == b {
            a
        } else {
            ArchiveContents::Mixed
        }
    }
}

pub struct ExtractedArchive {
    pub object_dir: ObjectTempDir,
    pub contents_type: ArchiveContents,
}

impl Debug for ExtractedArchive {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractedArchive")
            .field("object_dir", &self.object_dir.dir.path())
            .field("contents_type", &self.contents_type)
            .finish()
    }
}

fn archive_object_type(object_header: &[u8; 16]) -> ArchiveContents {
    match peek_bytes(object_header) {
        Ok(Hint::Elf(_)) => ArchiveContents::Elf,
        Ok(Hint::Mach(_) | Hint::MachFat(_)) => ArchiveContents::MachO,
        Ok(_) => ArchiveContents::Other,
        Err(_) => ArchiveContents::Other, // Malformed input
    }
}

pub fn extract_objects<I: IntoParallelIterator<Item = InputLibrary<R>>, R: Read>(
    input_libraries: I,
) -> Result<ExtractedArchive, ProcessInputError> {
    let dir = tempfile::Builder::new()
        .prefix("armerge.")
        .tempdir()
        .map_err(ProcessInputError::TempDir)?;

    let (objects, archive_contents) = input_libraries
        .into_par_iter()
        .try_fold(
            || (Vec::new(), ArchiveContents::Empty),
            |(mut objects, mut archive_contents), mut input_lib| {
                let reading_err =
                    |e: std::io::Error| ProcessInputError::ReadingArchive { name: input_lib.name.clone(), inner: e };
                let parse_err = |e: object::Error| reading_err(std::io::Error::other(e));
                let mut data = Vec::new();
                input_lib.reader.read_to_end(&mut data).map_err(reading_err)?;
                let archive = ArchiveFile::parse(data.as_slice()).map_err(parse_err)?;
                for member in archive.members() {
                    let member = member.map_err(parse_err)?;
                    let entry = member.data(data.as_slice()).map_err(parse_err)?;

                    let rnd: String = Alphanumeric.sample_string(&mut rng(), 8);
                    let mut obj_path = dir.path().to_owned();
                    obj_path.push(format!(
                        "{}@{}.{}.o",
                        input_lib.name,
                        String::from_utf8_lossy(member.name()),
                        rnd
                    ));

                    let hint_bytes = entry
                        .get(..16)
                        .and_then(|h| h.try_into().ok())
                        .ok_or_else(|| reading_err(std::io::ErrorKind::UnexpectedEof.into()))?;
                    let obj_type = archive_object_type(hint_bytes);
                    archive_contents = ArchiveContents::merge(archive_contents, obj_type);

                    std::fs::write(&obj_path, entry)
                        .map_err(|e| ProcessInputError::ExtractingObject { path: obj_path.to_owned(), inner: e })?;
                    objects.push(obj_path);
                }

                Ok((objects, archive_contents))
            },
        )
        .try_reduce(
            || (Vec::new(), ArchiveContents::Empty),
            |(mut objs_a, contents_a), (mut objs_b, contents_b)| {
                objs_a.append(&mut objs_b);
                Ok((objs_a, ArchiveContents::merge(contents_a, contents_b)))
            },
        )?;

    Ok(ExtractedArchive {
        object_dir: ObjectTempDir { dir, objects },
        contents_type: archive_contents,
    })
}

pub fn get_object_name_from_path(path: &std::path::Path) -> String {
    let filename = path.file_name().unwrap().to_string_lossy();
    let name_parts = filename.rsplitn(3, '.').collect::<Vec<_>>();
    name_parts[2].to_string()
}

pub fn create_index(archive_path: &std::path::Path) -> Result<(), MergeError> {
    use std::process::Command;

    let ranlib_path = if let Some(var) = std::env::var_os("RANLIB") {
        var
    } else {
        OsString::from_str("ranlib").unwrap()
    };

    info!("{} {}", ranlib_path.to_string_lossy(), archive_path.to_string_lossy());

    let output = Command::new(&ranlib_path)
        .args(vec![archive_path])
        .output()
        .map_err(|e| ExternalToolLaunchError { tool: ranlib_path.to_string_lossy().to_string(), inner: e })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MergeError::ExternalToolError {
            reason: "Failed to create archive index".to_string(),
            tool: ranlib_path.to_string_lossy().to_string(),
            args: archive_path.iter().map(|p| p.to_owned()).collect(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

pub fn merge(mut output: Box<dyn ArBuilder>, objects_dir: ObjectTempDir) -> Result<(), MergeError> {
    for obj_path in objects_dir.objects {
        output.append_obj(obj_path.as_path())?;
    }
    output.close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ArchiveContents::*;

    #[test]
    fn merge_contents() {
        assert_eq!(ArchiveContents::merge(Empty, Elf), Elf);
        assert_eq!(ArchiveContents::merge(MachO, Empty), MachO);
        assert_eq!(ArchiveContents::merge(Elf, Elf), Elf);
        assert_eq!(ArchiveContents::merge(Elf, MachO), Mixed);
        assert_eq!(ArchiveContents::merge(Elf, Other), Mixed);
        assert_eq!(ArchiveContents::merge(Mixed, Empty), Mixed);
    }

    #[test]
    fn object_name_strips_random_suffix() {
        let path = std::path::Path::new("/tmp/armerge.xyz/libfoo.a@bar.o.Ab12Cd34.o");
        assert_eq!(get_object_name_from_path(path), "libfoo.a@bar.o");
    }
}
