#[cfg(unix)]
use std::path::{Path, PathBuf};

use super::adapter::{Adapter, DynRead, DynWrite};
use super::error::AdapterError;

#[derive(Clone, Debug)]
pub enum Endpoint {
	#[cfg(unix)]
	Unix(PathBuf),
	#[cfg(windows)]
	NamedPipe(String),
}

impl Endpoint {
	pub fn kern() -> Self {
		let dir = std::env::current_dir().unwrap_or_default();
		Self::kern_for(&dir)
	}

	// Same socket a daemon running with cwd `root` binds — lets the hub address
	// a node's endpoint without being in that directory.
	pub fn kern_for(root: &std::path::Path) -> Self {
		let tag = path_tag(root);
		Self::scoped(&format!("kern-{tag}"))
	}

	pub fn hub() -> Self {
		Self::scoped("kern-hub")
	}

	// Reconstruct from the wire form produced by `display()`.
	pub fn parse(s: &str) -> Self {
		#[cfg(unix)]
		{
			Endpoint::Unix(PathBuf::from(s))
		}
		#[cfg(windows)]
		{
			Endpoint::NamedPipe(s.to_string())
		}
	}

	fn scoped(name: &str) -> Self {
		#[cfg(unix)]
		{
			let path = std::env::var_os("XDG_RUNTIME_DIR")
				.map(PathBuf::from)
				.map(|d| d.join(format!("{name}.sock")))
				.unwrap_or_else(|| {
					let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
					PathBuf::from(format!("/tmp/{name}-{user}.sock"))
				});
			Endpoint::Unix(path)
		}
		#[cfg(windows)]
		{
			let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
			Endpoint::NamedPipe(format!(r"\\.\pipe\{name}-{user}"))
		}
	}

	pub fn display(&self) -> String {
		match self {
			#[cfg(unix)]
			Endpoint::Unix(p) => p.display().to_string(),
			#[cfg(windows)]
			Endpoint::NamedPipe(n) => n.clone(),
		}
	}
}

