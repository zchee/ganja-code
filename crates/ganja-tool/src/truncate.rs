//! Output truncation, so one tool call cannot flood the context window.
//!
//! Spec: upstream `packages/opencode/src/tool/truncate.ts` and
//! `truncation-dir.ts`. The budgets below are upstream's
//! `MAX_LINES`/`MAX_BYTES`, and the notice mirrors the `removed {unit}
//! truncated` wording `Truncate.output` appends in its "head" direction (the
//! only direction any caller here needs).
//!
//! Several ported prompts — `bash`'s among them — tell the model, verbatim,
//! that truncated output "will be written to a file" it can `Read` with
//! `offset`/`limit`. For that to be true, a truncating clamp has to actually
//! write it: `write_overflow` spills the full, untouched text to a file and
//! `hint` tells the model where, the same as upstream's `Truncate.output`.
//!
//! Two upstream pieces are deliberately not ported:
//!
//! - **Where the file lives.** Upstream's `TRUNCATION_DIR` is
//!   `path.join(Global.Path.data, "tool-output")`, and this port has no
//!   `Global.Path` equivalent. The location is resolved the way
//!   `ganja-core`'s `auth` and [`ganja_permission::project`] already resolve
//!   their own state — the same `ganja` directory under the XDG data
//!   home — landing on `<XDG data home>/ganja/tool-output/`.
//! - **The file name.** Upstream names the file with a `ToolID` (a sortable
//!   identifier tied to session bookkeeping this crate does not have yet).
//!   Files here are named `tool_<hex timestamp>_<hex counter>` — unique and
//!   creation-ordered, which is all a stray file on disk actually needs — and
//!   they carry upstream's `tool_` prefix, because that prefix is what a sweep
//!   recognises as its own.
//!
//! The sweep itself *is* ported ([`sweep`], [`spawn_sweep_loop`]): upstream
//! prunes the directory hourly from a forked background fiber, and a spill
//! directory nothing ever empties grows for as long as the machine lives.
//! It is deliberately not part of the clamp — a pure function that deletes
//! files as a side effect would be a surprise — so the frontend starts the
//! loop and cancels it on the way out, exactly as it does the catalog's.
//!
//! A truncating clamp tries the ganja data directory first and a process
//! temp directory second — the "app data path" upstream anchors under has no
//! equivalent this port can always resolve (no `$HOME`, a read-only data
//! volume), and the prompt's promise that the file exists should survive
//! more than the single most common way to make it not exist. Only when
//! neither candidate can be written — no resolvable home directory, a full
//! disk, a path a stray file blocks — does this degrade to the pathless
//! notice, silently. The tool call already succeeded by the time truncation
//! runs; losing the overflow file is never a reason to fail it.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use tokio_util::sync::CancellationToken;

/// Upper bound on the bytes a tool result may carry. Upstream's `MAX_BYTES`
/// (`50 * 1024`); named `MAX_CHARS` because other tools in this crate quote
/// it as a budget on `&str::len()`, which is bytes, not `char`s.
pub const MAX_CHARS: usize = 50 * 1024;

/// Upper bound on the lines a tool result may carry. Upstream's `MAX_LINES`.
pub const MAX_LINES: usize = 2_000;

/// Directory ganja keeps its state in, under the XDG data home. Matches
/// `ganja-core`'s `auth` and [`ganja_permission::project`], which resolve their
/// own state the same way.
const DIRECTORY: &str = "ganja";

/// Where a truncating clamp spills its full text, under [`DIRECTORY`].
/// Upstream's `TRUNCATION_DIR`.
const TOOL_OUTPUT: &str = "tool-output";

/// What every spilled file is called first, and the only thing [`sweep`] will
/// delete. Upstream's own prefix, and its own sweep's filter.
const PREFIX: &str = "tool_";

/// How old a spill has to be before [`sweep`] removes it. Upstream's seven
/// days.
///
/// Age is read from the modification stamp, which is the one question worth
/// asking: a spill still being appended to by a running command was modified a
/// moment ago, so a live file cannot be swept out from under its writer.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long [`spawn_sweep_loop`] waits between rounds. Upstream's hour.
const SWEEP_REPEAT: Duration = Duration::from_secs(60 * 60);

