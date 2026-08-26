use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::{MAX_CHARS, MAX_LINES, Truncated, clamp, clamp_with};

/// A day, for ages a person can read.
const DAY: u64 = 24 * 60 * 60;

/// Writes a file under `dir` and backdates it by `age`.
fn plant(dir: &Path, name: &str, age: Duration) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "spilled").expect("the fixture writes");
    stamp(
        &path,
        SystemTime::now()
            .checked_sub(age)
            .expect("a representable stamp"),
    );

    path
}

/// Moves `path`'s modification stamp to `when`, directory or file alike.
///
/// A file is opened for writing, because a stamp is metadata a handle must
/// be allowed to write and Windows grants that only to a handle that asked
/// for write access. A directory refuses a write handle on unix — and only
/// unix backdates one here — so the read-only open is kept as the fallback
/// rather than as the first choice.
fn stamp(path: &Path, when: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| std::fs::File::open(path))
        .and_then(|handle| handle.set_modified(when))
        .expect("the fixture can move the stamp");
}

/// What a sweep may and may not delete, in one table: the `tool_` prefix
/// and nothing else, past the week and not before it.
#[test]
fn a_sweep_deletes_old_spills_and_leaves_everything_else_alone() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let cases = [
        ("tool_1a2b_0", Duration::from_secs(8 * DAY), false),
        ("tool_3c4d_1", Duration::from_secs(6 * DAY), true),
        ("tool_5e6f_2", Duration::ZERO, true),
        ("notes.txt", Duration::from_secs(400 * DAY), true),
        ("tool-output.log", Duration::from_secs(400 * DAY), true),
        ("TOOL_shouting", Duration::from_secs(400 * DAY), true),
    ];
    for (name, age, _) in cases {
        plant(dir.path(), name, age);
    }

    assert_eq!(
        super::sweep_in(dir.path()),
        1,
        "exactly the one stale spill goes"
    );

    for (name, age, survives) in cases {
        assert_eq!(
            dir.path().join(name).exists(),
            survives,
            "{name}, {} days old",
            age.as_secs() / DAY
        );
    }
}

/// **D81.** A stamp in the future is not an age. Clock skew across a
/// shared filesystem is the ordinary way one arrives, and a sweep that
/// cannot say how old a file is has no business deleting it — least of
/// all silently, since what it would be deleting is a spill some live
/// tool call may still be pointing the model at.
#[test]
fn a_spill_stamped_in_the_future_is_kept() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ahead = dir.path().join("tool_fromtomorrow");
    std::fs::write(&ahead, "spilled").expect("the fixture writes");
    stamp(
        &ahead,
        SystemTime::now()
            .checked_add(Duration::from_secs(400 * DAY))
            .expect("a representable stamp"),
    );

    assert_eq!(
        super::sweep_in(dir.path()),
        0,
        "a stamp a sweep cannot subtract from is not an age"
    );
    assert!(ahead.exists());
}

/// **D80, the directory half.** Nothing here creates a directory under
/// this prefix, so an aged one is somebody else's — and the sweep neither
/// removes it nor treats its contents as spills to age out. Backdating
/// needs a descriptor on the directory itself, which is a unix affordance.
#[cfg(unix)]
#[test]
fn an_aged_directory_under_the_prefix_is_neither_entered_nor_removed() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let planted = dir.path().join("tool_notaspill");
    std::fs::create_dir(&planted).expect("the fixture directory is creatable");
    // A file inside it that WOULD be swept if the sweep descended.
    let inside = plant(&planted, "tool_inside", Duration::from_secs(400 * DAY));
    stamp(
        &planted,
        SystemTime::now()
            .checked_sub(Duration::from_secs(400 * DAY))
            .expect("a representable stamp"),
    );

    assert_eq!(super::sweep_in(dir.path()), 0);
    assert!(planted.is_dir(), "the directory itself stays");
    assert!(
        inside.exists(),
        "and the sweep never went in: this file is older than anything it deletes"
    );
}

/// Age is the entry's own, never its target's — which is what keeps a
/// sweep from reaching through a name somebody planted at it.
#[cfg(unix)]
#[test]
fn a_planted_link_is_judged_by_its_own_age_and_never_followed() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ancient = plant(dir.path(), "ancient.txt", Duration::from_secs(400 * DAY));
    let planted = dir.path().join("tool_planted");
    std::os::unix::fs::symlink(&ancient, &planted).expect("the link is creatable");

    assert_eq!(
        super::sweep_in(dir.path()),
        0,
        "a link created a moment ago is not a week old, whatever it points at"
    );
    assert!(
        std::fs::symlink_metadata(&planted).is_ok(),
        "the link itself is still there"
    );
    assert!(ancient.exists(), "and so is what it pointed at");
}

