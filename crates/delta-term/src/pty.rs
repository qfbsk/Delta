//! ConPTY host. The API shape and the process-attribute setup are transcribed
//! from wezterm 20240203 (`pty/src/win/psuedocon.rs`) and the first Sony-era
//! probe of kernel32 ConPTY documented in 第一代/02-有据.md.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{
	CloseHandle, FARPROC, HANDLE, HANDLE_FLAG_INHERIT, HMODULE, INVALID_HANDLE_VALUE,
	SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{WriteFile, PIPE_ACCESS_OUTBOUND};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Pipes::{
	CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
	CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
	EXTENDED_STARTUPINFO_PRESENT, GetProcessId, InitializeProcThreadAttributeList,
	LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
	STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

// Values from wezterm 20240203 `pty/src/win/psuedocon.rs`.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

static CTRL_PIPE_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct ConPty {
	hpc: HPCON,
	input_write: HANDLE,
	output_read: HANDLE,
	ctrl_write: HANDLE,
	process: HANDLE,
	closed: bool,
}

unsafe impl Send for ConPty {}

impl ConPty {
	pub fn spawn(cols: usize, rows: usize, command: &str) -> io::Result<Self> {
		Self::spawn_with_env(cols, rows, command, &[])
	}

	pub fn spawn_with_env(
		cols: usize,
		rows: usize,
		command: &str,
		envs: &[(String, String)],
	) -> io::Result<Self> {
		unsafe {
			let sa = SECURITY_ATTRIBUTES {
				nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
				lpSecurityDescriptor: null_mut(),
				bInheritHandle: 1,
			};

			let mut input_read: HANDLE = null_mut();
			let mut input_write: HANDLE = null_mut();
			let mut output_read: HANDLE = null_mut();
			let mut output_write: HANDLE = null_mut();

			if CreatePipe(&mut input_read, &mut input_write, &sa, 0) == 0 {
				return Err(io::Error::last_os_error());
			}
			if CreatePipe(&mut output_read, &mut output_write, &sa, 0) == 0 {
				CloseHandle(input_read);
				CloseHandle(input_write);
				return Err(io::Error::last_os_error());
			}

			let _ = SetHandleInformation(input_write, HANDLE_FLAG_INHERIT, 0);
			let _ = SetHandleInformation(output_read, HANDLE_FLAG_INHERIT, 0);

			let ctrl_name = format!(
				"\\\\.\\pipe\\delta-ctrl-{}-{}",
				std::process::id(),
				CTRL_PIPE_SEQ.fetch_add(1, Ordering::Relaxed)
			);
			let mut ctrl_wide: Vec<u16> = ctrl_name.encode_utf16().collect();
			ctrl_wide.push(0);
			let ctrl_write = CreateNamedPipeW(
				ctrl_wide.as_ptr(),
				PIPE_ACCESS_OUTBOUND,
				PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
				1,
				64,
				64,
				0,
				&sa,
			);
			if ctrl_write == INVALID_HANDLE_VALUE {
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
				return Err(io::Error::last_os_error());
			}

			let mut hpc: HPCON = 0;
			let api = conpty_api();
			let create_flags = PSEUDOCONSOLE_RESIZE_QUIRK;
			let hr = (api.create)(
				COORD { X: cols.max(1) as i16, Y: rows.max(1) as i16 },
				input_read,
				output_write,
				create_flags,
				&mut hpc,
			);
			if hr < 0 {
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
				CloseHandle(ctrl_write);
				return Err(io::Error::last_os_error());
			}
			// The pseudoconsole owns its ends of both pipes. Keep our copies
			// closed or the read side never sees EOF and teardown can stall.
			CloseHandle(input_read);
			CloseHandle(output_write);

			let mut attr_size = 0usize;
			let _ = InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
			let words = attr_size.div_ceil(size_of::<usize>()).max(1);
			let mut attr_buf = vec![0usize; words];
			let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
			if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
				(api.close)(hpc);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(ctrl_write);
				return Err(io::Error::last_os_error());
			}

			if UpdateProcThreadAttribute(
				attr_list,
				0,
				PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
				hpc as *const c_void,
				size_of::<HPCON>(),
				null_mut(),
				std::ptr::null(),
			) == 0
			{
				DeleteProcThreadAttributeList(attr_list);
				(api.close)(hpc);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(ctrl_write);
				return Err(io::Error::last_os_error());
			}

			let mut si: STARTUPINFOEXW = zeroed();
			si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
			si.StartupInfo.dwFlags = 0x100; // STARTF_USESTDHANDLES
			si.StartupInfo.hStdInput = usize::MAX as HANDLE;
			si.StartupInfo.hStdOutput = usize::MAX as HANDLE;
			si.StartupInfo.hStdError = usize::MAX as HANDLE;
			si.lpAttributeList = attr_list;

			let mut command: Vec<u16> = command.encode_utf16().collect();
			command.push(0);

			let mut envs = envs.to_vec();
			envs.push(("DELTA_CTRL".to_string(), ctrl_name.clone()));
			let env_block = environment_block(&envs);
			let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
			let env_ptr = env_block.as_ptr() as *const c_void;

			let mut pi: PROCESS_INFORMATION = zeroed();
			let ok = CreateProcessW(
				std::ptr::null(),
				command.as_mut_ptr(),
				std::ptr::null(),
				std::ptr::null(),
				0,
				flags,
				env_ptr,
				std::ptr::null(),
				&si.StartupInfo,
				&mut pi,
			);

			DeleteProcThreadAttributeList(attr_list);

			if ok == 0 {
				(api.close)(hpc);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(ctrl_write);
				return Err(io::Error::last_os_error());
			}

			CloseHandle(pi.hThread);

			Ok(Self {
				hpc,
				input_write,
				output_read,
				ctrl_write,
				process: pi.hProcess,
				closed: false,
			})
		}
	}

	pub fn write(&self, bytes: &[u8]) {
		if bytes.is_empty() || self.closed {
			return;
		}
		let mut written = 0u32;
		unsafe {
			let _ = WriteFile(
				self.input_write,
				bytes.as_ptr(),
				bytes.len() as u32,
				&mut written,
				null_mut(),
			);
		}
	}

	pub fn resize(&self, cols: usize, rows: usize) {
		if self.closed {
			return;
		}
		unsafe {
			let api = conpty_api();
			(api.resize)(self.hpc, COORD {
				X: cols.max(1) as i16,
				Y: rows.max(1) as i16,
			});
		}
	}

	/// Ask the shell to end its foreground job. conhost on this build swallows
	/// the Ctrl+C key event without ever raising CTRL_C_EVENT, so the signal
	/// has to be raised on the shell's side instead.
	pub fn interrupt(&self) {
		if self.closed {
			return;
		}
		let mut written = 0u32;
		unsafe {
			let _ = WriteFile(
				self.ctrl_write,
				b"\x03".as_ptr(),
				1,
				&mut written,
				null_mut(),
			);
		}
	}

	pub fn output_read(&self) -> HANDLE {
		self.output_read
	}

	/// The console process has ended. conhost keeps the output pipe open after the
	/// client exits, so end of file never arrives; the process handle is the truth.
	pub fn exited(&self) -> bool {
		if self.process.is_null() {
			return true;
		}
		unsafe { WaitForSingleObject(self.process, 0) == 0 } // WAIT_OBJECT_0
	}

	pub fn close(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		let hpc = self.hpc as usize;
		let process = self.process as usize;
		let input_write = self.input_write as usize;
		let ctrl_write = self.ctrl_write as usize;
		// ClosePseudoConsole waits for the console to shut down and has been
		// seen to stall for seconds; never let that block the UI thread.
		std::thread::spawn(move || unsafe {
			let hpc = hpc as HPCON;
			let process = process as HANDLE;
			let input_write = input_write as HANDLE;
			let pid = GetProcessId(process);
			if pid != 0 {
				let _ = std::process::Command::new("taskkill")
					.args(["/PID", &pid.to_string(), "/T", "/F"])
					.stdin(std::process::Stdio::null())
					.stdout(std::process::Stdio::null())
					.stderr(std::process::Stdio::null())
					.status();
			}
			let _ = TerminateProcess(process, 0);
			(conpty_api().close)(hpc);
			CloseHandle(process);
			CloseHandle(input_write);
			CloseHandle(ctrl_write as HANDLE);
		});
	}
}

