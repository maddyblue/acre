#[macro_use]
extern crate crossbeam_channel;

use crossbeam_channel::{bounded, Receiver};
use nine::p2000::OpenMode;
use plan9::acme::*;
use plan9::plumb;
use std::collections::HashMap;
use std::fmt::Write;
use std::process::Command;
use std::thread;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

fn main() -> Result<()> {
	let mut s = Server::new()?;
	s.wait()
}

struct Server {
	w: Win,
	ws: HashMap<usize, ServerWin>,
	// Sorted Vec of (names, win id) to know which order to print windows in.
	names: Vec<(String, usize)>,
	// Vec of (position, win id) to map Look locations to windows.
	addr: Vec<(usize, usize)>,

	output: Vec<String>,

	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	err_r: Receiver<Error>,
}

struct ServerWin {
	name: String,
	id: usize,
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
	fn new() -> Result<Server> {
		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let mut w = Win::new()?;
		w.name("acre")?;
		let mut wev = w.events()?;
		let s = Server {
			w,
			ws: HashMap::new(),
			names: vec![],
			addr: vec![],
			output: vec![],
			log_r,
			ev_r,
			err_r,
		};
		let err_s1 = err_s.clone();
		thread::Builder::new()
			.name("LogReader".to_string())
			.spawn(move || {
				let mut log = LogReader::new().unwrap();
				loop {
					match log.read() {
						Ok(ev) => match ev.op.as_str() {
							"new" | "del" => {
								println!("sending log event: {:?}", ev);
								log_s.send(ev).unwrap();
							}
							_ => {
								println!("log event: {:?}", ev);
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
				println!("window event: {:?}", ev);
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
		self.addr.clear();
		for (name, id) in &self.names {
			self.addr.push((body.len(), *id));
			write!(
				&mut body,
				"{}\n\t[definition] [describe] [referrers]\n",
				name
			)?;
		}
		self.addr.push((body.len(), 0));
		write!(&mut body, "-----\n")?;
		for s in &self.output {
			write!(&mut body, "\n{}\n", s)?;
		}
		self.w.clear()?;
		self.w.write(File::Body, &body)?;
		self.w.ctl("cleartag\nclean")?;
		self.w.write(File::Tag, " Get")?;
		Ok(())
	}
	fn sync_windows(&mut self) -> Result<()> {
		println!("sync windows");
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
					ServerWin {
						name: wi.name,
						id: wi.id,
						w,
					}
				}
			};
			ws.insert(wi.id, w);
		}
		self.ws = ws;
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
				let res = Command::new("guru").arg(ev.text).arg(&pos).output()?;
				let mut out = std::str::from_utf8(&res.stdout)?.trim().to_string();
				if out.len() == 0 {
					out = format!("{}: {}", pos, std::str::from_utf8(&res.stderr)?.trim());
				}
				self.output.insert(0, out);
				if self.output.len() > 5 {
					self.output.drain(5..);
				}
			}
			_ => {}
		}
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		self.sync_windows()?;
		loop {
			self.sync()?;
			select! {
				recv(self.log_r) -> msg => {
					match msg {
						Ok(_) => { self.sync_windows()?;},
						Err(_) => { println!("log_r closed"); break;},
					};
				},
				recv(self.ev_r) -> msg => {
					match msg {
						Ok(ev) => { self.run_cmd(ev)?; },
						Err(_) => { println!("ev_r closed"); break;},
					};
				},
				recv(self.err_r) -> msg => {
					match msg {
						Ok(v) => { println!("err: {}", v); break;},
						Err(_) => { println!("err_r closed"); break;},
					};
				},
			}
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
