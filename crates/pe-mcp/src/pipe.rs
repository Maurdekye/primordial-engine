//! The default transport (design 17.1): the harness launches `pe-mcp bridge`, which connects to the
//! engine over a named pipe (Windows) or a Unix socket (Linux) and relays stdio. No TCP port is
//! opened.
//!
//! * Windows: the pipe's DACL grants access to the current user's SID only; the first instance is
//!   created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (no squatting on the name); remote clients are
//!   refused.
//! * Unix: the socket lives in a fresh 0700 directory, the socket file is 0600, and the peer's uid
//!   (SO_PEERCRED) must equal ours.
//!
//! Then the token: the bridge's first line is `PE-MCP-AUTH <token>`; the engine answers `OK` and
//! the connection carries MCP from then on, as that token's principal, or `ERR unauthorized` and
//! closes. The token is re-checked on every call, so revocation takes effect at once.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::auth::TokenStore;
use crate::server::{Api, Auth, WorldServer};

pub const HANDSHAKE: &str = "PE-MCP-AUTH ";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HANDSHAKE: u64 = 256;

/// Authenticate one connection, then serve MCP on it until the client goes away.
pub async fn serve_connection<S>(api: Api, store: Arc<TokenStore>, stream: S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    let mut line = String::new();
    let read = tokio::time::timeout(HANDSHAKE_TIMEOUT, (&mut r).take(MAX_HANDSHAKE).read_line(&mut line)).await;
    let principal = match read {
        Ok(Ok(_)) if line.ends_with('\n') => {
            line.strip_prefix(HANDSHAKE).and_then(|t| store.authenticate(t.trim_end()).ok())
        }
        _ => None,
    };
    let Some(p) = principal else {
        let _ = w.write_all(b"ERR unauthorized\n").await;
        let _ = w.shutdown().await;
        return Ok(());
    };
    w.write_all(b"OK\n").await?;
    let id = p.token.expect("token principal");
    let server = WorldServer::new(api, Auth::Connection(store, id));
    let running = server.serve((r, w)).await.map_err(io::Error::other)?;
    let _ = running.waiting().await;
    Ok(())
}

/// Client side of the handshake, for the bridge and tests: returns the stream ready for MCP.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: S, token: &str) -> io::Result<BufReader<S>> {
    let mut s = BufReader::new(stream);
    s.get_mut().write_all(format!("{HANDSHAKE}{token}\n").as_bytes()).await?;
    let mut line = String::new();
    tokio::time::timeout(HANDSHAKE_TIMEOUT, (&mut s).take(MAX_HANDSHAKE).read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no handshake reply"))??;
    if line.trim_end() != "OK" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("engine refused the token: {}", line.trim_end()),
        ));
    }
    Ok(s)
}

/// Relay stdio to an authenticated engine connection until either side closes.
pub async fn relay_stdio<S: AsyncRead + AsyncWrite + Unpin>(conn: BufReader<S>) -> io::Result<()> {
    let (mut cr, mut cw) = tokio::io::split(conn);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let r = tokio::io::copy(&mut stdin, &mut cw).await;
        let _ = cw.shutdown().await;
        r
    };
    let down = async {
        let r = tokio::io::copy(&mut cr, &mut stdout).await;
        let _ = stdout.flush().await;
        r
    };
    tokio::select! {
        r = up => r.map(|_| ()),
        r = down => r.map(|_| ()),
    }
}

/// A fresh, unguessable endpoint name.
pub fn fresh_endpoint_name() -> String {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).expect("OS random number generator");
    let r: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("primordial-mcp-{}-{r}", std::process::id())
}