impl Drop for ConPty {
	fn drop(&mut self) {
		self.close();
	}
}

fn environment_block(extra: &[(String, String)]) -> Vec<u16> {
	let mut entries: Vec<(String, String)> = std::env::vars_os()
		.filter_map(|(key, value)| Some((key.to_str()?.to_string(), value.to_str()?.to_string())))
		.filter(|(key, _)| !extra.iter().any(|(name, _)| name.eq_ignore_ascii_case(key)))
		.collect();
	entries.extend(extra.iter().cloned());
	entries.sort_by(|a, b| a.0.to_ascii_uppercase().cmp(&b.0.to_ascii_uppercase()));

	let mut block = Vec::new();
	for (key, value) in entries {
		if key.is_empty() || key.contains('\0') || value.contains('\0') {
			continue;
		}
		block.extend(format!("{key}={value}").encode_utf16());
		block.push(0);
	}
	block.push(0);
	block
}

type CreatePseudoConsoleFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

struct ConPtyApi {
	create: CreatePseudoConsoleFn,
	resize: ResizePseudoConsoleFn,
	close: ClosePseudoConsoleFn,
}

unsafe impl Send for ConPtyApi {}
unsafe impl Sync for ConPtyApi {}

static CONPTY_API: OnceLock<ConPtyApi> = OnceLock::new();

fn conpty_api() -> &'static ConPtyApi {
	CONPTY_API.get_or_init(load_conpty_api)
}

unsafe fn resolve(module: HMODULE, name: &str) -> FARPROC {
	if module.is_null() {
		return None;
	}
	let mut bytes = name.as_bytes().to_vec();
	bytes.push(0);
	unsafe { GetProcAddress(module, bytes.as_ptr()) }
}

fn load_conpty_api() -> ConPtyApi {
	unsafe {
		let kernel_name: Vec<u16> = "kernel32.dll".encode_utf16().chain(std::iter::once(0)).collect();
		let kernel = GetModuleHandleW(kernel_name.as_ptr());
		let create = resolve(kernel, "CreatePseudoConsole").expect("CreatePseudoConsole");
		let resize = resolve(kernel, "ResizePseudoConsole").expect("ResizePseudoConsole");
		let close = resolve(kernel, "ClosePseudoConsole").expect("ClosePseudoConsole");
		ConPtyApi { create: transmute(create), resize: transmute(resize), close: transmute(close) }
	}
}