// FNV-1a over the canonical path — MUST stay stable across processes (unlike
// DefaultHasher's randomized state) so daemon, hub, and clients always agree.
fn path_tag(dir: &std::path::Path) -> String {
	let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
	let s = canon.to_string_lossy();
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
	for b in s.as_bytes() {
		hash ^= *b as u64;
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	format!("{hash:016x}")
}

#[cfg(test)]
mod cwd_tag_tests {
	use super::*;

	#[test]
	fn path_tag_is_stable_and_nonempty() {
		let dir = std::env::current_dir().unwrap();
		let a = path_tag(&dir);
		let b = path_tag(&dir);
		assert_eq!(a, b, "same path must yield the same tag");
		assert_eq!(a.len(), 16, "tag is 16 hex chars");
		assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
	}

	#[test]
	fn endpoint_kern_includes_tag() {
		let dir = std::env::current_dir().unwrap();
		let ep = Endpoint::kern();
		assert!(
			ep.display().contains(&path_tag(&dir)),
			"endpoint scoped by cwd tag"
		);
	}

	#[test]
	fn kern_for_cwd_matches_kern() {
		let dir = std::env::current_dir().unwrap();
		assert_eq!(
			Endpoint::kern().display(),
			Endpoint::kern_for(&dir).display(),
			"hub-computed endpoint must match the node's own"
		);
	}

	#[test]
	fn parse_round_trips_display() {
		let ep = Endpoint::hub();
		assert_eq!(Endpoint::parse(&ep.display()).display(), ep.display());
	}
}

#[cfg(unix)]
pub struct UnixStreamAdapter {
	stream: tokio::net::UnixStream,
}

#[cfg(unix)]
impl UnixStreamAdapter {
	pub fn new(stream: tokio::net::UnixStream) -> Self {
		Self { stream }
	}
	pub async fn connect(path: &Path) -> Result<Self, AdapterError> {
		let stream = tokio::net::UnixStream::connect(path).await?;
		Ok(Self { stream })
	}
}

#[cfg(unix)]
impl Adapter for UnixStreamAdapter {
	fn split(self: Box<Self>) -> (DynRead, DynWrite) {
		let (r, w) = self.stream.into_split();
		(Box::new(r), Box::new(w))
	}
}

#[cfg(windows)]
pub struct NamedPipeAdapter {
	inner: NamedPipeInner,
}

#[cfg(windows)]
enum NamedPipeInner {
	Server(tokio::net::windows::named_pipe::NamedPipeServer),
	Client(tokio::net::windows::named_pipe::NamedPipeClient),
}

#[cfg(windows)]
impl NamedPipeAdapter {
	pub fn from_server(server: tokio::net::windows::named_pipe::NamedPipeServer) -> Self {
		Self {
			inner: NamedPipeInner::Server(server),
		}
	}
	pub async fn connect(pipe_name: &str) -> Result<Self, AdapterError> {
		let client = tokio::net::windows::named_pipe::ClientOptions::new().open(pipe_name)?;
		Ok(Self {
			inner: NamedPipeInner::Client(client),
		})
	}
}

#[cfg(windows)]
impl Adapter for NamedPipeAdapter {
	fn split(self: Box<Self>) -> (DynRead, DynWrite) {
		match self.inner {
			NamedPipeInner::Server(s) => {
				let (r, w) = tokio::io::split(s);
				(Box::new(r), Box::new(w))
			}
			NamedPipeInner::Client(c) => {
				let (r, w) = tokio::io::split(c);
				(Box::new(r), Box::new(w))
			}
		}
	}
}

pub enum LocalAdapter {
	#[cfg(unix)]
	Unix(UnixStreamAdapter),
	#[cfg(windows)]
	NamedPipe(NamedPipeAdapter),
}

impl Adapter for LocalAdapter {
	fn split(self: Box<Self>) -> (DynRead, DynWrite) {
		match *self {
			#[cfg(unix)]
			LocalAdapter::Unix(a) => Box::new(a).split(),
			#[cfg(windows)]
			LocalAdapter::NamedPipe(a) => Box::new(a).split(),
		}
	}
}

// Who is on the other end, decided before a single byte goes out. A client's
// first frame is `present_auth`, which hands over the graph's `mcp-token` — the
// same secret `mcp_addr` demands — and with no `XDG_RUNTIME_DIR` the endpoint
// falls back to `/tmp/kern-<tag>-<user>.sock` (`Endpoint::scoped`), where the
// sticky bit stops another local user from *deleting* the name but not from
// *taking* it first. So the name gets checked before `connect`, which puts it
// ahead of every byte a client could write.
//
// This is the cheap half and it is deliberately not the whole check: a stat
// describes a path at one instant, and the path can move. `require_peer_is_caller`
// below is the half that cannot be raced. Both run; this one first, because
// failing on the stat gives a refusal that names the squatter's uid without
// opening a connection to it at all.
//
// Fails closed. Any stat error refuses, including a symlink that resolves to
// nothing; both the name and what it resolves to must be ours, because a
// symlink is a substitution even when its target is innocent. The one case
// that is *not* a refusal is a path with nothing at it at all: that is the
// ordinary no-daemon case, nothing is bound, nothing can be told the token,
// and callers that distinguish absence from a squat still need to.
//
// Windows has no analogue and gets none: a named pipe is not a filesystem
// object with an owning uid, and the server side already restricts every
// instance to this process's own SID (`owner_only::OwnerOnlySd`), so the name
// cannot be taken by another user in the first place.
#[cfg(unix)]
fn require_owned_by_caller(path: &Path) -> Result<(), AdapterError> {
	use std::os::unix::fs::MetadataExt;
	let untrusted =
		|what: &str| AdapterError::UntrustedEndpoint(format!("{}: {what}", path.display()));
	// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
	let euid = unsafe { libc::geteuid() };
	let link = std::fs::symlink_metadata(path).map_err(|e| {
		if e.kind() == std::io::ErrorKind::NotFound {
			AdapterError::Io(e)
		} else {
			untrusted(&format!("cannot stat: {e}"))
		}
	})?;
	let target = std::fs::metadata(path).map_err(|e| untrusted(&format!("cannot resolve: {e}")))?;
	// Reported separately because they fail for different reasons and a refusal
	// that cannot say which is a refusal nobody can act on: a symlink we own
	// pointing at root's socket mismatches only on the target, and folding the
	// two together printed the *link's* uid, i.e. "owned by uid 1000, not 1000".
	if link.uid() != euid {
		return Err(untrusted(&format!(
			"owned by uid {}, not {euid}",
			link.uid()
		)));
	}
	if target.uid() != euid {
		return Err(untrusted(&format!(
			"resolves to a path owned by uid {}, not {euid}",
			target.uid()
		)));
	}
	Ok(())
}

// The second half, and the one that is not a guess. `require_owned_by_caller`
// asks the filesystem about a *name*; this asks the kernel about *this
// connection*. `SO_PEERCRED` is recorded when the peer called `listen`, so it
// cannot be changed by anything that happens to the path afterwards — which
// matters because the window between the stat and the connect is opened by our
// own daemon, not by the attacker: `Drop for LocalListener` unlinks the socket
// on every shutdown, and the stale-rebind path unlinks it too, so a name that
// stats as ours can be free a microsecond later and bound by somebody else
// before `connect` lands. A stat alone cannot see that; this does.
//
// Still ahead of frame 1: `connect_kern` returns the adapter and only then does
// `present_auth` write the token.
#[cfg(unix)]
fn require_peer_is_caller(adapter: &UnixStreamAdapter, path: &Path) -> Result<(), AdapterError> {
	// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
	require_peer_uid(adapter, path, unsafe { libc::geteuid() })
}

// Split out with the expected uid as an argument for one reason: the refusal is
// otherwise untestable. Asserting that a foreign server is turned away would
// need a socket bound by a second uid, which no test can create; passing a uid
// that is deliberately not ours exercises the same real `SO_PEERCRED` read
// against a real connected socket and leaves only `geteuid()` uncovered.
#[cfg(unix)]
fn require_peer_uid(
	adapter: &UnixStreamAdapter,
	path: &Path,
	expected: u32,
) -> Result<(), AdapterError> {
	let untrusted =
		|what: &str| AdapterError::UntrustedEndpoint(format!("{}: {what}", path.display()));
	let cred = adapter
		.stream
		.peer_cred()
		.map_err(|e| untrusted(&format!("cannot read peer credentials: {e}")))?;
	if cred.uid() != expected {
		return Err(untrusted(&format!(
			"served by uid {}, not {expected}",
			cred.uid()
		)));
	}
	Ok(())
}

pub async fn connect_kern(endpoint: &Endpoint) -> Result<LocalAdapter, AdapterError> {
	match endpoint {
		#[cfg(unix)]
		Endpoint::Unix(path) => {
			require_owned_by_caller(path)?;
			let adapter = UnixStreamAdapter::connect(path).await?;
			require_peer_is_caller(&adapter, path)?;
			Ok(LocalAdapter::Unix(adapter))
		}
		#[cfg(windows)]
		Endpoint::NamedPipe(name) => Ok(LocalAdapter::NamedPipe(
			NamedPipeAdapter::connect(name).await?,
		)),
	}
}

// Test-only seam mirroring `bind_unix(path, expected_peer)`: the client-side
// peer-uid check is otherwise untestable on one uid — `connect_kern` hardcodes
// `geteuid()` through `require_peer_is_caller`, and no test can bind a socket
// as a second uid. Passing a uid that is deliberately not the server's drives
// the same real `SO_PEERCRED` read down the same branch, leaving only
// `geteuid()` uncovered — the same stance the bind arm's seam takes.
#[cfg(unix)]
#[cfg(test)]
async fn connect_kern_with_peer(
	endpoint: &Endpoint,
	expected_uid: u32,
) -> Result<LocalAdapter, AdapterError> {
	match endpoint {
		Endpoint::Unix(path) => {
			require_owned_by_caller(path)?;
			let adapter = UnixStreamAdapter::connect(path).await?;
			require_peer_uid(&adapter, path, expected_uid)?;
			Ok(LocalAdapter::Unix(adapter))
		}
		#[cfg(windows)]
		Endpoint::NamedPipe(name) => Ok(LocalAdapter::NamedPipe(
			NamedPipeAdapter::connect(name).await?,
		)),
	}
}

pub enum BindOutcome {
	Bound(LocalListener),
	AlreadyRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
	#[error("bind: {0}")]
	Io(#[from] std::io::Error),
	// Something this euid does not own holds the name. Deliberately *not*
	// `AlreadyRunning`: standing down for a squatter is the bug, and a daemon
	// that exits quietly leaves an operator with no daemon and no reason. The
	// string carries the refusal `require_owned_by_caller`/`require_peer_is_caller`
	// wrote, so the foreign uid survives to whatever prints this.
	#[error("bind refused: {0}")]
	Untrusted(String),
}

// The socket carries graph reads AND mutations behind one shared secret that
// proves a uid and nothing finer, so it must be owner-only: a foreign uid that
// could open it would be inside the only boundary there is. chmod-after-bind
// leaves a sub-ms window at the umask default (0755); the alternative is
// flipping the process-global umask, which in a multi-threaded daemon would
// race every unrelated file created concurrently.
#[cfg(unix)]
fn harden_socket(path: &Path) -> std::io::Result<()> {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

// The same posture on Windows, where there are no mode bits to set. A named
// pipe created with the default security descriptor is reachable by anything on
// the machine that can name it, so every instance is created with an explicit
// one instead: SDDL `D:P(A;;GA;;;<user>)` — a protected DACL (no inheritance)
// carrying exactly one ACE, full access for the SID this process runs as. No
// SYSTEM ACE and no Administrators ACE, because `0600` grants neither.
#[cfg(windows)]
mod owner_only {
	use std::io;

	use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
	use windows_sys::Win32::Security::Authorization::{
		ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
	};
	use windows_sys::Win32::Security::{
		GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
		TOKEN_USER,
	};
	use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

	/// An owner-only security descriptor, freed on drop. Held by the listener
	/// for the whole of its life: `accept` creates a fresh pipe instance per
	/// connection, and an instance created without this is an open door.
	pub struct OwnerOnlySd(PSECURITY_DESCRIPTOR);

	// SAFETY: the pointer is owned, never mutated after construction, and only
	// read through `attributes()`.
	unsafe impl Send for OwnerOnlySd {}
	unsafe impl Sync for OwnerOnlySd {}

	impl OwnerOnlySd {
		pub fn new() -> io::Result<Self> {
			let sid = current_user_sid()?;
			let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
				.encode_utf16()
				.chain(std::iter::once(0))
				.collect();
			let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
			// SAFETY: `sddl` is NUL-terminated and outlives the call; `psd` receives
			// a LocalAlloc'd descriptor this value then owns.
			let ok = unsafe {
				ConvertStringSecurityDescriptorToSecurityDescriptorW(
					sddl.as_ptr(),
					SDDL_REVISION_1,
					&mut psd,
					std::ptr::null_mut(),
				)
			};
			if ok == 0 || psd.is_null() {
				return Err(io::Error::last_os_error());
			}
			Ok(Self(psd))
		}

		pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
			SECURITY_ATTRIBUTES {
				nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
				lpSecurityDescriptor: self.0,
				bInheritHandle: 0,
			}
		}
	}

	impl Drop for OwnerOnlySd {
		fn drop(&mut self) {
			// SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
			// (LocalAlloc) and freed nowhere else.
			unsafe { LocalFree(self.0.cast()) };
		}
	}

	fn current_user_sid() -> io::Result<String> {
		let mut token: HANDLE = std::ptr::null_mut();
		// SAFETY: the pseudo-handle from GetCurrentProcess needs no close; `token`
		// receives a real handle closed below.
		if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
			return Err(io::Error::last_os_error());
		}
		let out = token_user_sid(token);
		// SAFETY: `token` was opened here and is not used after this.
		unsafe { CloseHandle(token) };
		out
	}

	fn token_user_sid(token: HANDLE) -> io::Result<String> {
		let mut len: u32 = 0;
		// SAFETY: the sizing call is *expected* to fail; it only writes `len`.
		unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
		if len == 0 {
			return Err(io::Error::last_os_error());
		}
		let mut buf = vec![0u8; len as usize];
		// SAFETY: `buf` is exactly the length the sizing call asked for.
		if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
		{
			return Err(io::Error::last_os_error());
		}
		// SAFETY: the buffer now holds a TOKEN_USER whose `Sid` points inside it.
		let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
		let mut raw: *mut u16 = std::ptr::null_mut();
		// SAFETY: `sid` is valid for the lifetime of `buf`; `raw` receives a
		// LocalAlloc'd NUL-terminated string freed below.
		if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 || raw.is_null() {
			return Err(io::Error::last_os_error());
		}
		let mut n = 0usize;
		// SAFETY: walking a NUL-terminated buffer the call above guaranteed.
		while unsafe { *raw.add(n) } != 0 {
			n += 1;
		}
		// SAFETY: `raw[..n]` is the string body, exclusive of the terminator.
		let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, n) });
		// SAFETY: `raw` came from ConvertSidToStringSidW and is dead after this.
		unsafe { LocalFree(raw.cast()) };
		Ok(s)
	}
}