/// The other side of the same rule: a link old enough on its own account
/// really is removed — and what goes is the **name**. A sweep that
/// followed the link would delete a file nobody asked it to touch, which
/// is the whole point of planting one at a name this code creates.
#[cfg(unix)]
#[test]
fn an_aged_link_is_unlinked_and_what_it_pointed_at_is_not() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let target = plant(dir.path(), "precious.txt", Duration::ZERO);
    let planted = dir.path().join("tool_aged");
    std::os::unix::fs::symlink(&target, &planted).expect("the link is creatable");
    age_link(&planted, Duration::from_secs(8 * DAY));

    assert_eq!(super::sweep_in(dir.path()), 1);
    assert!(
        std::fs::symlink_metadata(&planted).is_err(),
        "the link is gone"
    );
    assert!(
        target.exists(),
        "and the file it pointed at, which is fresh and not even a spill, is untouched"
    );
}

/// Backdates a symbolic link's **own** stamp.
///
/// Not [`stamp`]: opening a link opens what it points at, so the ordinary
/// route would age the target and leave the link exactly as new as it was.
#[cfg(unix)]
fn age_link(path: &Path, ago: Duration) {
    use std::os::unix::ffi::OsStrExt as _;

    let seconds = SystemTime::now()
        .checked_sub(ago)
        .and_then(|when| when.duration_since(SystemTime::UNIX_EPOCH).ok())
        .expect("a representable stamp")
        .as_secs();
    let when = libc::timespec {
        tv_sec: seconds as libc::time_t,
        tv_nsec: 0,
    };
    let times = [when, when];
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path with no NUL");

    // SAFETY: both pointers are to locals that outlive the call, `times`
    // is the two-element array utimensat is specified to take, and
    // AT_SYMLINK_NOFOLLOW is what makes the stamp the link's own.
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(
        result,
        0,
        "the fixture can age a link: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn sweeping_a_directory_that_is_not_there_is_not_a_failure() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    assert_eq!(super::sweep_in(&dir.path().join("never-created")), 0);
}

/// The one file `dir` holds, panicking if that is not exactly true.
fn only_entry(dir: &Path) -> PathBuf {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the overflow directory was created")
        .map(|entry| entry.expect("a readable directory entry").path())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one overflow file in {dir:?}, got {entries:?}"
    );
    entries.remove(0)
}

/// The permission bits of `path`.
#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .expect("the path exists")
        .permissions()
        .mode()
        & 0o777
}

