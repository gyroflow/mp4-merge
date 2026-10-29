// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2026 Adrian <adrian.eddy at gmail>

//! The output file of `join_files`: it is never one of the inputs, and it gets its name only once it is complete.
//!
//! - Whether the output is an input is told by file identity, not by comparing paths, so another spelling of an input's
//!   path (`..`, `.`), a letter case variant on a case-insensitive volume, a symbolic link and a hard link of an input are
//!   all recognized: by the device and inode numbers on Unix, by the volume serial number and file ID on Windows, and by the
//!   canonical path where a filesystem gives no identity.
//! - The output is written to a new temporary file in its destination directory, flushed to disk and only then renamed to
//!   its name, so a merge that fails, or is killed, never leaves a truncated or partial file under that name. An existing
//!   file under the name is replaced only then, as `File::create` would have overwritten it: where it could be written to,
//!   keeping its permissions on Unix. That the rename will be able to replace it is checked first too: on Windows a program
//!   that has it open without letting it be deleted, as the C runtime and so FFmpeg open files, keeps it from that. Where
//!   only that rename fails, because a program opened the file there while the merge ran, the complete output is kept under
//!   its temporary name, and the error says where.
//! - A symbolic link there is followed, as `File::create` follows it: the file at the end of its links is the one replaced,
//!   or the one created where they point when it isn't there yet, and the links stay. A link is followed only to a file
//!   name, and on Windows only to one that a program's path names as the link does.
//! - A device (`/dev/null`, `NUL`) is written to as it is: there is no file to leave partial, and none to rename onto.

use std::fs::File;
use std::io::{ Read, Seek, SeekFrom, Write, Result, Error, ErrorKind };
use std::path::{ Path, PathBuf };

/// Where the output goes, checked against the inputs. Nothing is created until [`Destination::create`]
pub struct Destination {
    /// The output's path as given, for messages
    path: PathBuf,
    /// What gets the output: the path, or the file at the end of the symbolic links there
    target: PathBuf,
    kind: Kind,
    /// The mode of the file it replaces, which the output keeps
    #[cfg(unix)]
    permissions: Option<std::fs::Permissions>,
}

enum Kind {
    /// No file is there yet (where the path is a symbolic link: none where it points)
    New,
    /// A file is there
    Replace,
    /// A device, written to directly
    Device,
}