#[cfg(windows)]
fn create_pipe_instance(
	name: &str,
	sd: &owner_only::OwnerOnlySd,
	first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
	use tokio::net::windows::named_pipe::ServerOptions;
	let mut attrs = sd.attributes();
	// SAFETY: `attrs` lives across the call and points at a descriptor `sd` owns.
	unsafe {
		ServerOptions::new()
			.first_pipe_instance(first)
			.create_with_security_attributes_raw(
				name,
				std::ptr::addr_of_mut!(attrs).cast::<std::ffi::c_void>(),
			)
	}
}

// Split out of `bind_kern_listener` with the expected peer uid as an argument,
// for exactly the reason `require_peer_uid` is split out of
// `require_peer_is_caller`: the second check in the `AddrInUse` arm is otherwise
// unreachable from a test. A live foreign socket needs a second uid to bind it,
// which no test can create — but the arm's *decision* does not care where the
// mismatch comes from, so handing it a uid that is deliberately not the
// server's drives the same real `SO_PEERCRED` read down the same branch and
// leaves only `geteuid()` uncovered. Without this the peer check in this arm
// could be deleted outright and the whole suite would stay green.
#[cfg(unix)]
async fn bind_unix(path: &Path, expected_peer: u32) -> Result<BindOutcome, BindError> {
	let listener = match tokio::net::UnixListener::bind(path) {
		Ok(listener) => listener,
		Err(e) if e.kind() != std::io::ErrorKind::AddrInUse => {
			return Err(e.into());
		}
		Err(_) => {
			// `AddrInUse` means *something* holds the name; it does not mean a
			// daemon of ours does. Deciding that takes the same two checks
			// `connect_kern` runs, and the server ran neither: a squatter simply
			// accepted the probe and the real daemon stood down, and when the
			// probe failed the unlink below ran on a path nobody had verified.
			// `/tmp`'s sticky bit covers a foreign *socket* there — it does not
			// cover a symlink this uid owns pointing at somebody else's file, so
			// that one was unlinked and rebound. Both halves refuse here now.
			//
			// Fails closed by construction: every error from either check —
			// including the `Io` ones, a path that vanished under the stat or a
			// dangling link — becomes a refusal, because the only thing after
			// this arm is `remove_file`, and unlinking on a maybe is the harm.
			require_owned_by_caller(path).map_err(|e| BindError::Untrusted(e.to_string()))?;
			match UnixStreamAdapter::connect(path).await {
				Ok(adapter) => {
					require_peer_uid(&adapter, path, expected_peer)
						.map_err(|e| BindError::Untrusted(e.to_string()))?;
					return Ok(BindOutcome::AlreadyRunning);
				}
				// Nothing answers a name we own: our own stale socket, ours to reclaim.
				Err(_) => {
					let _ = std::fs::remove_file(path);
					tokio::net::UnixListener::bind(path)?
				}
			}
		}
	};
	harden_socket(path)?;
	Ok(BindOutcome::Bound(LocalListener {
		inner: listener,
		socket_path: path.to_path_buf(),
	}))
}

