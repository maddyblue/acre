use crate::Result;
use crossbeam_channel::{unbounded, Receiver};
use lsp_types::{request::*, *};
use serde::ser::Serialize;
use serde_json;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;

pub struct Client {
	pub name: String,
	proc: Child,
	files: String,
	stdin: ChildStdin,
	next_id: usize,
	pub msg_r: Receiver<Vec<u8>>,
}

impl Client {
	#![allow(deprecated)]
	pub fn new<I, S>(
		name: String,
		files: String,
		program: S,
		args: I,
		root_uri: &str,
		workspace_folders: Option<Vec<&str>>,
	) -> Result<Client>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<std::ffi::OsStr>,
	{
		let mut proc = Command::new(program)
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.spawn()?;
		let mut stdout = BufReader::new(proc.stdout.take().unwrap());
		let stdin = proc.stdin.take().unwrap();
		let (msg_s, msg_r) = unbounded();
		let mut c = Client {
			name,
			files,
			proc,
			stdin,
			next_id: 1,
			msg_r,
		};
		thread::spawn(move || loop {
			let mut line = String::new();
			let mut content_len: usize = 0;
			loop {
				line.clear();
				stdout.read_line(&mut line).unwrap();
				if line.trim().len() == 0 {
					break;
				}
				let sp: Vec<&str> = line.trim().split(": ").collect();
				if sp.len() < 2 {
					panic!("bad line: {}", line);
				}
				match sp[0] {
					"Content-Length" => {
						content_len = sp[1].parse().unwrap();
					}
					_ => {
						panic!("unrecognized header: {}", sp[0]);
					}
				}
			}
			if content_len == 0 {
				panic!("expected content-length");
			}
			let mut v = vec![0u8; content_len];
			stdout.read_exact(&mut v).unwrap();
			//println!("got: {}", std::str::from_utf8(&v).unwrap());
			msg_s.send(v).unwrap();
		});
		// TODO: remove the unwrap here. Unsure how to bubble up errors
		// from a closure.
		let workspace_folders: Option<Vec<WorkspaceFolder>> = match workspace_folders {
			Some(f) => Some(
				f.iter()
					.map(|x| WorkspaceFolder {
						uri: Url::parse(x).unwrap(),
						name: "".to_string(),
					})
					.collect(),
			),
			None => None,
		};
		let i = InitializeParams {
			process_id: Some(1),
			root_path: None,
			root_uri: Some(Url::parse(root_uri)?),
			initialization_options: None,
			capabilities: ClientCapabilities::default(),
			trace: None,
			workspace_folders,
			client_info: None,
		};
		c.send::<Initialize, InitializeParams>(i).unwrap();
		Ok(c)
	}
	pub fn send<R: Request, S: Serialize>(&mut self, params: S) -> Result<()> {
		let id = self.new_id()?;
		let msg = Message {
			jsonrpc: "2.0",
			id,
			method: R::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		write!(self.stdin, "{}", s)?;
		Ok(())
	}
	pub fn wait(&mut self) -> Result<()> {
		self.proc.wait()?;
		Ok(())
	}
	fn new_id(&mut self) -> Result<usize> {
		let id = self.next_id;
		self.next_id += 1;
		Ok(id)
	}
}

impl Drop for Client {
	fn drop(&mut self) {
		let _ = self.proc.kill();
	}
}

#[derive(serde::Serialize)]
struct Message<P> {
	jsonrpc: &'static str,
	id: usize,
	method: &'static str,
	params: P,
}

#[derive(Debug, serde::Deserialize)]
pub struct DeMessage<'a> {
	pub id: Option<usize>,
	pub method: Option<&'a str>,
	#[serde(borrow)]
	pub params: Option<&'a serde_json::value::RawValue>,
	#[serde(borrow)]
	pub result: Option<&'a serde_json::value::RawValue>,
	pub error: Option<ResponseError>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ResponseError {
	pub code: usize,
	pub message: String,
	pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
	use crate::lsp::*;

	#[test]
	fn lsp() {
		let mut l = Client::new(
			".rs".to_string(),
			"rls",
			std::iter::empty(),
			"file:///home/mjibson/go/src/github.com/mjibson/plan9",
			None,
		)
		.unwrap();
		l.wait().unwrap();
	}
}
