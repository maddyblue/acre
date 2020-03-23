use crate::Result;
use crossbeam_channel::{unbounded, Receiver};
use lsp_types::{notification, request::*, *};
use serde_json;
use std::any::Any;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Client {
	pub name: String,
	proc: Child,
	pub files: String,
	stdin: ChildStdin,
	next_id: usize,
	id_map: Arc<Mutex<HashMap<usize, String>>>,
	pub msg_r: Receiver<Box<dyn Send + Any>>,
}

impl Client {
	#![allow(deprecated)]
	pub fn new<I, S>(
		name: String,
		files: String,
		program: S,
		args: I,
		root_uri: Option<&str>,
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
			id_map: Arc::new(Mutex::new(HashMap::new())),
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
			if cfg!(debug_assertions) {
				println!("got: {}", std::str::from_utf8(&v).unwrap());
			}
			let msg: DeMessage = serde_json::from_reader(Cursor::new(&v)).unwrap();
			let d: Box<dyn Send + Any> = if let Some(err) = msg.error {
				Box::new(err)
			} else if let Some(id) = msg.id {
				if msg.params.is_some() {
					println!("unsupported server -> client message");
					continue;
				}
				let typ = im.lock().unwrap().remove(&id).unwrap();
				let res = match msg.result {
					Some(res) => res,
					None => continue,
				};
				// TODO: figure out how to pass the structs over the chan instead of strings.
				match typ.as_str() {
					Initialize::METHOD => {
						Box::new(serde_json::from_str::<InitializeResult>(res.get()).unwrap())
					}
					GotoDefinition::METHOD => Box::new(
						serde_json::from_str::<Option<GotoDefinitionResponse>>(res.get()).unwrap(),
					),
					HoverRequest::METHOD => {
						Box::new(serde_json::from_str::<Option<Hover>>(res.get()).unwrap())
					}
					Completion::METHOD => Box::new(
						serde_json::from_str::<Option<CompletionResponse>>(res.get()).unwrap(),
					),
					References::METHOD => {
						Box::new(serde_json::from_str::<Option<Vec<Location>>>(res.get()).unwrap())
					}
					DocumentSymbolRequest::METHOD => Box::new(
						serde_json::from_str::<Option<DocumentSymbolResponse>>(res.get()).unwrap(),
					),
					_ => panic!("unrecognized type: {}", typ),
				}
			} else if let Some(method) = msg.method {
				match method.as_str() {
					"window/progress" => Box::new(
						serde_json::from_str::<WindowProgress>(msg.params.unwrap().get()).unwrap(),
					),
					"textDocument/publishDiagnostics" => Box::new(
						serde_json::from_str::<lsp_types::PublishDiagnosticsParams>(
							msg.params.unwrap().get(),
						)
						.unwrap(),
					),
					"window/showMessage" => Box::new(
						serde_json::from_str::<lsp_types::ShowMessageParams>(
							msg.params.unwrap().get(),
						)
						.unwrap(),
					),
					"$/progress" => Box::new(
						serde_json::from_str::<lsp_types::ProgressParams>(
							msg.params.unwrap().get(),
						)
						.unwrap(),
					),
					_ => {
						panic!("unrecognized method: {}", method);
					}
				}
			} else {
				panic!("unhandled lsp msg: {:?}", msg);
			};
			msg_s.send(d).unwrap();
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
			Some(u) => Some(Url::parse(u)?),
			None => None,
		};
		c.send::<Initialize>(InitializeParams {
			process_id: Some(1),
			root_path: None,
			root_uri,
			initialization_options: None,
			capabilities: ClientCapabilities::default(),
			trace: None,
			workspace_folders,
			client_info: None,
		})
		.unwrap();
		Ok(c)
	}
	pub fn send<R: Request>(&mut self, params: R::Params) -> Result<()> {
		let id = self.new_id::<R>()?;
		let msg = Message {
			jsonrpc: "2.0",
			id,
			method: R::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		if cfg!(debug_assertions) {
			println!("send request: {}", s);
		}
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		write!(self.stdin, "{}", s)?;
		Ok(())
	}
	pub fn notify<N: notification::Notification>(&mut self, params: N::Params) -> Result<()> {
		let msg = Notification {
			jsonrpc: "2.0",
			method: N::METHOD,
			params,
		};
		let s = serde_json::to_string(&msg)?;
		if cfg!(debug_assertions) {
			println!("send notification: {}", msg.method);
		}
		let s = format!("Content-Length: {}\r\n\r\n{}", s.len(), s);
		write!(self.stdin, "{}", s)?;
		Ok(())
	}
	pub fn wait(&mut self) -> Result<()> {
		self.proc.wait()?;
		Ok(())
	}
	fn new_id<R: Request>(&mut self) -> Result<usize> {
		let id = self.next_id;
		self.next_id += 1;
		self.id_map
			.lock()
			.unwrap()
			.insert(id, R::METHOD.to_string());
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

#[derive(serde::Serialize)]
struct Notification<P> {
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

#[cfg(test)]
mod tests {
	use crate::lsp::*;

	#[test]
	fn lsp() {
		let mut l = Client::new(
			"rls".to_string(),
			".rs".to_string(),
			"rls",
			std::iter::empty(),
			Some("file:///home/mjibson/go/src/github.com/mjibson/plan9"),
			None,
		)
		.unwrap();
		l.wait().unwrap();
	}
}

#[derive(Debug, serde::Deserialize)]
pub struct WindowProgress {
	pub done: Option<bool>,
	pub id: String,
	pub message: Option<String>,
	pub title: Option<String>,
	pub percentage: Option<f64>,
}