pub async fn bind_kern_listener(endpoint: &Endpoint) -> Result<BindOutcome, BindError> {
	match endpoint {
		#[cfg(unix)]
		Endpoint::Unix(path) => {
			// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
			bind_unix(path, unsafe { libc::geteuid() }).await
		}
		#[cfg(windows)]
		Endpoint::NamedPipe(name) => {
			// Fail closed: no descriptor, no pipe. A pipe created with the default
			// DACL is one any process on the machine can open, which is worse than
			// not serving — it looks like a daemon and guards nothing.
			let security = owner_only::OwnerOnlySd::new()?;
			match create_pipe_instance(name, &security, true) {
                Ok(server) => Ok(BindOutcome::Bound(LocalListener {
                    pipe_name: name.clone(),
                    security,
                    current: Some(server),
                })),
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(5)    // ERROR_ACCESS_DENIED
                        || e.raw_os_error() == Some(231)  // ERROR_PIPE_BUSY
                =>
                {
                    Ok(BindOutcome::AlreadyRunning)
                }
                Err(e) => Err(e.into()),
            }
		}
	}
}

/// Takeover: adopt fd 0 as the already-bound kern.sock listener, inherited
/// from a predecessor daemon that is handing over. No bind, no AlreadyRunning
/// probe — probing would consume a queued client connect from the shared
/// backlog. Unix only; a named pipe server handle cannot cross processes.
#[cfg(unix)]
pub fn adopt_kern_listener(endpoint: &Endpoint) -> Result<LocalListener, BindError> {
	use std::os::fd::FromRawFd;
	let Endpoint::Unix(path) = endpoint;
	// SAFETY: the takeover contract places the listener at fd 0 (the successor
	// is spawned with it as stdin) and nothing else in this process reads stdin
	// in daemon mode.
	let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(0) };
	std_listener.set_nonblocking(true)?;
	let inner = tokio::net::UnixListener::from_std(std_listener)?;
	Ok(LocalListener {
		inner,
		socket_path: path.clone(),
	})
}

