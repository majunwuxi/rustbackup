use std::io::{Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentStatus {
    pub state: AgentState,
    pub uptime_seconds: u64,
    pub active_tasks: u32,
}

impl AgentStatus {
    pub fn idle(uptime_seconds: u64) -> Self {
        Self {
            state: AgentState::Idle,
            uptime_seconds,
            active_tasks: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    GetStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u16,
    pub request_id: u64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcError {
    UnsupportedProtocol,
    InvalidRequest,
    InvalidFrame,
    FrameTooLarge,
    AgentUnavailable,
    TransportFailure,
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnsupportedProtocol => "Agent IPC protocol version is unsupported",
            Self::InvalidRequest => "Agent IPC request is invalid",
            Self::InvalidFrame => "Agent IPC frame is invalid",
            Self::FrameTooLarge => "Agent IPC frame exceeds the size limit",
            Self::AgentUnavailable => "RustBackup Agent is unavailable",
            Self::TransportFailure => "Agent IPC transport failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for IpcError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol_version: u16,
    pub request_id: u64,
    pub result: Result<AgentStatus, IpcError>,
}

pub fn dispatch(request: Request, status: AgentStatus) -> Response {
    let result = if request.request_id == 0 {
        Err(IpcError::InvalidRequest)
    } else if request.protocol_version != PROTOCOL_VERSION {
        Err(IpcError::UnsupportedProtocol)
    } else {
        match request.command {
            Command::GetStatus => Ok(status),
        }
    };

    Response {
        protocol_version: PROTOCOL_VERSION,
        request_id: request.request_id,
        result,
    }
}

fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(value).map_err(|_| IpcError::InvalidFrame)?;
    if payload.is_empty() {
        return Err(IpcError::InvalidFrame);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }

    let size = u32::try_from(payload.len()).map_err(|_| IpcError::FrameTooLarge)?;
    writer
        .write_all(&size.to_le_bytes())
        .and_then(|()| writer.write_all(&payload))
        .and_then(|()| writer.flush())
        .map_err(|_| IpcError::TransportFailure)
}

fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, IpcError> {
    let mut size_bytes = [0; 4];
    reader
        .read_exact(&mut size_bytes)
        .map_err(|_| IpcError::TransportFailure)?;
    let size = u32::from_le_bytes(size_bytes) as usize;
    if size == 0 {
        return Err(IpcError::InvalidFrame);
    }
    if size > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }

    let mut payload = vec![0; size];
    reader
        .read_exact(&mut payload)
        .map_err(|_| IpcError::TransportFailure)?;
    serde_json::from_slice(&payload).map_err(|_| IpcError::InvalidFrame)
}

#[cfg(windows)]
mod windows_transport {
    use std::ffi::c_void;
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, SetLastError, ERROR_ALREADY_EXISTS,
        ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, OpenProcessToken,
    };

    use super::{dispatch, read_frame, write_frame, AgentStatus, IpcError, Request, Response};

    const PIPE_PREFIX: &str = r"\\.\pipe\RustBackup.Agent.v1.";
    const MUTEX_PREFIX: &str = r"Global\RustBackup.Agent.v1.";
    const PIPE_BUFFER_BYTES: u32 = super::MAX_FRAME_BYTES as u32;
    const CLIENT_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
    static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

    struct Handle(*mut c_void);

    impl Handle {
        fn is_valid(&self) -> bool {
            !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
        }

        fn into_raw(mut self) -> *mut c_void {
            let handle = self.0;
            self.0 = null_mut();
            handle
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            if self.is_valid() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct SecurityDescriptor(*mut c_void);

    impl SecurityDescriptor {
        fn for_user_sid(sid: &str) -> Result<Self, IpcError> {
            let descriptor_text = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)");
            let descriptor_wide: Vec<u16> = descriptor_text
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut descriptor = null_mut();
            let created = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    descriptor_wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            };
            if created == 0 || descriptor.is_null() {
                return Err(IpcError::TransportFailure);
            }
            Ok(Self(descriptor))
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn current_user_sid() -> Result<String, IpcError> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(IpcError::TransportFailure);
        }
        let token = Handle(token);
        let mut required_bytes = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required_bytes);
        }
        if required_bytes == 0 {
            return Err(IpcError::TransportFailure);
        }

        let word_count = (required_bytes as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; word_count];
        let read = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                (storage.len() * size_of::<usize>()) as u32,
                &mut required_bytes,
            )
        };
        if read == 0 {
            return Err(IpcError::TransportFailure);
        }

        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_wide = null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_wide) } == 0
            || sid_wide.is_null()
        {
            return Err(IpcError::TransportFailure);
        }
        let sid = unsafe {
            let mut length = 0;
            while *sid_wide.add(length) != 0 {
                length += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_wide, length))
        };
        unsafe { LocalFree(sid_wide.cast()) };

        if !sid.starts_with("S-")
            || sid.len() > 184
            || !sid
                .bytes()
                .all(|byte| byte == b'S' || byte == b'-' || byte.is_ascii_digit())
        {
            return Err(IpcError::TransportFailure);
        }
        Ok(sid)
    }

    fn names(sid: &str) -> (Vec<u16>, Vec<u16>) {
        let safe_sid = sid.replace('-', "_");
        let pipe = format!("{PIPE_PREFIX}{safe_sid}");
        let mutex = format!("{MUTEX_PREFIX}{safe_sid}");
        (
            pipe.encode_utf16().chain(std::iter::once(0)).collect(),
            mutex.encode_utf16().chain(std::iter::once(0)).collect(),
        )
    }

    fn wait_for_complete_frame(pipe: *mut c_void) -> Result<(), IpcError> {
        let deadline = Instant::now() + CLIENT_FRAME_TIMEOUT;
        loop {
            let mut header = [0u8; 4];
            let mut header_bytes = 0;
            let mut available_bytes = 0;
            let peeked = unsafe {
                PeekNamedPipe(
                    pipe,
                    header.as_mut_ptr().cast(),
                    header.len() as u32,
                    &mut header_bytes,
                    &mut available_bytes,
                    null_mut(),
                )
            };
            if peeked == 0 {
                return Err(IpcError::TransportFailure);
            }

            if header_bytes == header.len() as u32 {
                let payload_bytes = u32::from_le_bytes(header) as usize;
                if payload_bytes == 0 {
                    return Err(IpcError::InvalidFrame);
                }
                if payload_bytes > super::MAX_FRAME_BYTES {
                    return Err(IpcError::FrameTooLarge);
                }
                if available_bytes as usize >= 4 + payload_bytes {
                    return Ok(());
                }
            }

            if Instant::now() >= deadline {
                return Err(IpcError::TransportFailure);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub struct AgentInstance {
        _mutex: Handle,
    }

    pub fn acquire_agent_instance() -> Result<Option<AgentInstance>, IpcError> {
        let sid = current_user_sid()?;
        let (_, mutex_name) = names(&sid);
        let security = SecurityDescriptor::for_user_sid(&sid)?;
        let attributes = security.attributes();
        unsafe { SetLastError(0) };
        let mutex = Handle(unsafe { CreateMutexW(&attributes, 0, mutex_name.as_ptr()) });
        if !mutex.is_valid() {
            return Err(IpcError::TransportFailure);
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Ok(None);
        }
        Ok(Some(AgentInstance { _mutex: mutex }))
    }

    pub fn query_agent_status() -> Result<AgentStatus, IpcError> {
        let sid = current_user_sid()?;
        let (pipe_name, _) = names(&sid);
        if unsafe { WaitNamedPipeW(pipe_name.as_ptr(), 1000) } == 0 {
            return Err(IpcError::AgentUnavailable);
        }
        let pipe = Handle(unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        });
        if !pipe.is_valid() {
            return Err(IpcError::AgentUnavailable);
        }
        let mut stream = unsafe { File::from_raw_handle(pipe.into_raw() as RawHandle) };
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).max(1);
        let request = Request {
            protocol_version: super::PROTOCOL_VERSION,
            request_id,
            command: super::Command::GetStatus,
        };
        write_frame(&mut stream, &request)?;
        let response: Response = read_frame(&mut stream)?;
        if response.protocol_version != super::PROTOCOL_VERSION || response.request_id != request_id
        {
            return Err(IpcError::InvalidFrame);
        }
        response.result
    }

    pub fn serve_status_forever(started: std::time::Instant) -> Result<(), IpcError> {
        let sid = current_user_sid()?;
        let (pipe_name, _) = names(&sid);
        let security = SecurityDescriptor::for_user_sid(&sid)?;
        let attributes = security.attributes();

        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    pipe_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    PIPE_BUFFER_BYTES,
                    PIPE_BUFFER_BYTES,
                    1000,
                    &attributes,
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                return Err(IpcError::TransportFailure);
            }
            let pipe = Handle(pipe);
            let connected = unsafe { ConnectNamedPipe(pipe.0, null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                drop(pipe);
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            let pipe_handle = pipe.0;
            let mut stream = unsafe { File::from_raw_handle(pipe.into_raw() as RawHandle) };
            if wait_for_complete_frame(pipe_handle).is_ok() {
                if let Ok(request) = read_frame::<_, Request>(&mut stream) {
                    let response =
                        dispatch(request, AgentStatus::idle(started.elapsed().as_secs()));
                    let _ = write_frame(&mut stream, &response);
                }
            }
        }
    }
}