#[cfg(windows)]
mod windows_dacl {
    use std::{
        ffi::c_void,
        fs, io,
        os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _},
        path::Path,
        ptr, slice,
    };

    use windows_sys::Win32::{
        Foundation::{LocalFree, WIN32_ERROR},
        Security::{
            ACCESS_ALLOWED_ACE, ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
            GetSecurityDescriptorControl, GetSecurityDescriptorLength, INHERITED_ACE,
            OBJECT_INHERIT_ACE, SE_DACL_PROTECTED,
        },
        Storage::FileSystem::{FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL},
    };

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetSecurityInfo returns LocalAlloc-owned storage.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    struct Descriptor {
        _allocation: LocalAllocation,
        pointer: *mut c_void,
        dacl: *mut ACL,
    }

    impl Descriptor {
        fn read(path: &Path, directory: bool) -> Self {
            let flags = if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
            let file = fs::OpenOptions::new()
                .access_mode(READ_CONTROL)
                .custom_flags(flags)
                .open(path)
                .expect("the owner can inspect the security descriptor");
            let mut dacl = ptr::null_mut();
            let mut pointer = ptr::null_mut();
            // SAFETY: file is live with READ_CONTROL and the requested out
            // pointers remain valid for the call.
            let status = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle().cast(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut pointer,
                )
            };
            win32(status).expect("the security descriptor reads");

            Self {
                _allocation: LocalAllocation(pointer),
                pointer,
                dacl,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            // SAFETY: pointer is the live successful GetSecurityInfo
            // allocation held by _allocation.
            let length = unsafe { GetSecurityDescriptorLength(self.pointer) } as usize;
            assert_ne!(length, 0, "the descriptor has a serialized length");
            // SAFETY: Windows reported exactly length initialized bytes for
            // this security descriptor allocation.
            unsafe { slice::from_raw_parts(self.pointer.cast(), length) }.to_vec()
        }
    }

    pub(super) fn assert_owner_only(
        path: &Path,
        directory: bool,
        protected: Option<bool>,
        inherited: bool,
        inheritable: Option<bool>,
    ) {
        let descriptor = Descriptor::read(path, directory);
        assert!(!descriptor.dacl.is_null(), "the DACL must not be NULL");

        if let Some(protected) = protected {
            let mut control = 0;
            let mut revision = 0;
            // SAFETY: pointer is the live successful GetSecurityInfo
            // allocation held by descriptor.
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorControl(descriptor.pointer, &mut control, &mut revision)
                },
                0,
                "the descriptor control reads: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                control & SE_DACL_PROTECTED != 0,
                protected,
                "the DACL protection bit"
            );
        }

        // SAFETY: GetSecurityInfo returned a valid ACL header.
        let header = unsafe { &*descriptor.dacl };
        assert_eq!(
            header.AceCount, 1,
            "only the process user should receive access"
        );
        let mut raw = ptr::null_mut();
        // SAFETY: index zero exists by the assertion above.
        assert_ne!(
            unsafe { GetAce(descriptor.dacl, 0, &mut raw) },
            0,
            "the owner ACE reads: {}",
            io::Error::last_os_error()
        );
        // SAFETY: ganja writes an ACCESS_ALLOWED_ACE at index zero, and an
        // inherited copy retains that shape.
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
        let user = super::super::windows_acl::process_user().expect("the process has a user SID");
        assert_eq!(ace.Mask, FILE_ALL_ACCESS);
        // SAFETY: both pointers identify live, valid SIDs.
        assert_ne!(unsafe { EqualSid(sid, user.as_psid()) }, 0);

        let flags = u32::from(ace.Header.AceFlags);
        assert_eq!(
            flags & INHERITED_ACE != 0,
            inherited,
            "the ACE inheritance origin"
        );
        if let Some(inheritable) = inheritable {
            assert_eq!(
                flags & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE),
                if inheritable {
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
                } else {
                    0
                },
                "only a directory's grant should propagate to children"
            );
        }
    }

    pub(super) fn descriptor_bytes(path: &Path, directory: bool) -> Vec<u8> {
        Descriptor::read(path, directory).bytes()
    }

    fn win32(status: WIN32_ERROR) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

/// A spilled output holds whatever the tool read — an `env`, a `.env`, a
/// private repository's history — and [`super::candidate_dirs`] will fall
/// back to a world-readable `/tmp`. Neither the file nor the directory
/// this module creates may be readable by anyone else.
#[cfg(unix)]
#[test]
fn a_spilled_output_is_readable_only_by_its_owner() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    // A directory that does not exist yet, so what is asserted below is
    // the mode this module chose rather than the one tempfile did.
    let spill = dir.path().join("nested").join("tool-output");
    let long = "x".repeat(MAX_CHARS + 1);

    assert!(clamp_with(&long, &spill).truncated);

    assert_eq!(
        mode(&spill),
        0o700,
        "a spill directory this code created is the owner's alone"
    );
    assert_eq!(
        mode(&only_entry(&spill)),
        0o600,
        "a spilled tool output must not be readable by everyone on the machine"
    );
}

/// The Windows vocabulary for the unix mode test above: both descriptors
/// are protected and grant full control only to the process user, while
/// the directory grant is the one that may flow to children.
#[cfg(windows)]
#[test]
fn a_spilled_output_is_readable_only_by_its_owner() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let spill = dir.path().join("nested").join("tool-output");
    let long = "x".repeat(MAX_CHARS + 1);

    assert!(clamp_with(&long, &spill).truncated);

    windows_dacl::assert_owner_only(&spill, true, Some(true), false, Some(true));
    windows_dacl::assert_owner_only(&only_entry(&spill), false, Some(true), false, Some(false));
}

/// A child created through plain std I/O proves the directory descriptor,
/// rather than the spill file's own seal, is enough to keep new contents
/// owner-only from birth.
#[cfg(windows)]
#[test]
fn a_private_spill_directory_passes_its_owner_only_acl_to_a_plain_file() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let spill = dir.path().join("nested").join("tool-output");
    super::create_dir_private(&spill).expect("the private leaf is created");
    let child = spill.join("plain-child");

    std::fs::write(&child, "not sealed by write_private").expect("the plain child is born");

    windows_dacl::assert_owner_only(&child, false, None, true, None);
}