pub struct LocalListener {
	#[cfg(unix)]
	inner: tokio::net::UnixListener,
	#[cfg(unix)]
	socket_path: PathBuf,
	#[cfg(windows)]
	pipe_name: String,
	// Kept for the life of the listener: `accept` creates the *next* instance,
	// and an instance without this descriptor is a hole beside a locked door.
	#[cfg(windows)]
	security: owner_only::OwnerOnlySd,
	#[cfg(windows)]
	current: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

#[cfg(unix)]
impl LocalListener {
	/// A dup of the listening fd, for handing to a successor process. The dup
	/// carries close-on-exec (the spawn path clears it by dup2-ing into a
	/// stdio slot), so holding one leaks nothing into unrelated children.
	pub fn dup_fd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
		use std::os::fd::AsFd;
		self.inner.as_fd().try_clone_to_owned()
	}
}

impl LocalListener {
	pub async fn accept(&mut self) -> Result<LocalAdapter, std::io::Error> {
		#[cfg(unix)]
		{
			let (stream, _peer) = self.inner.accept().await?;
			Ok(LocalAdapter::Unix(UnixStreamAdapter::new(stream)))
		}
		#[cfg(windows)]
		{
			let server = self.current.take().expect("listener uninitialised");
			server.connect().await?;
			// Pre-create the next instance so subsequent accept doesn't race
			// a fast reconnect — with the same descriptor as the first.
			self.current = Some(create_pipe_instance(
				&self.pipe_name,
				&self.security,
				false,
			)?);
			Ok(LocalAdapter::NamedPipe(NamedPipeAdapter::from_server(
				server,
			)))
		}
	}
}

