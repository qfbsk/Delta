use std::ffi::c_void;
use std::io;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::null_mut;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{
	CloseHandle, FARPROC, HANDLE, HANDLE_FLAG_INHERIT, HMODULE, SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
	CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
	EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
	LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
	STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};

pub struct ConPty {
	hpc: HPCON,
	input_write: HANDLE,
	output_read: HANDLE,
	input_read: HANDLE,
	output_write: HANDLE,
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

			let mut hpc: HPCON = 0;
			let api = conpty_api();
			let hr = (api.create)(
				COORD {
					X: cols.max(1) as i16,
					Y: rows.max(1) as i16,
				},
				input_read,
				output_write,
				10,
				&mut hpc,
			);
			if hr < 0 {
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
				return Err(io::Error::last_os_error());
			}

			let mut attr_size = 0usize;
			let _ = InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
			let words = attr_size.div_ceil(size_of::<usize>()).max(1);
			let mut attr_buf = vec![0usize; words];
			let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
			if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
				(conpty_api().close)(hpc);
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
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
				(conpty_api().close)(hpc);
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
				return Err(io::Error::last_os_error());
			}

			let mut si: STARTUPINFOEXW = zeroed();
			si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
			// Keep the child from inheriting this process's redirected stdio.
			// The pseudoconsole attribute supplies the console instead.
			si.StartupInfo.dwFlags = 0x100; // STARTF_USESTDHANDLES
			si.StartupInfo.hStdInput = usize::MAX as HANDLE;
			si.StartupInfo.hStdOutput = usize::MAX as HANDLE;
			si.StartupInfo.hStdError = usize::MAX as HANDLE;
			si.lpAttributeList = attr_list;

			let mut command: Vec<u16> = command.encode_utf16().collect();
			command.push(0);

			let env_block = environment_block(envs);
			let mut flags = EXTENDED_STARTUPINFO_PRESENT;
			let mut env_ptr: *const c_void = std::ptr::null();
			if !envs.is_empty() {
				flags |= CREATE_UNICODE_ENVIRONMENT;
				env_ptr = env_block.as_ptr() as *const c_void;
			}

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
				(conpty_api().close)(hpc);
				CloseHandle(input_read);
				CloseHandle(input_write);
				CloseHandle(output_read);
				CloseHandle(output_write);
				return Err(io::Error::last_os_error());
			}

			CloseHandle(pi.hThread);

			Ok(Self {
				hpc,
				input_write,
				output_read,
				input_read,
				output_write,
				process: pi.hProcess,
				closed: false,
			})
		}
	}

	pub fn write(&self, bytes: &[u8]) {
		if bytes.is_empty() {
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
		unsafe {
			let api = conpty_api();
			let _ = (api.resize)(
				self.hpc,
				COORD {
					X: cols.max(1) as i16,
					Y: rows.max(1) as i16,
				},
			);
		}
	}

	pub fn output_read(&self) -> HANDLE {
		self.output_read
	}

	pub fn process_handle(&self) -> HANDLE {
		self.process
	}

	pub fn input_write(&self) -> HANDLE {
		self.input_write
	}

	pub fn close(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		unsafe {
			let _ = TerminateProcess(self.process, 0);
			(conpty_api().close)(self.hpc);
			CloseHandle(self.process);
			CloseHandle(self.input_write);
			CloseHandle(self.output_read);
			CloseHandle(self.input_read);
			CloseHandle(self.output_write);
		}
	}
}

fn environment_block(extra: &[(String, String)]) -> Vec<u16> {
	let mut entries: Vec<(String, String)> = std::env::vars_os()
		.filter_map(|(key, value)| {
			Some((key.to_str()?.to_string(), value.to_str()?.to_string()))
		})
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

pub fn smoke() -> io::Result<()> {
	use std::sync::{Arc, Mutex};
	use std::thread;
	use std::time::Duration;
	let pty = ConPty::spawn(80, 24, "cmd.exe /C echo hello-from-conpty")?;
	let out = pty.output_read() as usize;
	let data = Arc::new(Mutex::new(Vec::<u8>::new()));
	let d2 = data.clone();
	thread::spawn(move || {
		let out = out as HANDLE;
		let mut buf = [0u8; 4096];
		for _ in 0..16 {
			let mut n = 0u32;
			let ok =
				unsafe { ReadFile(out, buf.as_mut_ptr(), buf.len() as u32, &mut n, null_mut()) };
			if ok == 0 || n == 0 {
				break;
			}
			d2.lock().unwrap().extend_from_slice(&buf[..n as usize]);
		}
	});
	thread::sleep(Duration::from_millis(1200));
	use std::io::Write;
	let bytes = data.lock().unwrap().clone();
	let mut out = std::io::stdout();
	let _ = writeln!(
		out,
		"{} bytes: {}",
		bytes.len(),
		String::from_utf8_lossy(&bytes).trim_end()
	);
	let _ = out.flush();
	std::process::exit(0);
}

impl Drop for ConPty {
	fn drop(&mut self) {
		self.close();
	}
}

type CreatePseudoConsoleFn =
	unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
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
		let kernel_name: Vec<u16> = "kernel32.dll"
			.encode_utf16()
			.chain(std::iter::once(0))
			.collect();
		let kernel = GetModuleHandleW(kernel_name.as_ptr());
		let create = resolve(kernel, "CreatePseudoConsole").expect("CreatePseudoConsole");
		let resize = resolve(kernel, "ResizePseudoConsole").expect("ResizePseudoConsole");
		let close = resolve(kernel, "ClosePseudoConsole").expect("ClosePseudoConsole");
		ConPtyApi {
			create: transmute(create),
			resize: transmute(resize),
			close: transmute(close),
		}
	}
}