impl Destination {
    /// Checks that the output `path` isn't one of the `inputs` (open, with `names` their paths), that an existing file there
    /// can be written to and replaced, and that a new one can be created there. Unless it's a device, it must be a file name,
    /// and so must the file at the end of the symbolic links there. Where it's a new file, nothing may be at the file its
    /// links lead to by their letters either, since the system reached nothing through them. Nothing is created or changed.
    pub fn check<P: AsRef<Path>>(path: &Path, names: &[P], inputs: &[(File, usize)]) -> Result<Self> {
        let error = |e: Error| Error::new(e.kind(), format!("checking the output {}: {e}", path.display()));
        let as_given = |kind| Self { path: path.to_owned(), target: path.to_owned(), kind, #[cfg(unix)] permissions: None };
        // Through the symbolic links there, which the system follows as it would for `File::create`, where it lets them be
        let existing = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Nothing there, or a symbolic link to where nothing is yet: the file is created where it points
                let target = if is_link(path) { links_end(path).map_err(error)? } else { path.to_owned() };
                check_new(path, &target)?;
                // Nothing may be at `target`: the system, following the links there as `File::create` would, reached nothing,
                // so this is a new file. Where the links lead by their letters to a file the system doesn't reach that way,
                // `create` and the rename would make and replace it with none of the input-identity, write and replace checks
                // the existing-file branch runs, and it could be an input. That is how a crafted drive-relative link (made by
                // `FSCTL_SET_REPARSE_POINT`, not by any normal API, its substitute name a bare `F:out.mp4`) destroyed a file:
                // the system resolves the relative reparse inside the link's own folder and finds nothing, while `read_link`
                // gives `F:out.mp4` verbatim, which resolves against F:'s current directory to an existing file. The check is
                // where the rename would act, so it is caught whatever the spelling. A healthy dangling link's end is absent,
                // and a link the system follows normally leads where its letters do, so neither is refused; a no-op on Unix
                if target != path && std::fs::symlink_metadata(&target).is_ok() {
                    return Err(error(Error::new(ErrorKind::InvalidInput, format!("the symbolic link leads to {}, an existing file that the system doesn't reach through it", target.display()))));
                }
                return Ok(Self { target, ..as_given(Kind::New) });
            },
            Err(e) if is_device(path) => {
                log::debug!("The output {} is a device ({e})", path.display());
                return Ok(as_given(Kind::Device));
            },
            Err(e) => return Err(error(e))
        };
        if existing.is_dir() {
            return Err(Error::new(ErrorKind::InvalidInput, format!("The output {} is a directory", path.display())));
        }
        if !existing.is_file() {
            // A device or a pipe, never a file of the inputs. Not opened to identify it: opening a pipe waits for a writer
            return Ok(as_given(Kind::Device));
        }
        // A file is there, but its path must be a file name, as a new output's: on Windows `out.mp4\.` names `out.mp4`, which
        // Unix refuses, and `out.mp4:alt` a stream of a file or a folder, whose temporary file would be a stream of another
        // file, which no rename can give the name
        check_name(path, path)?;
        let output = File::open(path);
        // The file replaced: the one at the end of the symbolic links there, which keep pointing to it. Its canonical path
        // names a stream where a link points to one
        let target = if is_link(path) { linked_file(path, output.as_ref().ok()).map_err(error)? } else { path.to_owned() };
        check_name(path, &target)?;
        // Where neither the identity nor the canonical paths tell, the output is refused, after what makes it fail anyway
        let mut unknown = None;
        for (input, name) in inputs.iter().zip(names) {
            let known = match &output {
                Ok(output) => same_file(&input.0, output).map_err(error)?,
                Err(_) => None
            };
            let same = match known {
                Some(same) => same,
                // A filesystem that gives no identity, or an output that can't be opened: the canonical paths tell
                None => match (std::fs::canonicalize(name), std::fs::canonicalize(&target)) {
                    (Ok(a), Ok(b)) => a == b,
                    (Err(e), _) | (_, Err(e)) => {
                        let why = match &output { Ok(_) => "its filesystem gives no file IDs".to_owned(), Err(open) => format!("it can't be opened ({open})") };
                        unknown.get_or_insert_with(|| Error::new(e.kind(), format!("The output {} can't be told apart from the input {}: {why}, and there is no canonical path: {e}", path.display(), name.as_ref().display())));
                        false
                    }
                }
            };
            if same {
                return Err(Error::new(ErrorKind::InvalidInput, format!("The output {} is the input {}: merging into it would destroy it", path.display(), name.as_ref().display())));
            }
        }
        drop(output);
        // Replaced only where `File::create` could have written to it...
        if let Err(e) = std::fs::OpenOptions::new().write(true).open(&target) {
            return Err(if is_busy(&e) { busy(path) } else { Error::new(e.kind(), format!("The output {} can't be written to: {e}", path.display())) });
        }
        // ...and where the rename at the end can replace it
        #[cfg(windows)]
        if let Err(e) = can_be_replaced(&target) {
            return Err(if is_busy(&e) { busy(path) } else { Error::new(e.kind(), format!("The output {} can't be replaced: {e}", path.display())) });
        }
        if let Some(e) = unknown {
            return Err(e);
        }
        Ok(Self { path: path.to_owned(), target, kind: Kind::Replace, #[cfg(unix)] permissions: Some(existing.permissions()) })
    }

    /// Creates what the output is written to: a temporary file next to the file that gets its name, or the device
    pub fn create(&self) -> Result<Partial> {
        let partial = |file, temp| Partial { file, temp, target: self.target.clone(), path: self.path.clone(), #[cfg(unix)] permissions: self.permissions.clone() };
        if let Kind::Device = self.kind {
            let file = File::options().read(true).write(true).open(&self.target).map_err(|e| Error::new(e.kind(), format!("opening the output {}: {e}", self.path.display())))?;
            return Ok(partial(file, None));
        }
        let dir = folder(&self.target);
        let name = self.target.file_name().unwrap_or_default();
        for n in 0..1000u32 {
            let mut temp_name = std::ffi::OsString::from(".");
            temp_name.push(name);
            temp_name.push(format!(".{}-{n}.mp4merge-partial", std::process::id()));
            let temp = dir.join(temp_name);
            match File::options().read(true).write(true).create_new(true).open(&temp) {
                Ok(file) => return Ok(partial(file, Some(temp))),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::new(e.kind(), format!("creating a temporary file in {}: {e}", dir.display())))
            }
        }
        Err(Error::new(ErrorKind::AlreadyExists, format!("no free temporary name next to {}", self.target.display())))
    }
}

/// The output while it is written: a temporary file, removed unless it is committed, or a device
pub struct Partial {
    file: File,
    /// The temporary file, until it is renamed
    temp: Option<PathBuf>,
    target: PathBuf,
    path: PathBuf,
    #[cfg(unix)]
    permissions: Option<std::fs::Permissions>,
}

impl Partial {
    /// Gives the complete output its name. It is on disk first, so a crash can't leave a short file under the name. Where only
    /// the rename fails, the complete output is kept under its temporary name, and the error says where; it is removed when
    /// anything before fails
    pub fn commit(mut self) -> Result<()> {
        self.file.flush()?;
        let Some(temp) = self.temp.clone() else { return Ok(()) };
        self.file.sync_all().map_err(|e| Error::new(e.kind(), format!("flushing {} to disk: {e}", temp.display())))?;
        #[cfg(unix)]
        if let Some(permissions) = &self.permissions {
            std::fs::set_permissions(&temp, permissions.clone()).map_err(|e| Error::new(e.kind(), format!("setting the permissions of {}: {e}", temp.display())))?;
        }
        if let Err(e) = std::fs::rename(&temp, &self.target) {
            // Complete and on disk: kept, rather than merged again
            self.temp = None;
            let kept = format!("The merged file is kept as {}.", temp.display());
            // A program that opened the file there while the merge ran
            #[cfg(windows)]
            if can_be_replaced(&self.target).is_err_and(|e| is_busy(&e)) {
                return Err(Error::new(ErrorKind::ResourceBusy, format!("{}. {kept}", busy(&self.path))));
            }
            return Err(Error::new(e.kind(), format!("giving the merged file the name {}: {e}. {kept}", self.path.display())));
        }
        self.temp = None;
        Ok(())
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(temp) = &self.temp {
            if let Err(e) = std::fs::remove_file(temp) {
                log::warn!("The partial output {} couldn't be removed: {e}", temp.display());
            }
        }
    }
}

impl Read for Partial {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> { self.file.read(buf) }
}
impl Write for Partial {
    fn write(&mut self, buf: &[u8]) -> Result<usize> { self.file.write(buf) }
    fn flush(&mut self) -> Result<()> { self.file.flush() }
}
impl Seek for Partial {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> { self.file.seek(pos) }
}

/// The folder of the file `path`
fn folder(path: &Path) -> &Path {
    path.parent().filter(|x| !x.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

/// Whether `path` is a symbolic link (on Windows also a junction)
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// That `File::create` could create the output `path`, which is a new file at `target` (where a symbolic link there
/// points): both are file names, and it is in a folder that exists
fn check_new(path: &Path, target: &Path) -> Result<()> {
    check_name(path, target)?;
    let output = shown(path, target);
    let dir = folder(target);
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(Error::new(ErrorKind::NotADirectory, format!("{output} can't be created: {} isn't a directory", dir.display()))),
        Err(e) => Err(Error::new(e.kind(), format!("{output} can't be created in {}: {e}", dir.display())))
    }
}

/// That the output `path`, and `target`, the file that gets its name (where the symbolic links there lead), are file names
fn check_name(path: &Path, target: &Path) -> Result<()> {
    match [path, target].into_iter().find(|x| not_a_file_name(x)) {
        Some(x) => Err(Error::new(ErrorKind::InvalidInput, format!("{} isn't a file name", shown(path, x)))),
        None => Ok(())
    }
}

/// The output `path` in messages, with `target` where the symbolic links there lead to another path
fn shown(path: &Path, target: &Path) -> String {
    if target == path { format!("The output {}", path.display()) } else { format!("The output {}, a symbolic link to {},", path.display(), target.display()) }
}

/// Whether `path` names no file: it ends with a separator, its last part is `.` or `..`, or on Windows its name holds a `:`.
/// Refused alike on every system: on Unix `File::create` and `rename` both refuse those, Windows refuses a separator at the
/// end too, and there a `:` names a stream of a file, which a rename can't give the output
fn not_a_file_name(path: &Path) -> bool {
    let text = path.as_os_str().as_encoded_bytes();
    let separator = |x: &u8| std::path::is_separator(*x as char);
    // `file_name` skips a `.` at the end
    let dot = text.strip_suffix(b".").is_some_and(|x| x.last().is_none_or(separator));
    text.last().is_some_and(separator) || dot || path.file_name().is_none_or(|x| cfg!(windows) && x.as_encoded_bytes().contains(&b':'))
}

/// The most symbolic links followed one after another: as many as Windows follows in a path, more than Linux (40) and macOS
/// (32), so a chain that the system followed is never cut short
const MAX_LINKS: usize = 63;

/// Where the symbolic links from `path` on lead: each followed to what it points to, a relative one from its folder, up to
/// the first path that isn't a link, which needn't exist. Relative ones are joined as they are, so the system resolves the
/// path that results as it would have resolved the link: a `..` in it physically on Unix, by its letters on Windows. Each
/// must point to a file name
fn links_end(path: &Path) -> Result<PathBuf> {
    let mut end = path.to_owned();
    // Each link followed, and then what the last one leads to
    for _ in 0..=MAX_LINKS {
        if !is_link(&end) {
            return Ok(end);
        }
        let to = std::fs::read_link(&end)?;
        if not_a_file_name(&to) {
            return Err(Error::new(ErrorKind::InvalidInput, format!("the symbolic link {} points to {}, which isn't a file name", end.display(), to.display())));
        }
        let next = end.parent().unwrap_or(Path::new("")).join(&to);
        #[cfg(windows)]
        check_link_name(&end, &to)?;
        #[cfg(windows)]
        check_up_link(&end, &to, &next)?;
        end = next;
    }
    Err(Error::new(ErrorKind::InvalidInput, format!("it leads through more than {MAX_LINKS} symbolic links, or a loop of them")))
}

/// The file the symbolic link `path` leads to, the existing file `output` (`None`: it couldn't be opened): its canonical
/// path, or where its volume has none (on Windows one without a DOS name), the end of its links when that is the same file
fn linked_file(path: &Path, output: Option<&File>) -> Result<PathBuf> {
    let e = match std::fs::canonicalize(path) {
        Ok(target) => return Ok(target),
        Err(e) => e
    };
    let end = links_end(path)?;
    let same = match (output, File::open(&end)) {
        (Some(output), Ok(end)) => same_file(output, &end)?,
        _ => None
    };
    match same {
        Some(true) => Ok(end),
        _ => Err(Error::new(e.kind(), format!("finding the file its symbolic links lead to: {e}")))
    }
}

/// `ERROR_SHARING_VIOLATION`
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether `e` is the error of a file that another program has open without sharing what was asked for: on Windows a
/// sharing violation. On Unix files aren't held against other programs
fn is_busy(e: &Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
}

/// The error for an output that a program has open, which keeps it from being written to or replaced
fn busy(path: &Path) -> Error {
    Error::new(ErrorKind::ResourceBusy, format!("The output {} is open in another program, which keeps it from being replaced: close it there first", path.display()))
}

/// Whether the open files `a` and `b` are the same file, `None` where the filesystem doesn't tell
#[cfg(unix)]
fn same_file(a: &File, b: &File) -> Result<Option<bool>> {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok(Some((a.dev(), a.ino()) == (b.dev(), b.ino())))
}

/// Whether `path`, which can't be examined, is a device: on Unix those can, so it's never one
#[cfg(not(windows))]
fn is_device(_path: &Path) -> bool { false }

#[cfg(windows)]
use win::*;
#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::io::{ Error, ErrorKind, Result };
    use std::os::windows::io::AsRawHandle;
    use std::ffi::c_void;
    use std::path::{ Component, Path };

    /// `FILE_ID_INFO`
    #[repr(C)]
    struct FileIdInfo { volume_serial_number: u64, file_id: [u8; 16] }
    /// `BY_HANDLE_FILE_INFORMATION`, its `FILETIME`s as two `DWORD`s each. The system fills it; only the volume and the
    /// file index are read
    #[repr(C)]
    #[allow(dead_code)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    /// `FileIdInfo` of `FILE_INFO_BY_HANDLE_CLASS`
    const FILE_ID_INFO_CLASS: i32 = 18;
    /// `FILE_TYPE_CHAR` of `GetFileType`
    const FILE_TYPE_CHAR: u32 = 2;
    /// The `DELETE` access right
    const DELETE: u32 = 0x0001_0000;
    /// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`
    const FILE_SHARE_ALL: u32 = 1 | 2 | 4;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandleEx(file: *mut c_void, class: i32, info: *mut c_void, size: u32) -> i32;
        fn GetFileInformationByHandle(file: *mut c_void, info: *mut ByHandleFileInformation) -> i32;
        fn GetFileType(file: *mut c_void) -> u32;
    }