#[cfg(unix)]
impl Drop for LocalListener {
	fn drop(&mut self) {
		// Best-effort cleanup so the next daemon doesn't trip the stale-sock probe.
		let _ = std::fs::remove_file(&self.socket_path);
	}
}

#[cfg(all(test, windows))]
mod bind_tests_windows {
	use super::*;

	#[tokio::test]
	async fn a_second_bind_of_the_same_pipe_reports_already_running() {
		let ep = Endpoint::NamedPipe(format!(r"\\.\pipe\kern-bindtest-{}", std::process::id()));
		let first = bind_kern_listener(&ep).await.unwrap();
		assert!(
			matches!(first, BindOutcome::Bound(_)),
			"first bind owns the pipe"
		);
		let second = bind_kern_listener(&ep).await.unwrap();
		assert!(
			matches!(second, BindOutcome::AlreadyRunning),
			"second bind sees AlreadyRunning"
		);
		drop(first); // keep the first instance alive until the assertion above
	}
}

#[cfg(all(test, unix))]
mod bind_tests_unix {
	use super::*;

	#[tokio::test]
	async fn a_live_owner_reports_already_running() {
		let dir = tempfile::tempdir().unwrap();
		let ep = Endpoint::Unix(dir.path().join("kern.sock"));
		let first = bind_kern_listener(&ep).await.unwrap();
		let BindOutcome::Bound(_listener) = first else {
			panic!("first bind should own the socket")
		};
		let second = bind_kern_listener(&ep).await.unwrap();
		assert!(
			matches!(second, BindOutcome::AlreadyRunning),
			"a live owner -> AlreadyRunning"
		);
	}