#[cfg(windows)]
pub use self::windows::*;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn from_wide(p: *const u16) -> String {
        let mut n = 0;
        while unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
    }

    /// The current process user's SID, as a string (S-1-5-21-...).
    pub fn current_user_sid() -> io::Result<String> {
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s: *mut u16 = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
                return Err(io::Error::last_os_error());
            }
            let out = from_wide(s);
            LocalFree(s as *mut c_void);
            Ok(out)
        }
    }

    /// `D:P(A;;GA;;;<sid>)`: a protected DACL with one ACE, full access for the current user.
    pub fn pipe_sddl() -> io::Result<String> {
        Ok(format!("D:P(A;;GA;;;{})", current_user_sid()?))
    }

    struct Sd(PSECURITY_DESCRIPTOR);
    impl Drop for Sd {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    fn create(path: &str, first: bool) -> io::Result<NamedPipeServer> {
        let sddl = wide(&pipe_sddl()?);
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut psd, null_mut())
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sd = Sd(psd);
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let mut o = ServerOptions::new();
        o.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `sa` and the descriptor it points to outlive the call.
        unsafe { o.create_with_security_attributes_raw(path, &mut sa as *mut _ as *mut c_void) }
    }

    /// The DACL of a pipe handle, in SDDL (for tests and diagnostics).
    pub fn dacl_of(h: &impl AsRawHandle) -> io::Result<String> {
        unsafe {
            let mut psd: PSECURITY_DESCRIPTOR = null_mut();
            let rc = GetSecurityInfo(
                h.as_raw_handle() as HANDLE,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut psd,
            );
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let sd = Sd(psd);
            let mut s: *mut u16 = null_mut();
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let out = from_wide(s);
            LocalFree(s as *mut c_void);
            Ok(out)
        }
    }

    pub fn pipe_path(name: &str) -> String {
        format!(r"\\.\pipe\{name}")
    }

    /// The first instance, created before the accept loop starts so a client can connect at once.
    pub fn bind(name: &str) -> io::Result<(String, NamedPipeServer)> {
        let path = pipe_path(name);
        let first = create(&path, true)?;
        Ok((path, first))
    }

    /// Accept connections forever, each served as its token's principal.
    pub async fn accept_loop(
        path: String,
        mut server: NamedPipeServer,
        api: Api,
        store: Arc<TokenStore>,
    ) -> io::Result<()> {
        loop {
            server.connect().await?;
            let conn = server;
            server = create(&path, false)?;
            let (api, store) = (api.clone(), store.clone());
            tokio::spawn(async move {
                let _ = serve_connection(api, store, conn).await;
            });
        }
    }

    /// Bind and spawn the accept loop; returns the pipe path for `PE_MCP_PIPE`.
    pub fn serve_pipe(api: Api, store: Arc<TokenStore>) -> io::Result<String> {
        let (path, first) = bind(&fresh_endpoint_name())?;
        tokio::spawn(accept_loop(path.clone(), first, api, store));
        Ok(path)
    }

    pub async fn connect(path: &str) -> io::Result<NamedPipeClient> {
        for _ in 0..200 {
            match ClientOptions::new().open(path) {
                Ok(c) => return Ok(c),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "pipe busy"))
    }
}

#[cfg(unix)]
pub use self::unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    pub struct Endpoint {
        pub dir: PathBuf,
        pub path: PathBuf,
        uid: u32,
    }

    impl Drop for Endpoint {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    pub fn bind(name: &str) -> io::Result<(Endpoint, UnixListener)> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(name);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let uid = std::fs::metadata(&dir)?.uid();
        let path = dir.join("mcp.sock");
        let l = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok((Endpoint { dir, path, uid }, l))
    }

    pub async fn accept_loop(ep: Endpoint, l: UnixListener, api: Api, store: Arc<TokenStore>) -> io::Result<()> {
        loop {
            let (s, _) = l.accept().await?;
            if s.peer_cred().map(|c| c.uid()).ok() != Some(ep.uid) {
                continue; // another user: drop the connection
            }
            let (api, store) = (api.clone(), store.clone());
            tokio::spawn(async move {
                let _ = serve_connection(api, store, s).await;
            });
        }
    }

    pub fn serve_pipe(api: Api, store: Arc<TokenStore>) -> io::Result<String> {
        let (ep, l) = bind(&fresh_endpoint_name())?;
        let path = ep.path.to_string_lossy().into_owned();
        tokio::spawn(accept_loop(ep, l, api, store));
        Ok(path)
    }

    pub async fn connect(path: &str) -> io::Result<UnixStream> {
        UnixStream::connect(path).await
    }
}
