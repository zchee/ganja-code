#![cfg(windows)]

use std::{
    env,
    ffi::{OsString, c_void},
    fs, io,
    mem::size_of,
    os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _},
    ptr,
};

use ganja_provider::auth::{AuthError, credential_for, set_credential, store_path};
use tempfile::TempDir;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LocalFree, WIN32_ERROR,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
        CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid,
        GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, IsValidSid,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED, SECURITY_MAX_SID_SIZE,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinWorldSid,
    },
    Storage::FileSystem::{FILE_ALL_ACCESS, FILE_GENERIC_READ, READ_CONTROL, WRITE_DAC},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const CANARY: &str = "sk-windows-acl-canary-8842";

struct Environment {
    data_home: Option<OsString>,
    api_key: Option<OsString>,
}

impl Environment {
    fn isolated(directory: &TempDir) -> Self {
        let saved = Self {
            data_home: env::var_os("XDG_DATA_HOME"),
            api_key: env::var_os("ANTHROPIC_API_KEY"),
        };
        // SAFETY: this integration binary has one test, so no other test
        // thread can observe or mutate these process-wide variables.
        unsafe {
            env::set_var("XDG_DATA_HOME", directory.path());
            env::remove_var("ANTHROPIC_API_KEY");
        }
        saved
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        // SAFETY: this integration binary has one test, and this guard restores
        // the values it alone replaced.
        unsafe {
            match self.data_home.take() {
                Some(value) => env::set_var("XDG_DATA_HOME", value),
                None => env::remove_var("XDG_DATA_HOME"),
            }
            match self.api_key.take() {
                Some(value) => env::set_var("ANTHROPIC_API_KEY", value),
                None => env::remove_var("ANTHROPIC_API_KEY"),
            }
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the successful OpenProcessToken result.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

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

struct OwnedSid {
    words: Box<[usize]>,
}

impl OwnedSid {
    fn zeroed(bytes: u32) -> Self {
        let words = (bytes as usize).div_ceil(size_of::<usize>());
        Self {
            words: vec![0; words].into_boxed_slice(),
        }
    }

    fn as_psid(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }

    fn len(&self) -> u32 {
        // SAFETY: every constructor leaves a valid SID in the allocation.
        unsafe { GetLengthSid(self.as_psid()) }
    }

    fn copy_from(source: PSID) -> io::Result<Self> {
        // SAFETY: source came from a successful TokenUser query.
        if unsafe { IsValidSid(source) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an invalid user SID",
            ));
        }
        // SAFETY: IsValidSid established that source is readable.
        let length = unsafe { GetLengthSid(source) };
        let sid = Self::zeroed(length);
        // SAFETY: the destination has length bytes and source is that long.
        if unsafe { CopySid(length, sid.as_psid(), source) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }

    fn everyone() -> io::Result<Self> {
        let mut length = SECURITY_MAX_SID_SIZE;
        let sid = Self::zeroed(length);
        // SAFETY: the allocation is the documented maximum SID size.
        if unsafe { CreateWellKnownSid(WinWorldSid, ptr::null_mut(), sid.as_psid(), &mut length) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }
}

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

fn process_user() -> io::Result<OwnedSid> {
    let mut token = ptr::null_mut();
    // SAFETY: token is a valid out pointer and GetCurrentProcess is borrowed.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut length = 0;
    // SAFETY: a zero-sized first query asks for the required buffer length.
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
    // SAFETY: the aligned buffer is at least length bytes and token is live.
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
    // SAFETY: TokenUser initialized a TOKEN_USER at the aligned buffer start.
    let user = unsafe { information.as_ptr().cast::<TOKEN_USER>().read() };
    OwnedSid::copy_from(user.User.Sid)
}

fn exposed_acl(user: &OwnedSid, everyone: &OwnedSid) -> io::Result<OwnedAcl> {
    let ace = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    let bytes = size_of::<ACL>() + ace * 2 + user.len() as usize + everyone.len() as usize;
    let words = bytes.div_ceil(size_of::<usize>());
    let mut acl = OwnedAcl {
        words: vec![0; words].into_boxed_slice(),
    };
    // SAFETY: the aligned allocation is bytes bytes long.
    if unsafe { InitializeAcl(acl.as_mut_ptr(), bytes as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    for (sid, mask) in [(user, FILE_ALL_ACCESS), (everyone, FILE_GENERIC_READ)] {
        // SAFETY: the ACL was sized for both ACEs and sid remains live.
        if unsafe { AddAccessAllowedAceEx(acl.as_mut_ptr(), ACL_REVISION, 0, mask, sid.as_psid()) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(acl)
}

fn assert_owner_only(file: &fs::File, user: &OwnedSid) {
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: file is live with READ_CONTROL and the requested out pointers
    // remain valid for the call.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let _descriptor = LocalAllocation(descriptor);
    win32(status).expect("the store security descriptor reads");
    assert!(!dacl.is_null(), "the store must not have a NULL DACL");

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is the live successful GetSecurityInfo allocation.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0,
        "the descriptor control reads: {}",
        io::Error::last_os_error()
    );
    assert_ne!(
        control & SE_DACL_PROTECTED,
        0,
        "the store DACL must sever inheritance"
    );

    // SAFETY: GetSecurityInfo returned a valid ACL header.
    let header = unsafe { &*dacl };
    assert_eq!(header.AceCount, 1, "only the token user should be granted");
    let mut raw = ptr::null_mut();
    // SAFETY: index zero exists by the assertion above.
    assert_ne!(
        unsafe { GetAce(dacl, 0, &mut raw) },
        0,
        "the owner ACE reads: {}",
        io::Error::last_os_error()
    );
    // SAFETY: ganja writes an ACCESS_ALLOWED_ACE at index zero.
    let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
    let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);
    // SAFETY: both pointers identify live, valid SIDs.
    assert_ne!(unsafe { EqualSid(sid, user.as_psid()) }, 0);
}

fn win32(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

#[test]
fn the_windows_store_is_owner_only_and_refuses_an_everyone_read_grant() {
    let directory = TempDir::new().expect("a temporary directory is creatable");
    let _environment = Environment::isolated(&directory);

    set_credential("anthropic", CANARY).expect("the key stores");
    let path = store_path().expect("the store path resolves");
    assert!(
        path.starts_with(directory.path()),
        "the test must not reach the real store: {}",
        path.display()
    );

    let file = fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .open(&path)
        .expect("the owner can inspect and replace the DACL");
    let user = process_user().expect("the process has a user SID");
    assert_owner_only(&file, &user);

    let everyone = OwnedSid::everyone().expect("Everyone has a SID");
    let acl = exposed_acl(&user, &everyone).expect("the exposed ACL builds");
    // SAFETY: file has WRITE_DAC, acl is live, and every unused SID pointer is
    // deliberately null.
    win32(unsafe {
        SetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl.as_ptr(),
            ptr::null(),
        )
    })
    .expect("the Everyone read grant is planted");
    drop(file);

    let error = credential_for("anthropic").expect_err("the exposed store is refused");
    let grantee = match &error {
        AuthError::Permissions { grantee, .. } => grantee,
        other => panic!("the exposure should be a permissions error, got {other:?}"),
    };
    assert!(
        grantee.contains("Everyone") || grantee == "S-1-1-0",
        "the offending grantee should be named, got {grantee}"
    );

    let explanation = error.to_string();
    assert!(explanation.contains(grantee));
    assert!(explanation.contains("icacls \""));
    assert!(explanation.contains("/inheritance:r"));
    assert!(explanation.contains("/grant:r \"%USERNAME%:F\""));
    let mut rendered = Some(&error as &dyn std::error::Error);
    while let Some(line) = rendered {
        assert!(
            !line.to_string().contains(CANARY),
            "a permissions error must not carry the key: {line}"
        );
        rendered = line.source();
    }
}
