use crate::dial;
use crate::fsys::Fsys;
use nine::p2000::OpenMode;
use std::io::{BufRead, BufReader};

fn mount() -> Result<Fsys, String> {
	dial::mount_service("acme")
}

pub fn windows() -> Result<Vec<WinInfo>, String> {
	let mut fsys = mount()?;
	let index = fsys.open("index", OpenMode::READ)?;
	let r = BufReader::new(index);
	let mut ws = Vec::new();
	for line in r.lines() {
		let line = match line {
			Ok(v) => v,
			Err(e) => return Err(e.to_string()),
		};
		let sp: Vec<&str> = line.split_whitespace().collect();
		if sp.len() < 6 {
			continue;
		}
		let id: usize = match sp[0].parse() {
			Ok(v) => v,
			Err(e) => return Err(e.to_string()),
		};
		ws.push(WinInfo {
			id,
			name: sp[5].to_string(),
		});
	}
	Ok(ws)
}

#[derive(Debug)]
pub struct WinInfo {
	id: usize,
	name: String,
}

#[cfg(test)]
mod tests {
	use crate::acme;

	#[test]
	fn windows() {
		let ws = acme::windows().unwrap();
		assert_ne!(ws.len(), 0);
		println!("ws: {:?}", ws);
	}
}