	#[tokio::test]
	async fn a_bound_socket_is_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		let ep = Endpoint::Unix(path.clone());
		let BindOutcome::Bound(_listener) = bind_kern_listener(&ep).await.unwrap() else {
			panic!("first bind should own the socket")
		};
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600, "socket must be owner-only, got {mode:o}");
	}

	#[tokio::test]
	async fn a_rebound_stale_socket_is_also_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		{
			let _l = tokio::net::UnixListener::bind(&path).unwrap();
		}
		let ep = Endpoint::Unix(path.clone());
		let BindOutcome::Bound(_listener) = bind_kern_listener(&ep).await.unwrap() else {
			panic!("stale file should be removed and rebound")
		};
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(
			mode, 0o600,
			"the stale-rebind path hardens too, got {mode:o}"
		);
	}

	// The first of the arm's two checks, and the one shape of foreign endpoint a
	// single uid can really build: a symlink we own whose target is root's. `bind`
	// returns `EADDRINUSE` on it (the name exists), which is the arm under test.
	// Before the checks moved into that arm the probe failed — `/etc/hosts` is
	// not a socket — the `remove_file` unlinked *our own link* (the sticky bit
	// protects the target, never the link) and the bind then succeeded on a name
	// a foreign path had been substituted into.
	#[tokio::test]
	async fn a_symlink_to_a_foreign_target_refuses_the_bind() {
		let Some(foreign) = super::owner_tests_unix::foreign_path() else {
			return; // running as root: nothing here is foreign
		};
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		std::os::unix::fs::symlink(&foreign, &path).unwrap();
		let ep = Endpoint::Unix(path.clone());
		// Not `expect_err`: `BindOutcome` holds a `LocalListener` and is not `Debug`,
		// and deriving it on a live listener to word one assertion is the tail
		// wagging the dog.
		let Err(err) = bind_kern_listener(&ep).await else {
			panic!("a foreign-owned endpoint must refuse the bind, not bind over it")
		};
		assert!(
			matches!(err, BindError::Untrusted(_)),
			"a squat is not i/o and is not AlreadyRunning: {err}"
		);
		assert!(
			err.to_string().contains("owned by uid 0"),
			"the refusal names the foreign uid: {err}"
		);
		assert!(
			path.symlink_metadata().is_ok(),
			"a path we refused is a path we must not have unlinked"
		);
	}

	// The arm's *second* check, which the symlink above cannot reach: a live
	// socket that answers, served by somebody who is not us. Binding one takes a
	// second uid, so the uid is injected instead — the same move
	// `the_peer_check_reads_the_server_uid_and_decides_both_ways` makes, against
	// the same real `SO_PEERCRED` read on a real connected socket, but driven
	// through the whole arm rather than through the predicate alone. What this
	// pins is the wiring, which was code-review-only before it existed: that the
	// arm calls the peer check at all, that its verdict becomes `Untrusted`
	// rather than `AlreadyRunning`, and that a refusal does not fall through to
	// the unlink. Delete the call and every other bind test still passes.
	//
	// What stays out of reach is the live squat end to end — a real foreign
	// daemon holding the real path. That is a second uid and nothing less.
	#[tokio::test]
	async fn a_live_endpoint_served_by_another_uid_refuses_the_bind() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		// Bound, listening and answering — the shape a squatter presents. Only the
		// uid the arm compares against is a fiction.
		let _squatter = tokio::net::UnixListener::bind(&path).unwrap();
		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		let euid = unsafe { libc::geteuid() };
		let Err(err) = bind_unix(&path, euid.wrapping_add(1)).await else {
			panic!("a live endpoint served by a foreign uid must refuse the bind, not stand down for it")
		};
		assert!(
			matches!(err, BindError::Untrusted(_)),
			"a squat is not i/o and is not AlreadyRunning: {err}"
		);
		assert!(
			err.to_string().contains(&format!("served by uid {euid}")),
			"the refusal names who is actually serving: {err}"
		);
		assert!(
			path.symlink_metadata().is_ok(),
			"a name we refused is a name we must not have unlinked"
		);
	}

	#[tokio::test]
	async fn a_stale_socket_file_is_removed_and_rebound() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		{
			let _l = tokio::net::UnixListener::bind(&path).unwrap();
		}
		assert!(
			path.exists(),
			"stale socket file remains after the listener drops"
		);
		let ep = Endpoint::Unix(path);
		let outcome = bind_kern_listener(&ep).await.unwrap();
		assert!(
			matches!(outcome, BindOutcome::Bound(_)),
			"stale file removed, endpoint rebound"
		);
	}
}

#[cfg(all(test, unix))]
mod owner_tests_unix {
	use super::*;

	// A path owned by somebody else, without needing a second uid to make one.
	// `/etc/hosts` is root's on every Unix a developer runs this on; under an
	// euid of 0 it is *ours*, and there is then nothing on the filesystem this
	// process could fail to own, so the case is skipped rather than faked.
	pub(super) fn foreign_path() -> Option<PathBuf> {
		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		if unsafe { libc::geteuid() } == 0 {
			return None;
		}
		let p = PathBuf::from("/etc/hosts");
		p.exists().then_some(p)
	}

