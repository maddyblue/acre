use crate::dial;
use crate::{fsys::Fsys, Result};
use nine::p2000::OpenMode;
use std::io::{BufRead, BufReader};

fn mount() -> Result<Fsys> {
	dial::mount_service("acme")
}

pub fn windows() -> Result<Vec<WinInfo>> {
	let mut fsys = mount()?;
	let index = fsys.open("index", OpenMode::READ)?;
	let r = BufReader::new(index);
	let mut ws = Vec::new();
	for line in r.lines() {
		if let Ok(line) = line {
			let sp: Vec<&str> = line.split_whitespace().collect();
			if sp.len() < 6 {
				continue;
			}
			ws.push(WinInfo {
				id: sp[0].parse()?,
				name: sp[5].to_string(),
			});
		}
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