/// Mode a spilled file is created with: its owner, and nobody else.
///
/// A deliberate divergence. Upstream writes these through
/// `fs.writeFileString`, whose Node default is `0o666 & ~umask` — 0644 on a
/// normal machine. What lands in the file is a tool's entire output, which is
/// as easily `env`, a `.env` a grep walked into or a private repository's
/// history as it is a build log, and [`candidate_dirs`] will fall back to a
/// world-readable `/tmp` when there is no data directory to use. Narrowing to
/// the owner costs nothing and closes that, on the same footing as `read` and
/// `grep` refusing the credential store (`tool/mod.rs`,
/// `is_credential_store`): both are places where upstream's behaviour would
/// hand this machine's secrets to somebody who asked politely.
#[cfg(unix)]
const PRIVATE: u32 = 0o600;

/// Mode a spill directory is created with.
///
/// Only ever applied to a directory this code creates — [`fs::DirBuilder`]
/// leaves an existing one exactly as it found it, which is the intent: a
/// directory somebody else made is theirs, and quietly chmod-ing it is not
/// this function's business.
#[cfg(unix)]
const PRIVATE_DIR: u32 = 0o700;

/// A possibly-clamped tool output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncated {
    /// What survives, with a note appended when anything was cut.
    pub text: String,
    /// Whether anything was cut.
    pub truncated: bool,
}

/// Clamps `text` to the line and byte budgets, spilling the full original to
/// a file when anything was cut — the ganja data directory first, a temp
/// directory second if that could not be resolved or written to (see this
/// module's doc comment).
///
/// There is no error to report when neither candidate can be written: the
/// call this wraps already succeeded, and `clamp_in` degrades to the
/// pathless notice rather than fail it.
#[must_use]
pub fn clamp(text: &str) -> Truncated {
    clamp_in(text, MAX_CHARS, candidate_dirs())
}

/// Same as [`clamp`], but spills to exactly `dir` — no XDG resolution, no
/// temp-dir fallback — so a caller can assert on the overflow file without
/// touching a real person's data directory, and so a test can force the
/// degraded path by pointing `dir` somewhere writing will fail. Test-only —
/// no production caller names a spill directory, and the crate gates such a
/// seam rather than shipping it (`shell.rs`'s `spilling_into`).
#[cfg(test)]
fn clamp_with(text: &str, dir: &Path) -> Truncated {
    clamp_in(text, MAX_CHARS, [dir.to_owned()])
}

/// Same as [`clamp`], but against `max_bytes` rather than [`MAX_CHARS`] — the
/// budget a caller with its own configured byte cap names, an MCP server's
/// `output_limit` among them. The line budget ([`MAX_LINES`]) is unchanged: no
/// caller of this so far has needed to move it, and moving one budget without
/// the other is what a per-caller byte cap actually asked for.
#[must_use]
pub fn clamp_bytes(text: &str, max_bytes: usize) -> Truncated {
    clamp_in(text, max_bytes, candidate_dirs())
}

/// [`clamp_bytes`] over exactly `dir`, for the reason `clamp_with` exists,
/// and gated the same way.
#[cfg(test)]
fn clamp_bytes_with(text: &str, max_bytes: usize, dir: &Path) -> Truncated {
    clamp_in(text, max_bytes, [dir.to_owned()])
}

/// Shared implementation behind [`clamp`] and [`clamp_bytes`], and behind the
/// `#[cfg(test)]` `clamp_with`/`clamp_bytes_with` beside them: clamps `text`
/// to `max_bytes`, then writes it to the first of `dirs` that accepts it.
fn clamp_in(text: &str, max_bytes: usize, dirs: impl IntoIterator<Item = PathBuf>) -> Truncated {
    let Some(body) = clamp_body(text, max_bytes) else {
        return Truncated {
            text: text.to_owned(),
            truncated: false,
        };
    };

    let written = dirs
        .into_iter()
        .find_map(|dir| write_overflow(&dir, text.as_bytes()));
    let text = match written {
        Some((file, _)) => format!("{body}\n\n{}", hint(&file)),
        None => body,
    };

    Truncated {
        text,
        truncated: true,
    }
}

