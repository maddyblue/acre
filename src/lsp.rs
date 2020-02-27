use crate::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use lsp_types::{request::*, *};
use serde::ser::Serialize;
use serde_json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Client {
	proc: Child,
	stdin: ChildStdin,
	id_map: Arc<Mutex<HashMap<usize, Sender<Vec<u8>>>>>,
	next_id: usize,
}

impl Client {
	#![allow(deprecated)]
	pub fn new<I, S>(
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
		let mut c = Client {
			proc,
			stdin,
			id_map: Arc::new(Mutex::new(HashMap::new())),
			next_id: 1,
		};
		let im = Arc::clone(&c.id_map);
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
			println!("\n{}", std::str::from_utf8(&v).unwrap());
			/*
			let s = im
				.lock()
				.unwrap()
				.remove(&id)
				.expect(&format!("expected receiver with id {}", id));
			s.send(vec![]).unwrap();
			*/
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
		let (id, r) = self.new_id()?;
		let msg = Message {
			jsonrpc: "2.0",
			id,
			method: R::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		println!("send: {}", s);
		write!(self.stdin, "{}", s)?;
		Ok(())
	}
	pub fn wait(&mut self) -> Result<()> {
		self.proc.wait()?;
		Ok(())
	}
	fn new_id(&mut self) -> Result<(usize, Receiver<Vec<u8>>)> {
		let id = self.next_id;
		self.next_id += 1;
		let (s, r) = bounded(0);
		self.id_map.lock().unwrap().insert(id, s);
		Ok((id, r))
	}
}

#[derive(serde::Serialize)]
struct Message<P> {
	jsonrpc: &'static str,
	id: usize,
	method: &'static str,
	params: P,
}

#[cfg(test)]
mod tests {
	use crate::lsp::*;

	#[test]
	fn lsp() {
		let mut l = Client::new(
			"rls",
			std::iter::empty(),
			"file:///home/mjibson/go/src/github.com/mjibson/plan9",
			None,
		)
		.unwrap();
		l.wait().unwrap();
	}
}