/// A directory somebody else made is theirs: seeing `ALREADY_EXISTS`
/// succeeds without replacing, protecting or otherwise rewriting its
/// security descriptor.
#[cfg(windows)]
#[test]
fn a_preexisting_spill_directory_keeps_its_security_descriptor() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let spill = dir.path().join("nested").join("tool-output");
    std::fs::create_dir_all(&spill).expect("the fixture directory is creatable");
    let before = windows_dacl::descriptor_bytes(&spill, true);

    super::create_dir_private(&spill).expect("an existing directory is accepted");

    assert_eq!(
        windows_dacl::descriptor_bytes(&spill, true),
        before,
        "the existing directory's descriptor must remain byte-identical"
    );
}

/// The spill directory can be a world-writable `/tmp`, where anyone may
/// plant a link at a name before this code creates it. Creating
/// exclusively is what makes that harmless: the link is unlinked, never
/// followed, and whatever it pointed at is left exactly as it was.
#[cfg(unix)]
#[test]
fn a_link_planted_at_the_spill_name_is_replaced_rather_than_followed() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "not yours to write").expect("the fixture writes");
    let planted = dir.path().join("planted");
    std::os::unix::fs::symlink(&victim, &planted).expect("the link is creatable");

    super::write_private(&planted, b"tool output").expect("the spill is written");

    assert_eq!(
        std::fs::read_to_string(&victim).expect("the victim still exists"),
        "not yours to write",
        "the spill followed a planted link and wrote through it"
    );
    assert!(
        !std::fs::symlink_metadata(&planted)
            .expect("the spill exists")
            .file_type()
            .is_symlink(),
        "the planted link should have been replaced by a real file"
    );
    assert_eq!(
        std::fs::read_to_string(&planted).expect("the spill is readable"),
        "tool output"
    );
    assert_eq!(mode(&planted), 0o600);
}

#[test]
fn candidate_dirs_tries_the_data_directory_before_the_temp_directory() {
    let dirs = super::candidate_dirs();

    assert!(
        !dirs.is_empty(),
        "the temp directory is always a candidate, even with no resolvable home"
    );
    let last = dirs.last().expect("checked non-empty above");
    assert_eq!(
        *last,
        std::env::temp_dir()
            .join(super::DIRECTORY)
            .join(super::TOOL_OUTPUT),
        "the temp directory is always the last resort"
    );
    if dirs.len() > 1 {
        assert_ne!(
            dirs[0], *last,
            "a resolvable data directory must be tried before the temp fallback"
        );
    }
}

#[test]
fn short_output_passes_through_untouched() {
    assert_eq!(
        clamp("hello"),
        Truncated {
            text: "hello".to_owned(),
            truncated: false,
        }
    );
    assert_eq!(
        clamp(""),
        Truncated {
            text: String::new(),
            truncated: false,
        }
    );
}

#[test]
fn exactly_at_both_budgets_is_not_truncated() {
    let at_lines = "a\n".repeat(MAX_LINES - 1) + "a";
    assert_eq!(at_lines.split('\n').count(), MAX_LINES);
    assert!(!clamp(&at_lines).truncated);

    let at_bytes = "x".repeat(MAX_CHARS);
    assert!(!clamp(&at_bytes).truncated);
}

#[test]
fn output_over_the_byte_budget_is_cut_and_says_so() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "x".repeat(MAX_CHARS + 1);

    let clamped = clamp_with(&long, dir.path());
    assert!(clamped.truncated);
    assert!(
        clamped.text.contains("bytes truncated"),
        "got {:?}",
        clamped.text
    );
}

#[test]
fn output_over_the_line_budget_is_cut_at_the_budget() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "line\n".repeat(MAX_LINES + 10);

    let clamped = clamp_with(&long, dir.path());
    assert!(clamped.truncated);
    assert!(
        clamped.text.contains("lines truncated"),
        "a line-budget cut reports lines, not bytes: {:?}",
        clamped.text
    );
    assert_eq!(
        clamped
            .text
            .lines()
            .take_while(|line| *line == "line")
            .count(),
        MAX_LINES
    );
}