/// The clamped preview and upstream's `...N {unit} truncated...` notice, or
/// [`None`] when `text` already fits both budgets.
///
/// Splitting on `\n` first and only ever rejoining whole lines is what keeps
/// a clamp from splitting a UTF-8 code point: every piece rejoined here was
/// already a valid `&str` before the split. `max_bytes` is the byte half of
/// the budget; the line half stays [`MAX_LINES`] for every caller — see
/// [`clamp_bytes`].
fn clamp_body(text: &str, max_bytes: usize) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();

    if lines.len() <= MAX_LINES && total_bytes <= max_bytes {
        return None;
    }

    let mut bytes = 0_usize;
    let mut kept = 0_usize;
    let mut hit_bytes = false;
    for (index, line) in lines.iter().enumerate() {
        if index >= MAX_LINES {
            break;
        }
        let size = line.len() + usize::from(index > 0);
        if bytes + size > max_bytes {
            hit_bytes = true;
            break;
        }
        bytes += size;
        kept = index + 1;
    }

    let preview = lines[..kept].join("\n");
    let removed = if hit_bytes {
        total_bytes - bytes
    } else {
        lines.len() - kept
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };

    Some(format!("{preview}\n\n...{removed} {unit} truncated..."))
}

/// Tells the model where the full output went and how to read it without
/// pulling the whole thing back into context. Upstream's `hint`, minus the
/// branch for an agent holding a Task tool: that branch tells the model to
/// delegate the reading to a subagent, and this port deliberately does not —
/// a spill file is read with the tools the caller already has, and which
/// agents may delegate is not a question a truncation notice may answer. So
/// every call is upstream's other branch, the one that points at `grep` and
/// `read` instead.
fn hint(file: &Path) -> String {
    format!(
        "The tool call succeeded but the output was truncated. Full output saved to: {}\n\
         Use Grep to search the full content or Read with offset/limit to view specific sections.",
        file.display()
    )
}

/// Opens the file a still-running stream spills into, seeded with everything
/// the stream produced before the spill was needed, and hands back the handle
/// so the rest can be appended to it as it arrives.
///
/// This is what keeps a command that writes more than it is allowed to keep
/// from having to hold the overflow in memory (`tool/shell.rs`, `Collector`,
/// and `ganja-core`'s `job::Buffer` for a background job's own pump). [`None`]
/// means there is nowhere writable to spill to, which the caller has to
/// survive rather than report: the tool call itself is fine, and only the
/// overflow is lost.
///
/// Public — not `pub(crate)` — because `ganja-core`'s background-job registry
/// spills a running job's output the same owner-only way a foreground
/// command's collector does, and duplicating the symlink-safe,
/// permission-hardened file creation this wraps would be duplicating the
/// exact code a security review has to read twice instead of once.
pub fn open_spill(head: &[u8]) -> Option<(PathBuf, fs::File)> {
    candidate_dirs()
        .into_iter()
        .find_map(|dir| write_overflow(&dir, head))
}

/// Same as [`open_spill`], but into exactly `dir` — no XDG resolution and no
/// temp-dir fallback — so a test can assert on what was spilled without
/// filling a real person's data directory with fixtures. Mirrors
/// `clamp_with`, which exists for the same reason.
pub fn open_spill_in(dir: &Path, head: &[u8]) -> Option<(PathBuf, fs::File)> {
    write_overflow(dir, head)
}

/// Writes `bytes` to a fresh file under `dir`, creating `dir` first if it
/// does not exist yet, and hands back the still-open handle beside its path.
/// [`None`] on any failure — the directory cannot be created or secured, the
/// write fails — which is exactly the signal [`clamp_in`] needs to fall back
/// to the pathless notice instead, and [`open_spill`] needs to try the next
/// candidate directory.
fn write_overflow(dir: &Path, bytes: &[u8]) -> Option<(PathBuf, fs::File)> {
    create_dir_private(dir).ok()?;
    let path = dir.join(overflow_filename());
    let file = write_private(&path, bytes).ok()?;

    Some((path, file))
}

/// Creates `dir` and any missing parent, owner-only where the platform has
/// modes to set.
#[cfg(unix)]
fn create_dir_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR)
        .create(dir)
}

