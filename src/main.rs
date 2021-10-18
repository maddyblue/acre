use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::metadata;
use std::io::Read;
use std::thread;

use anyhow::{bail, Error, Result};
use crossbeam_channel::{bounded, Receiver, Select};
use diff;
use lazy_static::lazy_static;
use lsp_types::{notification::*, request::*, *};
use nine::p2000::OpenMode;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use plan9::{acme::*, plumb};

mod lsp;

#[derive(Deserialize)]
struct TomlConfig {
	servers: HashMap<String, ConfigServer>,
}

#[derive(Clone, Deserialize)]
struct ConfigServer {
	executable: Option<String>,
	args: Option<Vec<String>>,
	files: String,
	root_uri: Option<String>,
	workspace_folders: Option<Vec<String>>,
	options: Option<Value>,
	actions_on_put: Option<Vec<CodeActionKind>>,
	format_on_put: Option<bool>,
	env: Option<HashMap<String, String>>,
}

fn main() -> Result<()> {
	let dir = xdg::BaseDirectories::new()?;
	const ACRE_TOML: &str = "acre.toml";
	let config = match dir.find_config_file(ACRE_TOML) {
		Some(c) => c,
		None => {
			let mut path = dir.get_config_home();
			path.push(ACRE_TOML);
			eprintln!("could not find {}", path.to_str().unwrap());
			std::process::exit(1);
		}
	};
	let config = std::fs::read_to_string(config)?;
	let config: TomlConfig = toml::from_str(&config)?;
	if config.servers.is_empty() {
		eprintln!("empty servers in configuration file");
		std::process::exit(1);
	}
	let mut s = Server::new(config)?;
	s.wait()
}

struct WDProgress {
	name: String,
	percentage: Option<u32>,
	message: Option<String>,
	title: String,
}

impl WDProgress {
	fn new(
		name: String,
		percentage: Option<u32>,
		message: Option<String>,
		title: Option<String>,
	) -> Self {
		Self {
			name,
			percentage,
			message,
			title: title.unwrap_or("".to_string()),
		}
	}
}

impl std::fmt::Display for WDProgress {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"[{}%] {}:{} ({})",
			format_pct(self.percentage),
			self.name,
			if let Some(msg) = &self.message {
				format!(" {}", msg)
			} else {
				"".to_string()
			},
			self.title,
		)
	}
}

#[derive(Debug, Clone)]
enum Action {
	Command(CodeActionOrCommand),
	Completion(CompletionItem),
	CodeLens(CodeLens),
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
struct ClientId {
	client_name: String,
	msg_id: usize,
}

impl ClientId {
	fn new<S: Into<String>>(client_name: S, msg_id: usize) -> Self {
		ClientId {
			client_name: client_name.into(),
			msg_id,
		}
	}
}

struct Server {
	config: TomlConfig,
	w: Win,
	ws: HashMap<usize, ServerWin>,
	/// Sorted Vec of (filenames, win id) to know which order to print windows in.
	names: Vec<(String, usize)>,
	/// Vec of (position, win id) to map Look locations to windows.
	addr: Vec<(usize, usize)>,
	/// Set of opened files. Needed to distinguish Zerox'd windows.
	opened_urls: HashSet<Url>,

	body: String,
	output: String,
	focus: String,
	progress: HashMap<String, WDProgress>,
	/// file name -> list of diagnostics
	diags: HashMap<String, Vec<String>>,
	/// request (client_name, id) -> (method, file Url)
	requests: HashMap<ClientId, (String, Url)>,

	/// current window info
	current_hover: Option<WindowHover>,

	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	err_r: Receiver<Error>,

	/// client name -> client
	clients: HashMap<String, lsp::Client>,
	/// client name -> capabilities
	capabilities: HashMap<String, lsp_types::ServerCapabilities>,
	/// file name -> client name
	files: HashMap<String, String>,
	/// list of LSP message IDs to auto-run actions
	autorun: HashMap<usize, ()>,
}

struct WindowHover {
	client_name: String,
	url: Url,
	/// line text of the hover.
	line: String,
	/// token (word at the cursor) of the hover.
	token: Option<String>,
	/// on hover response from lsp
	hover: Option<String>,
	/// result of signature request
	signature: Option<String>,
	lens: Vec<CodeLens>,
	/// completion response. we need to cache this because we also need the token
	/// response to come, and we don't know which will come first.
	completion: Vec<CompletionItem>,
	code_actions: Vec<Action>,

