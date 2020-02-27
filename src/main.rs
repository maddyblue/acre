use crossbeam_channel::{bounded, Receiver, Select, Sender};
use nine::p2000::OpenMode;
use plan9::{acme::*, lsp, plumb};
use serde_json;
use std::collections::HashMap;
use std::fmt::Write;
use std::process::Command;
use std::thread;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

fn main() -> Result<()> {
	let rust_client = lsp::Client::new(
		"rls".to_string(),
		".rs".to_string(),
		"rls",
		std::iter::empty(),
		"file:///home/mjibson/go/src/github.com/mjibson/plan9",
		None,
	)
	.unwrap();
	let mut s = Server::new(vec![rust_client])?;
	s.wait()
}

struct Server {
	w: Win,
	ws: HashMap<usize, ServerWin>,
	// Sorted Vec of (names, win id) to know which order to print windows in.
	names: Vec<(String, usize)>,
	// Vec of (position, win id) to map Look locations to windows.
	addr: Vec<(usize, usize)>,

	body: String,
	output: Vec<String>,
	focus: String,
	progress: HashMap<String, String>,
	// file name -> list of diagnostics
	diags: HashMap<String, Vec<String>>,

	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	guru_r: Receiver<String>,
	guru_s: Sender<String>,
	err_r: Receiver<Error>,
	clients: Vec<lsp::Client>,
}

struct ServerWin {
	name: String,
	w: Win,
}

impl ServerWin {
	fn pos(&mut self) -> Result<(usize, usize)> {
		self.w.ctl("addr=dot")?;
		// TODO: convert these character (rune) offsets to byte offsets.
		self.w.read_addr()
	}
}