/// Creates only the Windows leaf with an inheritable owner-only DACL.
///
/// Parents still go through [`fs::create_dir_all`], unlike the unix twin,
/// whose recursive builder applies its mode to every directory it creates.
/// Secrets land only in this leaf, so it is the boundary that must be born
/// private; an existing leaf is somebody else's and stays exactly as found.
#[cfg(windows)]
fn create_dir_private(dir: &Path) -> io::Result<()> {
    windows_acl::create_dir_private(dir)
}

/// Platforms without unix modes or Windows DACLs retain the old create.
#[cfg(not(any(unix, windows)))]
fn create_dir_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Creates `path` for writing, refusing to follow anything already sitting
/// there.
///
/// `create_new` is what makes the `/tmp` candidate safe to use at all: the
/// directory is world-writable, and a plain create would happily follow a
/// symbolic link somebody planted at the name and write a tool's whole output
/// wherever the link led. Mirrors `auth::create_private`, which guards the
/// credential store the same way and for the same reason.
#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    // The mode is set at creation rather than afterwards, so the file is
    // never, even briefly, readable by anyone else.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE)
        .open(path)
}

/// Creates a Windows file with the right to replace its inherited DACL.
#[cfg(windows)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_WRITE, WRITE_DAC};

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_WRITE | WRITE_DAC)
        .open(path)
}

/// Platforms without unix modes or Windows DACLs retain the old open.
#[cfg(not(any(unix, windows)))]
fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Writes `bytes` to a newly created file only its owner can read, leaving it
/// open for whatever else the caller has to append.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<fs::File> {
    use std::io::Write as _;

    let mut file = match create_private(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Either an earlier spill that died before its name was reused,
            // or something planted to catch this one. Unlinking the name and
            // creating it again exclusively settles both: what is removed is
            // the name, never whatever it pointed at, and a second link
            // planted in between fails the retry outright.
            fs::remove_file(path)?;
            create_private(path)?
        }
        result => result?,
    };
    // `open` masks the mode with the process umask, so a wide umask cannot
    // widen this but a narrow one could leave the file unreadable to the
    // owner — which is the one reader that matters, since the notice tells
    // the model to go and read it. This is on the descriptor, not the path,
    // so nothing that happens to the name can redirect it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(fs::Permissions::from_mode(PRIVATE))?;
    }
    // The handle carries WRITE_DAC from its exclusive open, so inheritance is
    // severed before there is a tool-output byte for another identity to race
    // for. The name cannot redirect this descriptor-anchored seal.
    #[cfg(windows)]
    windows_acl::seal_private(&file)?;
    file.write_all(bytes)?;

    Ok(file)
}

/// Windows spells the unix owner-only modes as protected DACLs granting the
/// process token's user alone. SYSTEM and Administrators are not named: both
/// can take ownership regardless, so explicit grants would only widen access.
#[cfg(windows)]
mod windows_acl {
    use std::{
        fs, io,
        mem::size_of,
        os::windows::{ffi::OsStrExt as _, io::AsRawHandle as _},
        path::Path,
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE,
            WIN32_ERROR,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx,
            Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
            CONTAINER_INHERIT_ACE, CopySid, DACL_SECURITY_INFORMATION, GetLengthSid,
            GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, IsValidSid,
            OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
            SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{CreateDirectoryW, FILE_ALL_ACCESS},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    /// The revision accepted by `InitializeSecurityDescriptor`.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// A kernel handle which is not a borrowed file or process pseudo-handle.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper is constructed only from a successful
            // OpenProcessToken call and owns that one handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// An aligned, self-contained SID.
    pub(super) struct OwnedSid {
        words: Box<[usize]>,
    }

    impl OwnedSid {
        fn zeroed(bytes: u32) -> Self {
            let words = (bytes as usize).div_ceil(size_of::<usize>());
            Self {
                words: vec![0; words].into_boxed_slice(),
            }
        }

        pub(super) fn as_psid(&self) -> PSID {
            self.words.as_ptr().cast_mut().cast()
        }

        fn len(&self) -> u32 {
            // SAFETY: every constructor validates the SID held by this aligned
            // allocation.
            unsafe { GetLengthSid(self.as_psid()) }
        }

        fn copy_from(source: PSID) -> io::Result<Self> {
            // SAFETY: source comes from a successful token-information call.
            if unsafe { IsValidSid(source) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows returned an invalid user SID",
                ));
            }

            // SAFETY: IsValidSid established that this is a readable SID.
            let length = unsafe { GetLengthSid(source) };
            let sid = Self::zeroed(length);
            // SAFETY: the destination is length bytes and source is a valid SID
            // of exactly that reported length.
            if unsafe { CopySid(length, sid.as_psid(), source) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(sid)
        }
    }