	/// merged actions from the code action and completion requests
	actions: Vec<Action>,
	/// Vec of (position, index) into the vec of actions
	action_addrs: Vec<(usize, usize)>,
	/// cached output result of hover and actions
	body: String,
}

struct ServerWin {
	w: Win,
	url: Url,
	version: i32,
	client: String,
}

impl ServerWin {
	fn new(name: String, w: Win, client: String) -> Result<ServerWin> {
		let url = Url::parse(&format!("file://{}", name))?;
		let version = 1;
		Ok(ServerWin {
			w,
			url,
			version,
			client,
		})
	}
	fn pos(&mut self) -> Result<(u32, u32)> {
		self.w.ctl("addr=dot")?;
		// TODO: convert these character (rune) offsets to byte offsets.
		self.w.read_addr()
	}
	fn nl(&mut self) -> Result<NlOffsets> {
		NlOffsets::new(self.w.read(File::Body)?)
	}
	fn position(&mut self) -> Result<Position> {
		let pos = self.pos()?;
		let nl = self.nl()?;
		let (line, col) = nl.offset_to_line(pos.0);
		Ok(Position::new(line, col))
	}
	fn text(&mut self) -> Result<(i32, String)> {
		let mut buf = String::new();
		self.w.read(File::Body)?.read_to_string(&mut buf)?;
		self.version += 1;
		Ok((self.version, buf))
	}
	fn change_params(&mut self) -> Result<DidChangeTextDocumentParams> {
		let (version, text) = self.text()?;
		Ok(DidChangeTextDocumentParams {
			text_document: VersionedTextDocumentIdentifier::new(self.url.clone(), version),
			content_changes: vec![TextDocumentContentChangeEvent {
				range: None,
				range_length: None,
				text,
			}],
		})
	}
	fn doc_ident(&self) -> TextDocumentIdentifier {
		TextDocumentIdentifier::new(self.url.clone())
	}
	fn text_doc_pos(&mut self) -> Result<TextDocumentPositionParams> {
		let pos = self.position()?;
		Ok(TextDocumentPositionParams::new(self.doc_ident(), pos))
	}
	/// Returns the current line's text.
	fn line(&mut self) -> Result<String> {
		let mut buf = String::new();
		self.w.read(File::Body)?.read_to_string(&mut buf)?;
		let pos = self.pos()?;
		let nl = NlOffsets::new(buf.as_bytes())?;
		let (line, _col) = nl.offset_to_line(pos.0);
		let line = buf
			.lines()
			.nth(line as usize)
			.ok_or(anyhow::anyhow!("no such line"))?
			.to_string();
		Ok(line)
	}
}

impl Server {
	fn new(config: TomlConfig) -> Result<Server> {
		let mut clients = vec![];
		let mut requests = HashMap::new();
		for (name, server) in config.servers.clone() {
			let (client, msg_id) = lsp::Client::new(
				name.clone(),
				server.files,
				server.executable.unwrap_or(name.clone()),
				server.args.unwrap_or(vec![]),
				server.env.unwrap_or(HashMap::new()),
				server.root_uri,
				server.workspace_folders,
				server.options,
			)?;
			requests.insert(
				ClientId::new(name, msg_id),
				(Initialize::METHOD.into(), Url::parse("file:///").unwrap()),
			);
			clients.push(client);
		}

		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let mut w = Win::new()?;
		w.name("acre")?;
		let mut wev = w.events()?;
		let mut cls = HashMap::new();
		for c in clients {
			let name = c.name.clone();
			cls.insert(name, c);
		}
		let s = Server {
			w,
			ws: HashMap::new(),
			names: vec![],
			opened_urls: HashSet::new(),
			addr: vec![],
			output: "".to_string(),
			body: "".to_string(),
			focus: "".to_string(),
			progress: HashMap::new(),
			requests,
			diags: HashMap::new(),
			current_hover: None,
			log_r,
			ev_r,
			err_r,
			clients: cls,
			capabilities: HashMap::new(),
			files: HashMap::new(),
			config,
			autorun: HashMap::new(),
		};
		let err_s1 = err_s.clone();
		thread::Builder::new()
			.name("LogReader".to_string())
			.spawn(move || {
				let mut log = LogReader::new().unwrap();
				loop {
					match log.read() {
						Ok(ev) => match ev.op.as_str() {
							"new" | "del" | "focus" | "put" => match log_s.send(ev) {
								Ok(_) => {}
								Err(_err) => {
									//eprintln!("log_s send err {}", _err);
									return;
								}
							},
							_ => {}
						},
						Err(err) => {
							err_s1.send(err).unwrap();
							return;
						}
					};
				}
			})
			.unwrap();
		thread::Builder::new()
			.name("WindowEvents".to_string())
			.spawn(move || loop {
				let mut ev = match wev.read_event() {
					Ok(ev) => ev,
					Err(err) => {
						eprintln!("read event err {}", err);
						return;
					}
				};
				match ev.c2 {
					'x' | 'X' => match ev.text.as_str() {
						"Del" => {
							return;
						}
						"Get" => {
							ev_s.send(ev).unwrap();
						}
						_ => {
							wev.write_event(ev).unwrap();
						}
					},
					'L' => {
						ev.load_text();
						ev_s.send(ev).unwrap();
					}
					_ => {}
				}
			})
			.unwrap();
		Ok(s)
	}
	/// Runs f if self.current_hover is Some and matches the Url, and updates the hover action addrs
	/// and body.
	fn set_hover<F: FnOnce(&mut WindowHover)>(&mut self, url: &Url, f: F) {
		if let Some(hover) = self.current_hover.as_mut() {
			if &hover.url == url {
				f(hover);

				hover.actions.clear();
				hover.actions.extend(hover.code_actions.clone());
				if let Some(token) = &hover.token {
					let mut v = vec![];
					for a in &hover.completion {
						let filter = if let Some(ref filter) = a.filter_text {
							filter.clone()
						} else {
							a.label.clone()
						};
						if filter.contains(token) {
							v.push(Action::Completion(a.clone()));
							if v.len() == 10 {
								break;
							}
						}
					}
					hover.actions.extend(v);
				}
				hover
					.actions
					.extend(hover.lens.iter().map(|lens| Action::CodeLens(lens.clone())));

				hover.body.clear();
				if let Some(text) = &hover.hover {
					hover.body.push_str(text.trim());
					hover.body.push_str("\n");
				}
				if let Some(text) = &hover.signature {
					if !hover.body.is_empty() {
						hover.body.push_str("\n");
					}
					hover.body.push_str(text.trim());
					hover.body.push_str("\n");
				}

				hover.action_addrs.clear();
				for (idx, action) in hover.actions.iter().enumerate() {
					if idx == 0 && !hover.body.is_empty() {
						hover.body.push_str("\n");
					}
					hover.action_addrs.push((hover.body.len(), idx));
					let newline = if hover.body.is_empty() { "" } else { "\n" };
					match action {
						Action::Command(CodeActionOrCommand::Command(cmd)) => {
							write!(&mut hover.body, "{}[{}]", newline, cmd.title).unwrap();
						}
						Action::Command(CodeActionOrCommand::CodeAction(action)) => {
							write!(&mut hover.body, "{}[{}]", newline, action.title).unwrap();
						}
						Action::Completion(item) => {
							write!(&mut hover.body, "\n[insert] {}:", item.label).unwrap();
							if item.deprecated.unwrap_or(false) {
								write!(&mut hover.body, " DEPRECATED").unwrap();
							}
							if let Some(k) = item.kind {
								write!(&mut hover.body, " ({:?})", k).unwrap();
							}
							if let Some(d) = &item.detail {
								write!(&mut hover.body, " {}", d).unwrap();
							}
						}
						// TODO: extract out the range text and append it to the command title to
						// distinguish between lenses.
						Action::CodeLens(_lens) => {
							/*
							write!(
								&mut hover.body,
								"{}[{}]",
								newline,
								lens.command
									.as_ref()
									.map(|c| c.title.clone())
									.unwrap_or("unknown command".into())
							)
							.unwrap();
							*/
						}
					}
				}
				hover.action_addrs.push((hover.body.len(), 100000));
			}
		}
	}
	fn winid_by_name(&self, filename: &str) -> Option<usize> {
		for (name, id) in &self.names {
			if filename == name {
				return Some(*id);
			}
		}
		None
	}
	fn get_sw_by_name(&mut self, filename: &str) -> Result<&mut ServerWin> {
		let wid = self.winid_by_name(filename);
		let wid = match wid {
			Some(id) => id,
			None => bail!("could not find file {}", filename),
		};
		let sw = match self.ws.get_mut(&wid) {
			Some(sw) => sw,
			None => bail!("could not find window {}", wid),
		};
		Ok(sw)
	}
	fn get_sw_by_url(&mut self, url: &Url) -> Result<&mut ServerWin> {
		let filename = url.path();
		self.get_sw_by_name(filename)
	}
	fn sync(&mut self) -> Result<()> {
		let mut body = String::new();
		if let Some(hover) = &self.current_hover {
			write!(&mut body, "{}\n----\n", hover.body)?;
		}
		for (_, ds) in &self.diags {
			for d in ds {
				write!(&mut body, "{}\n", d)?;
			}
			if ds.len() > 0 {
				body.push('\n');
			}
		}
		self.addr.clear();
		for (file_name, id) in &self.names {
			self.addr.push((body.len(), *id));
			write!(
				&mut body,
				"{}{}\n\t",
				if *file_name == self.focus { "*" } else { " " },
				file_name
			)?;
			let client_name = self.files.get(file_name).unwrap();
			let caps = match self.capabilities.get(client_name) {
				Some(v) => v,
				None => continue,
			};
			if caps.definition_provider.is_some() {
				body.push_str("[definition] ");
			}
			if caps.implementation_provider.is_some() {
				body.push_str("[impl] ");
			}
			if caps.references_provider.is_some() {
				body.push_str("[references] ");
			}
			if caps.document_symbol_provider.is_some() {
				body.push_str("[symbols] ");
			}
			if caps.type_definition_provider.is_some() {
				body.push_str("[typedef] ");
			}
			body.push('\n');
		}
		self.addr.push((body.len(), 0));
		write!(&mut body, "-----\n")?;
		if !self.output.is_empty() {
			// Only take the first 50 lines.
			let output = self
				.output
				.trim()
				.lines()
				.take(50)
				.collect::<Vec<_>>()
				.join("\n");
			write!(&mut body, "\n{}\n", output)?;
		}
		if self.progress.len() > 0 {
			body.push('\n');
		}
		for (_, p) in &self.progress {
			write!(&mut body, "{}\n", p)?;
		}
		if self.requests.len() > 0 {
			body.push('\n');
		}
		for (client_id, (method, url)) in &self.requests {
			write!(
				&mut body,
				"{}: {}: {}...\n",
				client_id.client_name,
				url.path(),
				method
			)?;
		}
		if self.body != body {
			self.body = body.clone();
			self.w.write(File::Addr, &format!(","))?;
			self.w.write(File::Data, &body)?;
			self.w.ctl("cleartag\nclean")?;
			self.w.write(File::Tag, " Get")?;
		}
		Ok(())
	}
	fn sync_windows(&mut self) -> Result<()> {
		let mut ws = HashMap::new();
		let mut wins = WinInfo::windows()?;
		self.names.clear();
		wins.sort_by(|a, b| {
			if a.name != b.name {
				return a.name.cmp(&b.name);
			} else {
				a.id.cmp(&b.id)
			}
		});
		self.files.clear();
		for wi in wins {
			let mut client = None;
			for (_, c) in self.clients.iter_mut() {
				if c.files.is_match(&wi.name) {
					// Don't open windows for a client that hasn't initialized yet.
					if !self.capabilities.contains_key(&c.name) {
						continue;
					}
					self.files.insert(wi.name.clone(), c.name.clone());
					client = Some(c);
					break;
				}
			}
			let client = match client {
				Some(c) => c,
				None => continue,
			};
			self.names.push((wi.name.clone(), wi.id));
			let w = match self.ws.remove(&wi.id) {
				Some(w) => w,
				None => {
					let mut fsys = FSYS.lock().unwrap();
					let ctl = fsys.open(format!("{}/ctl", wi.id).as_str(), OpenMode::RDWR)?;
					let w = Win::open(&mut fsys, wi.id, ctl)?;
					// Explicitly drop fsys here to remove its lock to prevent deadlocking if we
					// call w.events().
					drop(fsys);
					let mut sw = ServerWin::new(wi.name, w, client.name.clone())?;

					// Send an open event if this is the first time we've seen this filename (ID
					// tracking is not enough due to Zerox).
					if self.opened_urls.insert(sw.url.clone()) {
						let (version, text) = sw.text()?;
						self.send_notification::<DidOpenTextDocument>(
							&sw.client,
							DidOpenTextDocumentParams {
								text_document: TextDocumentItem::new(
									sw.url.clone(),
									"".to_string(), // lang id
									version,
									text,
								),
							},
						)?;
					}

					sw
				}
			};
			ws.insert(wi.id, w);
		}

		// close remaining files
		let to_close: Vec<(String, TextDocumentIdentifier)> = self
			.ws
			.iter()
			.map(|(_, w)| (w.client.clone(), w.doc_ident()))
			.collect();
		for (client_name, text_document) in to_close {
			self.opened_urls.remove(&text_document.uri);
			self.send_notification::<DidCloseTextDocument>(
				&client_name,
				DidCloseTextDocumentParams { text_document },
			)?;
		}
		self.ws = ws;
		Ok(())
	}
	fn lsp_msg(&mut self, client_name: String, orig_msg: Vec<u8>) -> Result<()> {
		let msg: lsp::DeMessage = serde_json::from_slice(&orig_msg)?;
		if msg.id.is_some() && msg.error.is_some() {
			self.lsp_error(
				ClientId::new(client_name, msg.id.unwrap()),
				msg.error.unwrap(),
			)
		} else if msg.id.is_some() && msg.method.is_some() {
			self.lsp_request(msg)
		} else if msg.id.is_some() {
			self.lsp_response(ClientId::new(client_name, msg.id.unwrap()), msg, &orig_msg)
		} else if msg.method.is_some() {
			self.lsp_notification(client_name, msg.method.unwrap(), msg.params)
		} else {
			panic!(
				"unknown message {}",
				std::str::from_utf8(&orig_msg).unwrap()
			);
		}
	}
	fn lsp_error(&mut self, client_id: ClientId, err: lsp::ResponseError) -> Result<()> {
		self.requests.remove(&client_id);
		self.output = format!("{}", err.message);
		Ok(())
	}
	fn lsp_response(
		&mut self,
		client_id: ClientId,
		msg: lsp::DeMessage,
		_orig_msg: &[u8],
	) -> Result<()> {
		let (typ, url) = self
			.requests
			.remove(&client_id)
			.expect(&format!("expected client id {:?}", client_id));
		let result = match msg.result {
			Some(v) => v,
			None => {
				// Ignore empty results. Unsure if/how we should report this to a user.
				return Ok(());
			}
		};
		match typ.as_str() {
			Initialize::METHOD => {
				let msg = serde_json::from_str::<InitializeResult>(result.get())?;
				self.send_notification::<Initialized>(
					&client_id.client_name,
					InitializedParams {},
				)?;
				self.capabilities
					.insert(client_id.client_name, msg.capabilities);
				self.sync_windows()?;
			}
			GotoDefinition::METHOD => {
				let msg = serde_json::from_str::<Option<GotoDefinitionResponse>>(result.get())?;
				if let Some(msg) = msg {
					goto_definition(&msg)?;
				}
			}
			HoverRequest::METHOD => {
				let msg = serde_json::from_str::<Option<Hover>>(result.get())?;
				if let Some(msg) = msg {
					match &msg.contents {
						HoverContents::Array(mss) => {
							let mut o: Vec<String> = vec![];
							for ms in mss {
								match ms {
									MarkedString::String(s) => o.push(s.clone()),
									MarkedString::LanguageString(s) => o.push(s.value.clone()),
								};
							}
							self.set_hover(&url, |hover| {
								hover.hover = Some(o.join("\n").trim().to_string());
							});
						}
						HoverContents::Markup(mc) => {
							self.set_hover(&url, |hover| {
								hover.hover = Some(mc.value.trim().to_string());
							});
						}
						_ => panic!("unknown hover response: {:?}", msg),
					};
				}
			}
			References::METHOD => {
				let msg = serde_json::from_str::<Option<Vec<Location>>>(result.get())?;
				if let Some(mut msg) = msg {
					msg.sort_by(cmp_location);
					let o: Vec<String> = msg.into_iter().map(|x| location_to_plumb(&x)).collect();
					if o.len() > 0 {
						self.output = o.join("\n");
					}
				}
			}
			DocumentSymbolRequest::METHOD => {
				let msg = serde_json::from_str::<Option<DocumentSymbolResponse>>(result.get())?;
				if let Some(msg) = msg {
					let mut o: Vec<String> = vec![];
					fn add_symbol(
						o: &mut Vec<String>,
						container: &Vec<String>,
						name: &String,
						kind: SymbolKind,
						loc: &Location,
					) {
						o.push(format!(
							"{}{} ({:?}): {}",
							container
								.iter()
								.map(|c| format!("{}::", c))
								.collect::<Vec<String>>()
								.join(""),
							name,
							kind,
							location_to_plumb(loc),
						));
					}
					match msg.clone() {
						DocumentSymbolResponse::Flat(sis) => {
							for si in sis {
								// Ignore variables in methods.
								if si.container_name.as_ref().unwrap_or(&"".to_string()).len() == 0
									&& si.kind == SymbolKind::Variable
								{
									continue;
								}
								let cn = match si.container_name.clone() {
									Some(c) => vec![c],
									None => vec![],
								};
								add_symbol(&mut o, &cn, &si.name, si.kind, &si.location);
							}
						}
						DocumentSymbolResponse::Nested(mut dss) => {
							fn process(
								url: &Url,
								mut o: &mut Vec<String>,
								parents: &Vec<String>,
								dss: &mut Vec<DocumentSymbol>,
							) {
								dss.sort_by(|a, b| a.range.start.line.cmp(&b.range.start.line));
								for ds in dss {
									add_symbol(
										&mut o,
										parents,
										&ds.name,
										ds.kind,
										&Location::new(url.clone(), ds.range),
									);
									if let Some(mut children) = ds.children.clone() {
										let mut parents = parents.clone();
										parents.push(ds.name.clone());
										process(url, o, &parents, &mut children);
									}
								}
							}
							process(&url, &mut o, &vec![], &mut dss);
						}
					}
					if o.len() > 0 {
						self.output = o.join("\n");
					}
				}
			}
			SignatureHelpRequest::METHOD => {
				let msg = serde_json::from_str::<Option<SignatureHelp>>(result.get())?;
				if let Some(msg) = msg {
					let sig = match msg.active_signature {
						Some(i) => i,
						None => 0,
					};
					let sig = msg
						.signatures
						.get(sig as usize)
						.ok_or(anyhow::anyhow!("expected signature"))?;
					self.set_hover(&url, |hover| {
						let mut s: String = sig.label.clone();
						if let Some(doc) = &sig.documentation {
							s.push_str("\n");
							s.push_str(extract_doc(doc));
						}
						hover.signature = Some(s);
					});
				}
			}
			CodeLensRequest::METHOD => {
				let msg = serde_json::from_str::<Option<Vec<CodeLens>>>(result.get())?;
				if let Some(msg) = msg {
					self.set_hover(&url, |hover| {
						hover.lens = msg;
					});
				}
			}
			CodeActionRequest::METHOD => {
				let msg = serde_json::from_str::<Option<CodeActionResponse>>(result.get())?;
				if let Some(msg) = msg {
					if self.autorun.remove_entry(&client_id.msg_id).is_some() {
						for m in msg.iter().cloned() {
							self.run_action(
								&client_id.client_name,
								url.clone(),
								Action::Command(m),
							)?;
						}
					} else {
						self.set_hover(&url, |hover| {
							for m in msg.iter().cloned() {
								hover.code_actions.push(Action::Command(m));
							}
						});
					}
				}
			}
			CodeActionResolveRequest::METHOD => {
				let msg = serde_json::from_str::<Option<CodeAction>>(result.get())?;
				if let Some(msg) = msg {
					if let Some(edit) = msg.edit {
						self.apply_workspace_edit(&edit)?;
					} else {
						eprintln!("unexpected CodeActionResolveRequest response: {:#?}", msg);
					}
				}
			}
			Completion::METHOD => {
				let msg = serde_json::from_str::<Option<CompletionResponse>>(result.get())?;
				if let Some(msg) = msg {
					self.set_hover(&url, move |hover| {
						hover.completion = match msg {
							CompletionResponse::Array(cis) => cis,
							CompletionResponse::List(cls) => cls.items,
						};
					});
				}
			}
			Formatting::METHOD => {
				let msg = serde_json::from_str::<Option<Vec<TextEdit>>>(result.get())?;
				if let Some(msg) = msg {
					self.apply_text_edits(&url, InsertTextFormat::PlainText, &msg)?;
					// Run any on put actions.
					let actions = self
						.config
						.servers
						.get(&client_id.client_name)
						.unwrap()
						.actions_on_put
						.clone()
						.unwrap_or(vec![]);
					if !actions.is_empty() {
						let id = self.send_request::<CodeActionRequest>(
							&client_id.client_name,
							url.clone(),
							CodeActionParams {
								text_document: TextDocumentIdentifier { uri: url },
								range: Range::new(Position::new(0, 0), Position::new(0, 0)),
								context: CodeActionContext {
									diagnostics: vec![],
									only: Some(actions),
								},
								work_done_progress_params: WorkDoneProgressParams {
									work_done_token: None,
								},
								partial_result_params: PartialResultParams {
									partial_result_token: None,
								},
							},
						)?;
						self.autorun.insert(id, ());
					}
				}
			}
			GotoImplementation::METHOD => {
				let msg = serde_json::from_str::<Option<GotoImplementationResponse>>(result.get())?;
				if let Some(msg) = msg {
					goto_definition(&msg)?;
				}
			}
			GotoTypeDefinition::METHOD => {
				let msg = serde_json::from_str::<Option<GotoTypeDefinitionResponse>>(result.get())?;
				if let Some(msg) = msg {
					goto_definition(&msg)?;
				}
			}
			SemanticTokensRangeRequest::METHOD => {
				let msg = serde_json::from_str::<Option<SemanticTokensRangeResult>>(result.get())?;
				if let Some(msg) = msg {
					match msg {
						SemanticTokensRangeResult::Tokens(tokens) => {
							// TODO: use the result_id and probably verify it with the send message?
							// Not sure why there would be more than 1 result, but we only need to care
							// about a single one anyway.
							if let Some(token) = tokens.data.into_iter().next() {
								self.set_hover(&url, |hover| {
									hover.token = Some(
										hover
											.line
											.chars()
											.skip(token.delta_start as usize)
											.take(token.length as usize)
											.collect(),
									);
								});
							}
						}
						_ => eprintln!("unsupported: {:#?}", msg),
					}
				}
			}
			_ => panic!("unrecognized type: {}", typ),
		}
		Ok(())
	}
	fn lsp_notification(
		&mut self,
		client_name: String,
		method: String,
		params: Option<Box<serde_json::value::RawValue>>,
	) -> Result<()> {
		match method.as_str() {
			LogMessage::METHOD => {
				let msg: LogMessageParams = serde_json::from_str(params.unwrap().get())?;
				self.output = format!("[{:?}] {}", msg.typ, msg.message);
			}
			PublishDiagnostics::METHOD => {
				let msg: PublishDiagnosticsParams = serde_json::from_str(params.unwrap().get())?;
				let mut v = vec![];
				let path = msg.uri.path();
				// Cap diagnostic length.
				for p in msg.diagnostics.iter().take(5) {
					let msg = p.message.lines().next().unwrap_or("");
					v.push(format!(
						"{}:{}: [{:?}] {}",
						path,
						p.range.start.line + 1,
						p.severity.unwrap_or(lsp_types::DiagnosticSeverity::Error),
						msg,
					));
				}
				self.diags.insert(path.to_string(), v);
			}
			ShowMessage::METHOD => {
				let msg: ShowMessageParams = serde_json::from_str(params.unwrap().get())?;
				self.output = format!("[{:?}] {}", msg.typ, msg.message);
			}
			Progress::METHOD => {
				let msg: ProgressParams = serde_json::from_str(params.unwrap().get())?;
				let name = format!("{}-{:?}", client_name, msg.token);
				match &msg.value {
					ProgressParamsValue::WorkDone(value) => match value {
						WorkDoneProgress::Begin(value) => {
							self.progress.insert(
								name.clone(),
								WDProgress::new(
									name,
									value.percentage,
									value.message.clone(),
									Some(value.title.clone()),
								),
							);
						}
						WorkDoneProgress::Report(value) => {
							let p = self.progress.get_mut(&name).unwrap();
							p.percentage = value.percentage;
							p.message = value.message.clone();
						}
						WorkDoneProgress::End(_) => {
							self.progress.remove(&name);
						}
					},
				}
			}
			_ => {
				eprintln!("unrecognized method: {}", method);
			}
		}
		Ok(())
	}
	fn lsp_request(&mut self, msg: lsp::DeMessage) -> Result<()> {
		eprintln!("unknown request {:?}", msg);
		Ok(())
	}
	fn apply_workspace_edit(&mut self, edit: &WorkspaceEdit) -> Result<()> {
		if let Some(ref doc_changes) = edit.document_changes {
			match doc_changes {
				DocumentChanges::Edits(edits) => {
					for edit in edits {
						let text_edits = edit
							.edits
							.iter()
							.filter_map(|e| {
								match e {
									// A TextEdit, keep it.
									OneOf::Left(e) => Some(e),
									// A AnnotatedTextEdit, discard until we support it.
									_ => None,
								}
							})
							.cloned()
							.collect();
						self.apply_text_edits(
							&edit.text_document.uri,
							InsertTextFormat::PlainText,
							&text_edits,
						)?;
					}
				}
				_ => panic!("unsupported document_changes {:?}", doc_changes),
			}
		}
		if let Some(ref changes) = edit.changes {
			for (url, edits) in changes {
				self.apply_text_edits(&url, InsertTextFormat::PlainText, &edits)?;
			}
		}
		Ok(())
	}
	fn apply_text_edits(
		&mut self,
		url: &Url,
		format: InsertTextFormat,
		edits: &Vec<TextEdit>,
	) -> Result<()> {
		if edits.is_empty() {
			return Ok(());
		}
		let sw = self.get_sw_by_url(url)?;
		let mut body = String::new();
		sw.w.read(File::Body)?.read_to_string(&mut body)?;
		let offsets = NlOffsets::new(std::io::Cursor::new(body.clone()))?;
		if edits.len() == 1 {
			if body == edits[0].new_text {
				return Ok(());
			}
			// Check if this is a full file replacement. If so, use a diff algorithm so acme doesn't scroll to the bottom.
			let edit = edits[0].clone();
			let last = offsets.last();
			if edit.range.start == Position::new(0, 0)
				&& edit.range.end == Position::new(last.0, last.1)
			{
				let lines = diff::lines(&body, &edit.new_text);
				let mut i = 0;
				for line in lines.iter() {
					i += 1;
					match line {
						diff::Result::Left(_) => {
							sw.w.addr(&format!("{},{}", i, i))?;
							sw.w.write(File::Data, "")?;
							i -= 1;
						}
						diff::Result::Right(s) => {
							sw.w.addr(&format!("{}+#0", i - 1))?;
							sw.w.write(File::Data, &format!("{}\n", s))?;
						}
						diff::Result::Both(_, _) => {}
					}
				}
				return Ok(());
			}
		}
		sw.w.seek(File::Body, std::io::SeekFrom::Start(0))?;
		sw.w.ctl("nomark")?;
		sw.w.ctl("mark")?;
		for edit in edits.iter().rev() {
			let soff = offsets.line_to_offset(edit.range.start.line, edit.range.start.character);
			let eoff = offsets.line_to_offset(edit.range.end.line, edit.range.end.character);
			let addr = format!("#{},#{}", soff, eoff);
			sw.w.addr(&addr)?;
			match format {
				InsertTextFormat::Snippet => {
					lazy_static! {
						static ref SNIPPET: Regex =
							Regex::new(r"(\$\{\d+:[[:alpha:]]+\})|(\$0)").unwrap();
					}
					let text = &SNIPPET.replace_all(&edit.new_text, "");
					sw.w.write(File::Data, text)?;
					text.len()
				}
				InsertTextFormat::PlainText => {
					sw.w.write(File::Data, &edit.new_text)?;
					edit.new_text.len()
				}
			};
		}
		Ok(())
	}
	fn did_change(&mut self, wid: usize) -> Result<()> {
		// Sometimes we are sending a DidChange before a DidOpen. Maybe this is because
		// acme's event log sometimes misses events. Sync the windows just to be sure.
		self.sync_windows()?;
		let sw = match self.ws.get_mut(&wid) {
			Some(sw) => sw,
			// Ignore untracked windows.
			None => return Ok(()),
		};
		let client = sw.client.clone();
		let params = sw.change_params()?;
		self.send_notification::<DidChangeTextDocument>(&client, params)
	}
	fn set_focus(&mut self, ev: LogEvent) -> Result<()> {
		self.focus = ev.name.clone();

		let sw = self.get_sw_by_name(&ev.name)?;
		let client_name = &sw.client.clone();
		let url = sw.url.clone();
		let wid = sw.w.id();
		let text_document_position_params = sw.text_doc_pos()?;
		let text_document = TextDocumentIdentifier::new(url.clone());
		let range = Range {
			start: text_document_position_params.position,
			end: text_document_position_params.position,
		};
		let line = sw.line()?;
		drop(sw);
		self.did_change(wid)?;
		self.current_hover = Some(WindowHover {
			client_name: client_name.into(),
			url: url.clone(),
			line,
			token: None,
			signature: None,
			lens: vec![],
			completion: vec![],
			code_actions: vec![],
			actions: vec![],
			action_addrs: vec![],
			hover: None,
			body: "".into(),
		});
		self.send_request::<HoverRequest>(
			&client_name,
			url.clone(),
			HoverParams {
				text_document_position_params: text_document_position_params.clone(),
				work_done_progress_params,
			},
		)?;
		self.send_request::<CodeActionRequest>(
			client_name,
			url.clone(),
			CodeActionParams {
				text_document: text_document.clone(),
				range,
				context: CodeActionContext {
					diagnostics: vec![],
					only: None,
				},
				work_done_progress_params,
				partial_result_params,
			},
		)?;
		self.send_request::<Completion>(
			&client_name,
			url.clone(),
			CompletionParams {
				text_document_position: text_document_position_params.clone(),
				work_done_progress_params,
				partial_result_params,
				context: Some(CompletionContext {
					trigger_kind: CompletionTriggerKind::Invoked,
					trigger_character: None,
				}),
			},
		)?;
		self.send_request::<SemanticTokensRangeRequest>(
			&client_name,
			url.clone(),
			SemanticTokensRangeParams {
				work_done_progress_params,
				partial_result_params,
				text_document: text_document.clone(),
				range,
			},
		)?;
		self.send_request::<SignatureHelpRequest>(
			client_name,
			url.clone(),
			SignatureHelpParams {
				context: None,
				text_document_position_params: text_document_position_params.clone(),
				work_done_progress_params,
			},
		)?;
		self.send_request::<CodeLensRequest>(
			client_name,
			url.clone(),
			CodeLensParams {
				text_document,
				work_done_progress_params,
				partial_result_params,
			},
		)?;
		Ok(())
	}
	fn run_event(&mut self, ev: Event, wid: usize) -> Result<()> {
		self.did_change(wid)?;
		let sw = self.ws.get_mut(&wid).unwrap();
		let client_name = &sw.client.clone();
		let url = sw.url.clone();
		let text_document_position_params = sw.text_doc_pos()?;
		let text_document_position = text_document_position_params.clone();
		let text_document = TextDocumentIdentifier::new(url.clone());
		drop(sw);
		match ev.text.as_str() {
			"definition" => {
				self.send_request::<GotoDefinition>(
					client_name,
					url,
					GotoDefinitionParams {
						text_document_position_params,
						work_done_progress_params,
						partial_result_params,
					},
				)?;
			}
			"references" => {
				self.send_request::<References>(
					client_name,
					url,
					ReferenceParams {
						text_document_position,
						work_done_progress_params,
						partial_result_params,
						context: ReferenceContext {
							include_declaration: true,
						},
					},
				)?;
			}
			"symbols" => {
				self.send_request::<DocumentSymbolRequest>(
					client_name,
					url,
					DocumentSymbolParams {
						text_document,
						work_done_progress_params,
						partial_result_params,
					},
				)?;
			}
			"impl" => {
				self.send_request::<GotoImplementation>(
					client_name,
					url,
					GotoImplementationParams {
						text_document_position_params,
						work_done_progress_params,
						partial_result_params,
					},
				)?;
			}
			"typedef" => {
				self.send_request::<GotoTypeDefinition>(
					client_name,
					url,
					GotoDefinitionParams {
						text_document_position_params,
						work_done_progress_params,
						partial_result_params,
					},
				)?;
			}
			_ => {}
		}
		Ok(())
	}
	fn send_request<R: Request>(
		&mut self,
		client_name: &str,
		url: Url,
		params: R::Params,
	) -> Result<usize> {
		let client = self.clients.get_mut(client_name).unwrap();
		let msg_id = client.send::<R>(params)?;
		self.requests
			.insert(ClientId::new(client_name, msg_id), (R::METHOD.into(), url));
		Ok(msg_id)
	}
	fn send_notification<N: notification::Notification>(
		&mut self,
		client_name: &String,
		params: N::Params,
	) -> Result<()> {
		let client = self.clients.get_mut(client_name).unwrap();
		client.notify::<N>(params)
	}
	fn run_action(&mut self, client_name: &str, url: Url, action: Action) -> Result<()> {
		match action {
			Action::Command(CodeActionOrCommand::Command(cmd)) => {
				if let Some(args) = cmd.arguments {
					for arg in args {
						#[derive(Deserialize)]
						#[serde(rename_all = "camelCase")]
						struct ArgWorkspaceEdit {
							workspace_edit: WorkspaceEdit,
						}
						match serde_json::from_value::<ArgWorkspaceEdit>(arg) {
							Ok(v) => self.apply_workspace_edit(&v.workspace_edit)?,
							Err(err) => {
								eprintln!("json err {}", err);
								continue;
							}
						}
					}
				}
			}
			Action::Command(CodeActionOrCommand::CodeAction(action)) => {
				if let Some(edit) = action.edit {
					self.apply_workspace_edit(&edit)?;
				} else {
					let _id = self.send_request::<CodeActionResolveRequest>(
						client_name.into(),
						url,
						action,
					)?;
				}
			}
			Action::Completion(item) => {
				let format = item
					.insert_text_format
					.unwrap_or(InsertTextFormat::PlainText);
				if let Some(edit) = item.text_edit.clone() {
					match edit {
						CompletionTextEdit::Edit(edit) => {
							return self.apply_text_edits(&url, format, &vec![edit])
						}
						CompletionTextEdit::InsertAndReplace(_) => {
							eprintln!("InsertAndReplace not supported");
							return Ok(());
						}
					}
				}
				panic!("unsupported");
			}
			Action::CodeLens(lens) => {
				// TODO: rust-analyzer complains about "code lens without data" here. If I set
				// lens.data to Value::Null, it complains with some resolution error.
				let _id = self.send_request::<CodeLensResolve>(client_name.into(), url, lens)?;
			}
		}
		Ok(())
	}
	fn run_cmd(&mut self, ev: Event) -> Result<()> {
		match ev.c2 {
			'x' | 'X' => match ev.text.as_str() {
				"Get" => {
					//self.actions.clear();
					self.output.clear();
					self.sync_windows()?;
					self.diags.clear();
					self.current_hover = None;
				}
				_ => {
					panic!("unexpected");
				}
			},
			'L' => {
				{
					let mut wid = 0;
					for (pos, id) in self.addr.iter().rev() {
						if (*pos as u32) < ev.q0 {
							wid = *id;
							break;
						}
					}
					if wid != 0 {
						return self.run_event(ev, wid);
					}
				}
				{
					let mut action: Option<(String, Url, Action)> = None;
					if let Some(hover) = self.current_hover.as_mut() {
						let mut action_idx: Option<usize> = None;
						for (pos, idx) in hover.action_addrs.iter().rev() {
							if (*pos as u32) < ev.q0 {
								action_idx = Some(*idx);
								break;
							}
						}
						if let Some(idx) = action_idx {
							action = Some((
								hover.client_name.to_string(),
								hover.url.clone(),
								hover.actions.remove(idx),
							));
						}
					}
					if let Some((client_name, url, action)) = action {
						self.set_hover(&url, |hover| {
							hover.code_actions.clear();
							hover.completion.clear();
						});
						return self.run_action(&client_name, url, action);
					}
				}
				return plumb_location(ev.text);
			}
			_ => {}
		}
		Ok(())
	}
	fn cmd_put(&mut self, id: usize) -> Result<()> {
		self.did_change(id)?;
		let sw = if let Some(sw) = self.ws.get(&id) {
			sw
		} else {
			// Ignore unknown ids (untracked files, zerox, etc.).
			return Ok(());
		};
		let client_name = &sw.client.clone();
		let text_document = sw.doc_ident();
		let url = sw.url.clone();
		drop(sw);
		self.send_notification::<DidSaveTextDocument>(
			client_name,
			DidSaveTextDocumentParams {
				text_document: text_document.clone(),
				text: None,
			},
		)?;
		let capabilities = self.capabilities.get(client_name).unwrap();
		if self
			.config
			.servers
			.get(client_name)
			.unwrap()
			.format_on_put
			.unwrap_or(true)
			&& capabilities.document_formatting_provider.is_some()
		{
			self.send_request::<Formatting>(
				client_name,
				url,
				DocumentFormattingParams {
					text_document,
					options: FormattingOptions {
						tab_size: 4,
						insert_spaces: false,
						properties: HashMap::new(),
						trim_trailing_whitespace: Some(true),
						insert_final_newline: Some(true),
						trim_final_newlines: Some(true),
					},
					work_done_progress_params: WorkDoneProgressParams {
						work_done_token: None,
					},
				},
			)?;
		}
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		let (sync_s, sync_r) = bounded(1);

		self.sync_windows()?;
		// chan index -> (recv chan, self.clients index)

		// one-time index setup
		let mut sel = Select::new();
		let sel_log_r = sel.recv(&self.log_r);
		let sel_ev_r = sel.recv(&self.ev_r);
		let sel_err_r = sel.recv(&self.err_r);
		let sel_sync_r = sel.recv(&sync_r);
		let mut clients = HashMap::new();

		for (name, c) in &self.clients {
			clients.insert(sel.recv(&c.msg_r), (c.msg_r.clone(), name.to_string()));
		}
		drop(sel);

		loop {
			let mut no_sync = false;

			let mut sel = Select::new();
			sel.recv(&self.log_r);
			sel.recv(&self.ev_r);
			sel.recv(&self.err_r);
			sel.recv(&sync_r);
			for (_, c) in &self.clients {
				sel.recv(&c.msg_r);
			}
			let index = sel.ready();

			match index {
				_ if index == sel_log_r => {
					let msg = self.log_r.recv();
					match msg {
						Ok(ev) => match ev.op.as_str() {
							"focus" => {
								let _ = self.set_focus(ev);
							}
							"put" => {
								self.cmd_put(ev.id)?;
								no_sync = true;
							}
							"new" | "del" => {
								self.sync_windows()?;
							}
							_ => {
								panic!("unknown event op {:?}", ev);
							}
						},
						Err(_) => {
							break;
						}
					}
				}
				_ if index == sel_ev_r => {
					let msg = self.ev_r.recv();
					match msg {
						Ok(ev) => {
							self.run_cmd(ev)?;
						}
						Err(_) => {
							break;
						}
					}
				}
				_ if index == sel_err_r => {
					let msg = self.err_r.recv();
					eprintln!("err {:?}", msg);
					match msg {
						Ok(_) => {
							break;
						}
						Err(_) => {
							break;
						}
					}
				}
				_ if index == sel_sync_r => {
					no_sync = true;
					let _ = sync_r.recv();
					self.sync()?;
				}
				_ => {
					let (ch, name) = clients.get(&index).unwrap();
					let msg = ch.recv()?;
					self.lsp_msg(name.to_string(), msg)?;
				}
			};

			// Only send a sync message if the channel is empty. If a bunch of LSP messages
			// arrive (like window progress updatets), they don't each have to wait for a
			// full sync before beingc processed.
			if !no_sync && sync_s.is_empty() {
				sync_s.send(())?;
			}
		}
		Ok(())
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.w.del(true);
	}
}

fn goto_definition(goto: &GotoDefinitionResponse) -> Result<()> {
	match goto {
		GotoDefinitionResponse::Array(locs) => match locs.len() {
			0 => {}
			_ => {
				let plumb = location_to_plumb(&locs[0]);
				plumb_location(plumb)?;
			}
		},
		_ => panic!("unknown definition response: {:?}", goto),
	};
	Ok(())
}

fn location_to_plumb(l: &Location) -> String {
	format!("{}:{}", l.uri.path(), l.range.start.line + 1,)
}

fn plumb_location(loc: String) -> Result<()> {
	let path = loc.split(":").next().unwrap();
	// Verify path exists. If not, do nothing.
	if metadata(path).is_err() {
		return Ok(());
	}
	let f = plumb::open("send", OpenMode::WRITE)?;
	let msg = plumb::Message {
		dst: "edit".to_string(),
		typ: "text".to_string(),
		data: loc.into(),
	};
	return msg.send(f);
}

fn format_pct(pct: Option<u32>) -> String {
	match pct {
		Some(v) => format!("{}", v),
		None => "?".to_string(),
	}
}

fn cmp_location(a: &Location, b: &Location) -> Ordering {
	if a.uri != b.uri {
		return a.uri.as_str().cmp(b.uri.as_str());
	}
	return cmp_range(&a.range, &b.range);
}

fn cmp_range(a: &Range, b: &Range) -> Ordering {
	if a.start != b.start {
		return cmp_position(&a.start, &b.start);
	}
	return cmp_position(&a.end, &b.end);
}

fn cmp_position(a: &Position, b: &Position) -> Ordering {
	if a.line != b.line {
		return a.line.cmp(&b.line);
	}
	return a.character.cmp(&b.character);
}

fn extract_doc(d: &Documentation) -> &str {
	match d {
		Documentation::String(s) => s,
		Documentation::MarkupContent(c) => &c.value,
	}
}

#[allow(non_upper_case_globals)]
const work_done_progress_params: WorkDoneProgressParams = WorkDoneProgressParams {
	work_done_token: None,
};
#[allow(non_upper_case_globals)]
const partial_result_params: PartialResultParams = PartialResultParams {
	partial_result_token: None,
};