impl Server {
	fn new(clients: Vec<lsp::Client>) -> Result<Server> {
		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let (guru_s, guru_r) = bounded(0);
		let mut w = Win::new()?;
		w.name("acre")?;
		let mut wev = w.events()?;
		let s = Server {
			w,
			ws: HashMap::new(),
			names: vec![],
			addr: vec![],
			output: vec![],
			body: "".to_string(),
			focus: "".to_string(),
			progress: HashMap::new(),
			diags: HashMap::new(),
			log_r,
			ev_r,
			guru_r,
			guru_s,
			err_r,
			clients,
		};
		let err_s1 = err_s.clone();
		thread::Builder::new()
			.name("LogReader".to_string())
			.spawn(move || {
				let mut log = LogReader::new().unwrap();
				loop {
					match log.read() {
						Ok(ev) => match ev.op.as_str() {
							"new" | "del" | "focus" => {
								//println!("sending log event: {:?}", ev);
								log_s.send(ev).unwrap();
							}
							_ => {
								//println!("log event: {:?}", ev);
							}
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
				let mut ev = wev.read_event().unwrap();
				//println!("window event: {:?}", ev);
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
	fn sync(&mut self) -> Result<()> {
		let mut body = String::new();
		for (_, p) in &self.progress {
			write!(&mut body, "{}\n", p)?;
		}
		if self.progress.len() > 0 {
			body.push('\n');
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
		for (name, id) in &self.names {
			self.addr.push((body.len(), *id));
			write!(
				&mut body,
				"{}{}\n\t[definition] [describe] [referrers]\n",
				if *name == self.focus { "*" } else { "" },
				name
			)?;
		}
		self.addr.push((body.len(), 0));
		write!(&mut body, "-----\n")?;
		for s in &self.output {
			write!(&mut body, "\n{}\n", s)?;
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
		wins.sort_by(|a, b| a.name.cmp(&b.name));
		for wi in wins {
			if !wi.name.ends_with(".go") {
				continue;
			}
			self.names.push((wi.name.clone(), wi.id));
			let w = match self.ws.remove(&wi.id) {
				Some(w) => w,
				None => {
					let mut fsys = FSYS.lock().unwrap();
					let ctl = fsys.open(format!("{}/ctl", wi.id).as_str(), OpenMode::RDWR)?;
					let w = Win::open(&mut fsys, wi.id, ctl)?;
					ServerWin { name: wi.name, w }
				}
			};
			ws.insert(wi.id, w);
		}
		self.ws = ws;
		Ok(())
	}
	fn lsp_msg(&mut self, client_index: usize, msg: lsp::DeMessage) -> Result<()> {
		let client = &self.clients[client_index];
		if let Some(method) = msg.method {
			match method {
				"window/progress" => {
					let d: WindowProgress = serde_json::from_str(msg.params.unwrap().get())?;
					let name = format!("{}-{}", client.name, d.id);
					if d.done.unwrap_or(false) {
						self.progress.remove(&name);
					} else {
						let pct: String = match d.percentage {
							Some(v) => v.to_string(),
							None => "?".to_string(),
						};
						let s = format!(
							"[{}%] {}: {} ({})",
							pct,
							&name,
							d.message.unwrap_or(""),
							d.title.unwrap_or(""),
						);
						self.progress.insert(name, s);
					}
				}
				"textDocument/publishDiagnostics" => {
					let dp: lsp_types::PublishDiagnosticsParams =
						serde_json::from_str(msg.params.unwrap().get())?;
					let mut v = vec![];
					let path = dp.uri.path();
					for p in dp.diagnostics {
						let msg = p.message.lines().next().unwrap_or("");
						v.push(format!(
							"{}:{}: [{:?}] {}",
							path,
							p.range.start.line,
							p.severity.unwrap_or(lsp_types::DiagnosticSeverity::Error),
							msg,
						));
					}
					self.diags.insert(path.to_string(), v);
				}
				_ => {
					println!("unrecognized method: {:?}", msg);
				}
			}
		} else {
			println!("unhandled lsp msg: {:?}", msg);
		}
		Ok(())
	}
	fn run_cmd(&mut self, ev: Event) -> Result<()> {
		match ev.c2 {
			'x' | 'X' => match ev.text.as_str() {
				"Get" => {
					self.sync_windows()?;
				}
				_ => {
					panic!("unexpected");
				}
			},
			'L' => {
				let mut wid: usize = 0;
				for (pos, id) in self.addr.iter().rev() {
					if (*pos as u32) < ev.q0 {
						wid = *id;
						break;
					}
				}
				if wid == 0 {
					let f = plumb::open("send", OpenMode::WRITE)?;
					let msg = plumb::Message {
						dst: "edit".to_string(),
						typ: "text".to_string(),
						data: ev.text.into(),
					};
					return msg.send(f);
				}
				let sw = self.ws.get_mut(&wid).unwrap();
				let addr = sw.pos()?;
				let pos = format!("{}:#{}", sw.name, addr.0);
				let guru_s = self.guru_s.clone();
				thread::spawn(move || {
					let res = || -> Result<String> {
						let res = Command::new("guru").arg(ev.text).arg(&pos).output()?;
						let mut out = std::str::from_utf8(&res.stdout)?.trim().to_string();
						if out.len() == 0 {
							out = format!("{}: {}", pos, std::str::from_utf8(&res.stderr)?.trim());
						}
						Ok(out)
					}();
					let out = match res {
						Ok(s) => s,
						Err(e) => format!("{}", e),
					};
					guru_s.send(out).unwrap();
				});
			}
			_ => {}
		}
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		self.sync_windows()?;
		// chan index -> (recv chan, self.clients index)
		let mut clients: HashMap<usize, (Receiver<Vec<u8>>, usize)> = HashMap::new();
		loop {
			self.sync()?;
			let mut sel = Select::new();
			let sel_log_r = sel.recv(&self.log_r);
			let sel_ev_r = sel.recv(&self.ev_r);
			let sel_guru_r = sel.recv(&self.guru_r);
			let sel_err_r = sel.recv(&self.err_r);
			clients.clear();
			// TODO: this is probably needlessly duplicative due to
			// cloning the recv chans each event.
			for (i, c) in self.clients.iter().enumerate() {
				clients.insert(sel.recv(&c.msg_r), (c.msg_r.clone(), i));
			}
			let index = sel.ready();

			match index {
				_ if index == sel_log_r => match self.log_r.recv() {
					Ok(ev) => match ev.op.as_str() {
						"focus" => {
							self.focus = ev.name;
						}
						_ => {
							self.sync_windows()?;
						}
					},
					Err(_) => {
						println!("log_r closed");
						break;
					}
				},
				_ if index == sel_ev_r => match self.ev_r.recv() {
					Ok(ev) => {
						self.run_cmd(ev)?;
					}
					Err(_) => {
						println!("ev_r closed");
						break;
					}
				},
				_ if index == sel_guru_r => match self.guru_r.recv() {
					Ok(s) => {
						self.output.insert(0, s);
						if self.output.len() > 5 {
							self.output.drain(5..);
						}
					}
					Err(_) => {
						println!("guru_r closed");
						break;
					}
				},
				_ if index == sel_err_r => match self.err_r.recv() {
					Ok(v) => {
						println!("err: {}", v);
						break;
					}
					Err(_) => {
						println!("err_r closed");
						break;
					}
				},
				_ => {
					let (ch, i) = clients.get(&index).unwrap();
					let msg = ch.recv()?;
					let d: lsp::DeMessage = serde_json::from_slice(&msg)?;
					self.lsp_msg(*i, d)?;
				}
			};
		}
		println!("wait returning");
		Ok(())
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.w.del(true);
	}
}

#[derive(Debug, serde::Deserialize)]
struct WindowProgress<'a> {
	done: Option<bool>,
	id: &'a str,
	message: Option<&'a str>,
	title: Option<&'a str>,
	percentage: Option<f64>,
}