    /// An aligned ACL whose length is carried in its own header.
    struct OwnedAcl {
        words: Box<[usize]>,
    }

    impl OwnedAcl {
        fn as_mut_ptr(&mut self) -> *mut ACL {
            self.words.as_mut_ptr().cast()
        }

        fn as_ptr(&self) -> *const ACL {
            self.words.as_ptr().cast()
        }
    }

    /// The current process token's user, copied out before its token closes.
    pub(super) fn process_user() -> io::Result<OwnedSid> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a borrowed pseudo-handle and token is a
        // valid out pointer receiving an owned handle on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);

        let mut length = 0;
        // SAFETY: a zero-sized first query is how GetTokenInformation reports
        // the required TOKEN_USER allocation.
        let queried =
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
        if queried != 0 {
            return Err(io::Error::other(
                "Windows returned token-user data without a buffer",
            ));
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(source);
        }

        let words = (length as usize).div_ceil(size_of::<usize>());
        let mut information = vec![0usize; words];
        // SAFETY: the aligned buffer is at least length bytes and the token is
        // live for the call.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                information.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: a successful TokenUser query initializes a TOKEN_USER at the
        // start of the suitably aligned buffer.
        let user = unsafe { information.as_ptr().cast::<TOKEN_USER>().read() };
        OwnedSid::copy_from(user.User.Sid)
    }

    /// Builds the one-ACE DACL written by ganja.
    fn private_acl(user: &OwnedSid, flags: u32) -> io::Result<OwnedAcl> {
        let bytes = size_of::<ACL>()
            .checked_add(size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>())
            .and_then(|bytes| bytes.checked_add(user.len() as usize))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("the private DACL is too large"))?;
        let words = (bytes as usize).div_ceil(size_of::<usize>());
        let mut acl = OwnedAcl {
            words: vec![0; words].into_boxed_slice(),
        };

        // SAFETY: acl owns an aligned allocation of bytes bytes.
        if unsafe { InitializeAcl(acl.as_mut_ptr(), bytes, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the initialized ACL has room for this SID-bearing ACE and
        // user is a live, valid SID.
        if unsafe {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr(),
                ACL_REVISION,
                flags,
                FILE_ALL_ACCESS,
                user.as_psid(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(acl)
    }

    /// Severs inheritance and grants the process user alone full control.
    pub(super) fn seal_private(file: &fs::File) -> io::Result<()> {
        let user = process_user()?;
        let acl = private_acl(&user, 0)?;
        // SAFETY: file is live, its create access included WRITE_DAC, and acl
        // stays live for the duration of SetSecurityInfo.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl.as_ptr(),
                ptr::null(),
            )
        };
        win32(status)
    }

    /// Creates the leaf with an owner-only DACL inherited by its children.
    pub(super) fn create_dir_private(dir: &Path) -> io::Result<()> {
        let name = dir.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the private spill directory needs a leaf name",
            )
        })?;
        let parent = dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;

        // Canonicalizing the now-existing parent preserves std's `\\?\`
        // conversion before this raw Win32 leaf create, including UNC paths
        // and paths beyond the legacy CreateDirectoryW limit.
        let path = fs::canonicalize(parent)?.join(name);
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a spill directory path contains a NUL",
            ));
        }
        wide.push(0);

        let user = process_user()?;
        let acl = private_acl(&user, OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)?;
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_pointer = ptr::addr_of_mut!(descriptor).cast();
        // SAFETY: descriptor is live writable storage for an absolute security
        // descriptor and the documented revision initializes that storage.
        if unsafe { InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor is initialized and acl remains live through the
        // CreateDirectoryW call that consumes these attributes.
        if unsafe { SetSecurityDescriptorDacl(descriptor_pointer, 1, acl.as_ptr(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor is initialized; setting the protected bit is what
        // prevents a parent grant from widening the one-ACE DACL at birth.
        if unsafe {
            SetSecurityDescriptorControl(descriptor_pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_pointer,
            bInheritHandle: 0,
        };
        // SAFETY: wide is NUL-terminated, attributes points to the live
        // descriptor, and every SID/ACL allocation outlives this call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } != 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn win32(status: WIN32_ERROR) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

/// The ganja data directory's `tool-output` subdirectory, or [`None`] when
/// there is no home directory to resolve it against.
fn default_dir() -> Option<PathBuf> {
    let base = Xdg::new().ok()?;
    Some(base.data_dir().join(DIRECTORY).join(TOOL_OUTPUT))
}

/// Directories a truncating [`clamp`] tries, in order: the resolved data
/// directory when there is one, then a process temp directory, which is
/// nearly always writable even where a home directory is not (a sandboxed
/// or read-only-home environment, for instance).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    dirs.extend(default_dir());
    dirs.push(std::env::temp_dir().join(DIRECTORY).join(TOOL_OUTPUT));
    dirs
}

/// Deletes spilled output older than `MAX_AGE` from every directory a clamp
/// might have written one to, and answers with how many files went.
///
/// There is nothing here to report as an error. A directory that does not
/// exist has nothing to sweep; a file that refuses to be deleted belongs to
/// somebody else, and the next round will try it again. Failing a session over
/// either would be absurd — this is housekeeping.
#[must_use]
pub fn sweep() -> usize {
    candidate_dirs().into_iter().map(|dir| sweep_in(&dir)).sum()
}

/// The same sweep over exactly `dir` — no XDG resolution and no temp-dir
/// fallback — so a test can assert on what a sweep removes without reaching
/// into a real person's data directory. Mirrors `clamp_with` and
/// [`open_spill_in`], which exist for the same reason.
pub(crate) fn sweep_in(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;

    for entry in entries.flatten() {
        // The prefix is the whole permission this sweep has. A directory it
        // shares with anything else — the temp fallback is `/tmp` on a machine
        // with no data directory — holds files that are none of its business,
        // and it may not so much as stat them by mistake.
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(PREFIX.as_bytes())
        {
            continue;
        }
        let path = entry.path();
        // A link's own stamp decides a link's fate, never its target's, and
        // removing the name leaves whatever it pointed at exactly as it was.
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if metadata.is_dir() {
            // Nothing this module creates, so nothing this sweep understands.
            continue;
        }
        // No stamp, or a stamp in the future, reads as "not old enough" — a
        // sweep that cannot tell how old a file is does not delete it.
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MAX_AGE);
        if !stale {
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::debug!(%error, file = %path.display(), "a spilled tool output stayed");
            }
        }
    }

    removed
}

/// Sweeps once, then once an hour, until `cancel` fires.
///
/// Shaped after `ganja-core`'s `catalog::spawn_refresh_loop`, with one
/// difference:
/// the first round runs inside the spawned task rather than on the calling
/// thread. The catalog's first step installs the table a frontend's first
/// frame prices against; nothing at all waits on a sweep, so nothing is gained
/// by making a startup path wait for a directory scan.
///
/// # Panics
///
/// Through [`tokio::spawn`], when called outside a runtime.
pub fn spawn_sweep_loop(cancel: CancellationToken) {
    tokio::spawn(async move {
        loop {
            match tokio::task::spawn_blocking(sweep).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "old spilled tool output was deleted"),
                Err(error) => tracing::warn!(%error, "the spilled output was not swept"),
            }

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(SWEEP_REPEAT) => {}
            }
        }
    });
}

/// A name unique within a process and ordered by creation, carrying the
/// [`PREFIX`] upstream's cleanup sweep looks for and [`sweep`] ports.
///
/// The counter is what keeps two clamps in the same nanosecond from
/// colliding — a real possibility on a coarser clock, and free insurance
/// against one here regardless.
fn overflow_filename() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());

    format!("tool_{stamp:x}_{count:x}")
}

#[cfg(test)]
#[path = "truncate_tests.rs"]
mod tests;
