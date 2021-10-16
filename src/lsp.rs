use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver};
use lsp_types::{notification::*, request::*, *};
use regex;
use serde_json;

pub struct Client {
	pub name: String,
	proc: Child,
	pub files: regex::Regex,
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
		envs: HashMap<String, String>,
		root_uri: Option<String>,
		workspace_folders: Option<Vec<String>>,
		options: Option<serde_json::Value>,
	) -> Result<(Client, usize)>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<std::ffi::OsStr> + std::fmt::Display + Clone,
	{
		let mut proc = Command::new(program.clone())
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.envs(envs)
			.spawn()
			.expect(&format!("could not execute: {}", program));
		let mut stdout = BufReader::new(proc.stdout.take().unwrap());
		let stdin = proc.stdin.take().unwrap();
		let (msg_s, msg_r) = unbounded();
		let mut c = Client {
			name,
			files: regex::Regex::new(&files)?,
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
		let root_uri = match root_uri {
			Some(u) => Some(Url::parse(&u)?),
			None => None,
		};
		let id = c.send::<Initialize>(InitializeParams {
			process_id: Some(1),
			root_path: None,
			root_uri,
			initialization_options: options,
			capabilities: ClientCapabilities {
				text_document: Some(TextDocumentClientCapabilities {
					code_action: Some(CodeActionClientCapabilities {
						resolve_support: Some(CodeActionCapabilityResolveSupport {
							properties: vec!["edit".to_string()],
						}),
						code_action_literal_support: Some(CodeActionLiteralSupport {
							code_action_kind: CodeActionKindLiteralSupport {
								value_set: vec![
									"".to_string(),
									"quickfix".to_string(),
									"refactor".to_string(),
									"refactor.extract".to_string(),
									"refactor.inline".to_string(),
									"refactor.rewrite".to_string(),
									"source".to_string(),
									"source.organizeImports".to_string(),
								],
							},
						}),
						..Default::default()
					}),
					..Default::default()
				}),
				..Default::default()
			},
			trace: None,
			workspace_folders,
			client_info: None,
			locale: None,
		})?;
		Ok((c, id))
	}
	pub fn send<R: Request>(&mut self, params: R::Params) -> Result<usize> {
		let id = self.new_id()?;
		let msg = RequestMessage {
			jsonrpc: "2.0",
			id,
			method: R::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		write!(self.stdin, "{}", s)?;
		Ok(id)
	}
	pub fn notify<N: Notification>(&mut self, params: N::Params) -> Result<()> {
		let msg = NotificationMessage {
			jsonrpc: "2.0",
			method: N::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		write!(self.stdin, "{}", s)?;
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
struct RequestMessage<P> {
	jsonrpc: &'static str,
	id: usize,
	method: &'static str,
	params: P,
}

#[derive(serde::Serialize)]
struct NotificationMessage<P> {
	jsonrpc: &'static str,
	method: &'static str,
	params: P,
}

#[derive(Debug, serde::Deserialize)]
pub struct DeMessage {
	pub id: Option<usize>,
	pub method: Option<String>,
	pub params: Option<Box<serde_json::value::RawValue>>,
	pub result: Option<Box<serde_json::value::RawValue>>,
	pub error: Option<ResponseError>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ResponseError {
	pub code: i64,
	pub message: String,
	pub data: Option<serde_json::Value>,
}