/// A caller with its own byte budget — smaller than [`MAX_CHARS`] — is
/// clamped at exactly that budget, not at the module's own default: at
/// the budget, nothing is cut; one byte over it, the whole single line is
/// removed rather than partially kept, same as
/// `a_huge_single_line_keeps_no_preview_but_still_reports_the_full_size`
/// (no newline means the line-count budget never applies, so the byte
/// budget alone decides, and it decides on the very first line).
#[test]
fn clamp_bytes_honors_a_budget_smaller_than_the_default() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let text = "x".repeat(200);

    assert!(
        !super::clamp_bytes_with(&text, 200, dir.path()).truncated,
        "exactly at the budget is not truncated"
    );

    let over = super::clamp_bytes_with(&text, 199, dir.path());
    assert!(over.truncated);
    assert!(
        over.text.contains("200 bytes truncated"),
        "got {:?}",
        over.text
    );
}

/// [`clamp_bytes_with`] spills the full original text, exactly as
/// [`clamp_with`] does at its own budget — proven with a small budget so
/// the assertion cannot pass by accident against [`MAX_CHARS`].
#[test]
fn clamp_bytes_spills_the_full_original_at_its_own_budget() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let text = "y".repeat(500);

    let clamped = super::clamp_bytes_with(&text, 50, dir.path());

    assert!(clamped.truncated);
    assert!(
        clamped.text.contains("500 bytes truncated"),
        "got {:?}",
        clamped.text
    );
    let file = only_entry(dir.path());
    assert_eq!(
        std::fs::read_to_string(&file).expect("the overflow file was written"),
        text,
        "the spill must hold the full original, not the clamped preview"
    );
}

#[test]
fn a_clamp_never_splits_a_code_point() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "\u{1F980}".repeat(MAX_CHARS);

    let clamped = clamp_with(&long, dir.path());
    assert!(clamped.truncated);
    assert!(clamped.text.contains("bytes truncated"));
    // The assertion of interest: building this `Truncated` at all did not
    // panic on a byte index that split the emoji's 4-byte encoding.
}

#[test]
fn a_huge_single_line_keeps_no_preview_but_still_reports_the_full_size() {
    // No newline at all, so the line-count budget never applies; only the
    // byte budget can trigger, and it triggers on the very first line.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "x".repeat(MAX_CHARS * 2);

    let clamped = clamp_with(&long, dir.path());
    assert!(clamped.truncated);
    assert!(
        clamped
            .text
            .starts_with(&format!("\n\n...{} bytes truncated...", MAX_CHARS * 2)),
        "got {:?}",
        clamped.text
    );
}

#[test]
fn the_overflow_file_holds_the_full_untouched_original_not_the_preview() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "x".repeat(MAX_CHARS * 2);

    let clamped = clamp_with(&long, dir.path());

    let file = only_entry(dir.path());
    assert_eq!(
        std::fs::read_to_string(&file).expect("the overflow file was written"),
        long,
        "the file must hold everything, not just the clamped preview"
    );
    assert!(
        clamped.text.contains(&file.display().to_string()),
        "the notice must name the exact file that was written: {:?}",
        clamped.text
    );
}

#[test]
fn the_notice_names_the_overflow_file_and_how_to_read_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let long = "line\n".repeat(MAX_LINES + 10);

    let clamped = clamp_with(&long, dir.path());

    assert!(
        clamped.text.contains(
            "The tool call succeeded but the output was truncated. Full output saved to:"
        ),
        "got {:?}",
        clamped.text
    );
    assert!(
            clamped
                .text
                .contains("Use Grep to search the full content or Read with offset/limit to view specific sections."),
            "got {:?}",
            clamped.text
        );
}

#[test]
fn a_write_that_cannot_succeed_degrades_to_the_pathless_notice_rather_than_failing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    // A regular file sits where the overflow directory would need to be
    // created, so `create_dir_all` fails on it — the same failure shape
    // as a read-only or missing home directory in the field.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "not a directory").expect("the fixture writes");
    let long = "x".repeat(MAX_CHARS + 1);

    let clamped = clamp_with(&long, &blocked);

    assert!(clamped.truncated, "the budget was still exceeded");
    assert!(
        !clamped.text.contains("Full output saved to:"),
        "a failed write must degrade silently, not claim a file exists: {:?}",
        clamped.text
    );
    assert!(
        clamped.text.contains("bytes truncated"),
        "the pathless notice is still the one from clamp_body: {:?}",
        clamped.text
    );
}
