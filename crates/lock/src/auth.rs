//! Authentication. The UI never talks to PAM directly: it hands a [`Secret`] to an
//! [`Authenticator`] on a worker thread and gets an [`AuthOutcome`] back. Only
//! [`AuthOutcome::Success`] can ever lead to an unlock; errors and panics are failures.

use std::sync::Arc;

use crate::secret::Secret;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The credentials were accepted.
    Success,
    /// Wrong password (or the account may not log in).
    Denied,
    /// PAM itself failed or is unavailable. Counts as a failed attempt, never as success.
    Error(String),
}

/// Checks a password for `user`. Runs on a worker thread and may block (PAM modules
/// delay failures on purpose), so it must be `Send + Sync`.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, user: &str, secret: &Secret) -> AuthOutcome;

    /// Short description for the startup log (`pam service=login`, `stub`).
    fn describe(&self) -> String;
}

/// Runs an authentication and turns a panic inside it into an error outcome.
pub fn run_guarded(auth: &Arc<dyn Authenticator>, user: &str, secret: &Secret) -> AuthOutcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        auth.authenticate(user, secret)
    }))
    .unwrap_or_else(|_| AuthOutcome::Error("authenticator panicked".into()))
}

/// The authenticator this build uses: PAM when it was linked, else the refusing stub.
pub fn default_authenticator(service: &str) -> Arc<dyn Authenticator> {
    #[cfg(aurora_pam)]
    {
        Arc::new(pam::Pam::new(service))
    }
    #[cfg(not(aurora_pam))]
    {
        let _ = service;
        Arc::new(Stub)
    }
}

/// Whether the real PAM authenticator was compiled in.
pub const HAS_PAM: bool = cfg!(aurora_pam);

/// Fail-closed stand-in used when libpam was not available at build time: it accepts
/// nothing, ever.
#[cfg_attr(aurora_pam, allow(dead_code))]
pub struct Stub;

impl Authenticator for Stub {
    fn authenticate(&self, _: &str, _: &Secret) -> AuthOutcome {
        AuthOutcome::Error("built without PAM, refusing to unlock".into())
    }

    fn describe(&self) -> String {
        "STUB (no PAM: can never unlock)".into()
    }
}

/// The login name of the current user, from the password database (not `$USER`, which a
/// process environment can fake).
pub fn current_user() -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers refer to live locals; `buf` outlives the use of `pwd`'s strings.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut out,
        )
    };
    if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    name.to_str().ok().map(str::to_owned)
}

#[cfg(aurora_pam)]
mod pam {
    //! Minimal libpam FFI: `pam_start` with a conversation that answers every password
    //! prompt with the secret, `pam_authenticate`, `pam_acct_mgmt`, `pam_end`.

    use std::ffi::{CString, c_char, c_int, c_void};

    use super::{AuthOutcome, Authenticator};
    use crate::secret::Secret;

    const PAM_SUCCESS: c_int = 0;
    const PAM_BUF_ERR: c_int = 5;
    const PAM_AUTH_ERR: c_int = 7;
    const PAM_USER_UNKNOWN: c_int = 10;
    const PAM_MAXTRIES: c_int = 11;
    const PAM_CONV_ERR: c_int = 19;
    const PAM_NEW_AUTHTOK_REQD: c_int = 12;
    const PAM_ACCT_EXPIRED: c_int = 13;
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_ERROR_MSG: c_int = 3;
    const PAM_TEXT_INFO: c_int = 4;
    const PAM_DISALLOW_NULL_AUTHTOK: c_int = 1;

    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    type ConvFn =
        extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

    #[repr(C)]
    struct PamConv {
        conv: ConvFn,
        appdata_ptr: *mut c_void,
    }

    #[repr(C)]
    struct PamHandle {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        fn pam_start(
            service: *const c_char,
            user: *const c_char,
            conv: *const PamConv,
            handle: *mut *mut PamHandle,
        ) -> c_int;
        fn pam_authenticate(handle: *mut PamHandle, flags: c_int) -> c_int;
        fn pam_acct_mgmt(handle: *mut PamHandle, flags: c_int) -> c_int;
        fn pam_end(handle: *mut PamHandle, status: c_int) -> c_int;
    }