    /// Whether the open files `a` and `b` are the same file, `None` where the filesystem doesn't tell.
    ///
    /// Files are identified by the volume serial number and the 128-bit file ID of `FILE_ID_INFO`, which ReFS needs (its IDs
    /// don't fit 64 bits), and where a filesystem doesn't give those, by the 64-bit file index of
    /// `BY_HANDLE_FILE_INFORMATION`. Both files' come from the same call, so the two kinds are never compared. A file ID of
    /// 0 isn't one: some network filesystems give 0 for every file.
    pub fn same_file(a: &File, b: &File) -> Result<Option<bool>> {
        let (a, b) = match (file_id(a), file_id(b)) {
            (Some(a), Some(b)) => (a, b),
            _ => (file_index(a)?, file_index(b)?)
        };
        Ok((a.1 != 0 && b.1 != 0).then_some(a == b))
    }

    fn file_id(file: &File) -> Option<(u64, u128)> {
        let mut info = FileIdInfo { volume_serial_number: 0, file_id: [0; 16] };
        // SAFETY: the handle is the open file's, and `info` is a FILE_ID_INFO of the size given
        let ok = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FILE_ID_INFO_CLASS, &mut info as *mut FileIdInfo as *mut c_void, std::mem::size_of::<FileIdInfo>() as u32) };
        (ok != 0).then(|| (info.volume_serial_number, u128::from_le_bytes(info.file_id)))
    }

    fn file_index(file: &File) -> Result<(u64, u128)> {
        let mut info = ByHandleFileInformation { file_attributes: 0, creation_time: [0; 2], last_access_time: [0; 2], last_write_time: [0; 2], volume_serial_number: 0, file_size_high: 0, file_size_low: 0, number_of_links: 0, file_index_high: 0, file_index_low: 0 };
        // SAFETY: the handle is the open file's, and `info` is a BY_HANDLE_FILE_INFORMATION
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.volume_serial_number as u64, (info.file_index_high as u128) << 32 | info.file_index_low as u128))
    }

    /// Whether `path`, which can't be examined, is a device such as `NUL`: those have no file attributes
    pub fn is_device(path: &Path) -> bool {
        // SAFETY: the handle is the open file's
        File::options().write(true).open(path).is_ok_and(|f| unsafe { GetFileType(f.as_raw_handle()) } == FILE_TYPE_CHAR)
    }

    /// Whether the file `path` can be replaced by a rename: opening it to delete it fails as the rename would, where a
    /// program has it open without letting it be deleted (a sharing violation; the C runtime's `open` and `fopen`, and so
    /// FFmpeg, open files so) or it can't be deleted. A read-only file can be opened so, and it's refused for its writing
    pub fn can_be_replaced(path: &Path) -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        File::options().access_mode(DELETE).share_mode(FILE_SHARE_ALL).open(path).map(drop)
    }

    /// That the symbolic link `link`, pointing to `to`, leads to the file that the path joined from `to` names. Windows follows
    /// a link by the letters of `to`, while a program's path loses the dots and spaces at the end of its parts
    /// (`GetFullPathNameW`) and reads a `:` as a stream: through `final.mp4.` `File::create` makes `final.mp4.`, which the
    /// rename would publish as `final.mp4`, leaving the link pointing to nothing. Such a link isn't followed
    pub fn check_link_name(link: &Path, to: &Path) -> Result<()> {
        let renamed = |x: &[u8]| x.ends_with(b".") || x.ends_with(b" ") || x.contains(&b':');
        if to.components().any(|x| matches!(x, Component::Normal(x) if renamed(x.as_encoded_bytes()))) {
            return Err(Error::new(ErrorKind::InvalidInput, format!("the symbolic link {} points to {}, a name that Windows follows as it's written but programs can't use: a part of it ends with a dot or a space, or holds a ':'", link.display(), to.display())));
        }
        Ok(())
    }

    /// That the symbolic link `link`, pointing to `to`, which makes it lead to `next`, can't lead elsewhere. Windows resolves
    /// a `..` in a relative link by the letters of the path the link was reached by, in which a directory symbolic link has
    /// been replaced by its target but a junction hasn't: so where the link's folder is reached through one of those, the
    /// folder `..` goes up to can be another than `next`'s. Then it isn't followed
    pub fn check_up_link(link: &Path, to: &Path, next: &Path) -> Result<()> {
        let relative = matches!(to.components().next(), Some(Component::Normal(_) | Component::CurDir | Component::ParentDir));
        if !relative || !to.components().any(|x| x == Component::ParentDir) {
            return Ok(());
        }
        // The folder it goes into, and the one `..` going up from where the link's folder really is leads to
        let folder = |p: &Path| std::fs::canonicalize(super::folder(p));
        let refused = |why: String| Err(Error::new(ErrorKind::InvalidInput, format!("the symbolic link {} points to {}, up from its folder, {why}", link.display(), to.display())));
        match (folder(next), folder(link).map(|dir| folder(&dir.join(to)))) {
            (Ok(a), Ok(Ok(b))) if a == b => Ok(()),
            // Neither is there: it leads into a folder that doesn't exist, which the checks of the output then say
            (Err(a), Ok(Err(b))) if a.kind() == ErrorKind::NotFound && b.kind() == ErrorKind::NotFound => Ok(()),
            (Err(e), Ok(Err(_))) | (_, Err(e)) => refused(format!("and where that folder really is can't be told: {e}")),
            _ => refused("which is reached through another link: which folder Windows takes it to can't be told".to_owned())
        }
    }
}

/// Whether the open files `a` and `b` are the same file: this platform doesn't tell
#[cfg(not(any(unix, windows)))]
fn same_file(_: &File, _: &File) -> Result<Option<bool>> {
    Ok(None)
}