	#[test]
	fn a_path_this_user_owns_is_accepted() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		std::fs::write(&path, b"").unwrap();
		assert!(
			require_owned_by_caller(&path).is_ok(),
			"our own path must pass, or nothing connects"
		);
	}

	#[test]
	fn a_path_owned_by_another_uid_is_refused() {
		let Some(path) = foreign_path() else {
			return; // running as root: nothing here is foreign
		};
		let err = require_owned_by_caller(&path).expect_err("a foreign owner must refuse");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"refusal must be untrust, not i/o: {err}"
		);
		assert!(
			err.to_string().contains("owned by uid"),
			"the refusal names the owner: {err}"
		);
	}

	#[test]
	fn a_missing_path_reads_as_absence_not_as_a_squat() {
		let dir = tempfile::tempdir().unwrap();
		let err = require_owned_by_caller(&dir.path().join("nothing.sock"))
			.expect_err("nothing to connect to is still an error");
		assert!(
			matches!(err, AdapterError::Io(_)),
			"an empty path is the no-daemon case, not a squat: {err}"
		);
	}

	#[test]
	fn a_dangling_symlink_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		std::os::unix::fs::symlink(dir.path().join("gone"), &path).unwrap();
		let err = require_owned_by_caller(&path).expect_err("a link to nothing must refuse");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"a dangling link is a substitution, not an absence: {err}"
		);
	}

	#[test]
	fn a_symlink_to_a_foreign_target_is_refused() {
		let Some(foreign) = foreign_path() else {
			return; // running as root: nothing here is foreign
		};
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		std::os::unix::fs::symlink(&foreign, &path).unwrap();
		let err = require_owned_by_caller(&path).expect_err("the target's owner is what counts");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"a link we own to a socket we do not is still a squat: {err}"
		);
		// The link is ours and the target is not, so a refusal that printed the
		// link's uid would read "owned by uid 1000, not 1000" and tell an operator
		// nothing. It must name the uid that actually mismatched.
		assert!(
			err
				.to_string()
				.contains("resolves to a path owned by uid 0"),
			"the refusal names the target's owner, not the link's: {err}"
		);
	}

	// The stat says who owns a *name*; this says who is serving *this
	// connection*, which is the half a rename cannot move. Both directions are
	// pinned against a real socket and a real `SO_PEERCRED` read — the refusal
	// by handing the check a uid that is not the server's, since a socket bound
	// by a second uid is not something a test can create.
	#[tokio::test]
	async fn the_peer_check_reads_the_server_uid_and_decides_both_ways() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		let _listener = tokio::net::UnixListener::bind(&path).unwrap();
		let adapter = UnixStreamAdapter::connect(&path)
			.await
			.expect("our own listener accepts");

		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		let euid = unsafe { libc::geteuid() };
		assert!(
			require_peer_uid(&adapter, &path, euid).is_ok(),
			"our own daemon must pass, or nothing connects"
		);

		let err = require_peer_uid(&adapter, &path, euid.wrapping_add(1))
			.expect_err("a server that is not who we expect must be refused");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"the peer verdict is untrust, not i/o: {err}"
		);
		assert!(
			err.to_string().contains(&format!("served by uid {euid}")),
			"the refusal names who is actually serving: {err}"
		);
	}

	// The no-regression half in one test: a socket we bound, reached through the
	// real entry point, with both checks in the way.
	#[tokio::test]
	async fn connect_kern_accepts_a_socket_this_user_bound() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		let _listener = tokio::net::UnixListener::bind(&path).unwrap();
		assert!(
			connect_kern(&Endpoint::Unix(path)).await.is_ok(),
			"the daemon, the hub and `kern mcp` all arrive here"
		);
	}

	// The hub socket is not a second door: `Endpoint::hub()` and
	// `Endpoint::kern()` are both `scoped()`, so both land in the same
	// world-writable `/tmp` when `XDG_RUNTIME_DIR` is unset, and both reach the
	// wire through this one `connect_kern`.
	#[tokio::test]
	async fn connect_kern_refuses_a_foreign_endpoint_before_it_connects() {
		let Some(path) = foreign_path() else {
			return; // running as root: nothing here is foreign
		};
		let err = connect_kern(&Endpoint::Unix(path))
			.await
			.err()
			.expect("a foreign endpoint never becomes an adapter");
		// The proof of ordering: connecting to `/etc/hosts` would fail too, but
		// as `Io` (ENOTSOCK / ECONNREFUSED) from the syscall. `UntrustedEndpoint`
		// can only come from the check, so the check ran first — and every
		// caller's first frame is written after `connect_kern` returns.
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"refused by the owner check, not by the connect: {err}"
		);
	}

	// The peer-uid check on the client path — the one residue item 24 names.
	// `connect_kern_refuses_a_foreign_endpoint_before_it_connects` above covers
	// the *owner* check via a root-owned `foreign_path()`; this covers the
	// *peer-uid* check via the `connect_kern_with_peer` seam, the client-side
	// twin of `bind_unix`'s injected-uid test. A socket this uid serves, reached
	// with an expected uid that is deliberately not ours, must be refused — and
	// the refusal is `UntrustedEndpoint`, not `Io`, so it came from the check.
	#[tokio::test]
	async fn connect_kern_refuses_when_the_peer_uid_differs() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("kern.sock");
		let _listener = tokio::net::UnixListener::bind(&path).unwrap();
		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		let euid = unsafe { libc::geteuid() };
		let err = connect_kern_with_peer(&Endpoint::Unix(path), euid.wrapping_add(1))
			.await
			.err()
			.expect("a peer uid that is not the server's is refused");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"the peer verdict is untrust, not i/o: {err}"
		);
		assert!(
			err.to_string().contains(&format!("served by uid {euid}")),
			"the refusal names who is actually serving: {err}"
		);
	}
}