    /// What the conversation needs: the password bytes, borrowed for one call.
    struct ConvData<'a> {
        secret: &'a Secret,
    }

    /// Frees the first `n` responses, wiping any password copy.
    ///
    /// # Safety
    /// `resp` must be a `calloc`ed array of at least `n` entries whose `resp` fields are
    /// null or `malloc`ed NUL-terminated strings.
    unsafe fn free_responses(resp: *mut PamResponse, n: usize) {
        for i in 0..n {
            // SAFETY: per the contract, `resp[i]` is valid.
            let r = unsafe { &mut *resp.add(i) };
            if !r.resp.is_null() {
                // SAFETY: `r.resp` is a NUL-terminated malloc string.
                let len = unsafe { libc::strlen(r.resp) };
                // SAFETY: writing zeros over our own allocation.
                unsafe { std::ptr::write_bytes(r.resp, 0, len) };
                // SAFETY: allocated with malloc in `conversation`.
                unsafe { libc::free(r.resp.cast()) };
                r.resp = std::ptr::null_mut();
            }
        }
        // SAFETY: the array itself came from calloc.
        unsafe { libc::free(resp.cast()) };
    }

    extern "C" fn conversation(
        n: c_int,
        msgs: *mut *const PamMessage,
        out: *mut *mut PamResponse,
        data: *mut c_void,
    ) -> c_int {
        if n <= 0 || msgs.is_null() || out.is_null() || data.is_null() {
            return PAM_CONV_ERR;
        }
        let n = n as usize;
        // SAFETY: PAM passes back the `appdata_ptr` we gave it, a live `ConvData`.
        let data = unsafe { &*(data as *const ConvData) };
        // SAFETY: calloc of `n` zeroed responses; PAM frees it (and each `resp`) with free.
        let resp =
            unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
        if resp.is_null() {
            return PAM_BUF_ERR;
        }
        for i in 0..n {
            // SAFETY: PAM guarantees `n` valid message pointers.
            let msg = unsafe { &**msgs.add(i) };
            match msg.msg_style {
                PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                    let bytes = data.secret.as_bytes();
                    // SAFETY: malloc of len + 1 bytes, filled below and NUL terminated.
                    let buf = unsafe { libc::malloc(bytes.len() + 1) } as *mut u8;
                    if buf.is_null() {
                        // SAFETY: `resp` has `n` entries, the first `i` possibly filled.
                        unsafe { free_responses(resp, n) };
                        return PAM_BUF_ERR;
                    }
                    // SAFETY: `buf` has room for `bytes.len() + 1`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
                        *buf.add(bytes.len()) = 0;
                        (*resp.add(i)).resp = buf.cast();
                    }
                }
                PAM_ERROR_MSG | PAM_TEXT_INFO => {}
                _ => {
                    // SAFETY: as above.
                    unsafe { free_responses(resp, n) };
                    return PAM_CONV_ERR;
                }
            }
        }
        // SAFETY: `out` is a valid out pointer from PAM.
        unsafe { *out = resp };
        PAM_SUCCESS
    }

    pub struct Pam {
        service: CString,
    }

    impl Pam {
        pub fn new(service: &str) -> Self {
            // A service name with a NUL cannot be valid: fall back to the default.
            let service = CString::new(service)
                .unwrap_or_else(|_| CString::new("login").expect("literal has no NUL"));
            Self { service }
        }
    }

    fn describe_rc(rc: c_int) -> String {
        match rc {
            PAM_USER_UNKNOWN => "unknown user".into(),
            PAM_MAXTRIES => "too many attempts".into(),
            PAM_ACCT_EXPIRED => "account expired".into(),
            PAM_NEW_AUTHTOK_REQD => "password change required".into(),
            other => format!("pam error {other}"),
        }
    }

    impl Authenticator for Pam {
        fn authenticate(&self, user: &str, secret: &Secret) -> AuthOutcome {
            let Ok(user) = CString::new(user) else {
                return AuthOutcome::Error("invalid user name".into());
            };
            let data = ConvData { secret };
            let conv = PamConv {
                conv: conversation,
                appdata_ptr: (&data as *const ConvData).cast_mut().cast(),
            };
            let mut handle: *mut PamHandle = std::ptr::null_mut();
            // SAFETY: valid C strings and conversation; `data` and `conv` outlive the handle
            // because `pam_end` runs before this function returns.
            let rc = unsafe { pam_start(self.service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
            if rc != PAM_SUCCESS || handle.is_null() {
                return AuthOutcome::Error(format!("pam_start: {}", describe_rc(rc)));
            }
            // SAFETY: `handle` is valid until `pam_end`.
            let mut rc = unsafe { pam_authenticate(handle, PAM_DISALLOW_NULL_AUTHTOK) };
            if rc == PAM_SUCCESS {
                // SAFETY: as above. Expired or disabled accounts must not unlock either.
                rc = unsafe { pam_acct_mgmt(handle, 0) };
            }
            // SAFETY: ends the transaction and invalidates `handle`.
            unsafe { pam_end(handle, rc) };
            match rc {
                PAM_SUCCESS => AuthOutcome::Success,
                PAM_AUTH_ERR | PAM_MAXTRIES | PAM_USER_UNKNOWN | PAM_ACCT_EXPIRED => {
                    AuthOutcome::Denied
                }
                other => AuthOutcome::Error(describe_rc(other)),
            }
        }

        fn describe(&self) -> String {
            format!("pam service={}", self.service.to_string_lossy())
        }
    }
}