#[cfg(windows)]
pub use windows_transport::{acquire_agent_instance, query_agent_status, serve_status_forever};

#[cfg(not(windows))]
pub fn query_agent_status() -> Result<AgentStatus, IpcError> {
    Err(IpcError::AgentUnavailable)
}

#[cfg(not(windows))]
pub fn acquire_agent_instance() -> Result<Option<()>, IpcError> {
    Err(IpcError::AgentUnavailable)
}

#[cfg(not(windows))]
pub fn serve_status_forever(_started: std::time::Instant) -> Result<(), IpcError> {
    Err(IpcError::AgentUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn request(protocol_version: u16, request_id: u64) -> Request {
        Request {
            protocol_version,
            request_id,
            command: Command::GetStatus,
        }
    }

    #[test]
    fn status_request_round_trips_over_a_bounded_frame() {
        let expected = request(PROTOCOL_VERSION, 42);
        let mut frame = Vec::new();
        write_frame(&mut frame, &expected).expect("request should serialize");

        let actual: Request = read_frame(&mut Cursor::new(frame)).expect("frame should parse");

        assert_eq!(actual, expected);
    }

    #[test]
    fn dispatch_rejects_unsupported_protocol_and_zero_request_id() {
        let status = AgentStatus::idle(3);

        assert_eq!(
            dispatch(request(PROTOCOL_VERSION + 1, 1), status.clone()).result,
            Err(IpcError::UnsupportedProtocol)
        );
        assert_eq!(
            dispatch(request(PROTOCOL_VERSION, 0), status).result,
            Err(IpcError::InvalidRequest)
        );
    }

    #[test]
    fn frame_reader_rejects_zero_and_oversized_lengths_before_allocating() {
        let zero = 0u32.to_le_bytes();
        assert_eq!(
            read_frame::<_, Request>(&mut Cursor::new(zero)),
            Err(IpcError::InvalidFrame)
        );

        let oversized = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert_eq!(
            read_frame::<_, Request>(&mut Cursor::new(oversized)),
            Err(IpcError::FrameTooLarge)
        );
    }

    #[test]
    fn command_model_does_not_accept_unknown_operations() {
        let payload =
            br#"{"protocol_version":1,"request_id":1,"command":{"type":"execute_shell"}}"#;
        assert!(serde_json::from_slice::<Request>(payload).is_err());
    }
}
