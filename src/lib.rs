pub mod conn;
pub mod dial;
pub mod fid;
pub mod fsys;

#[cfg(test)]
#[allow(unused_variables)]
mod tests {
	extern crate nine;

	use crate::dial;
	use nine::p2000::OpenMode;
	use std::io;

	#[test]
	fn it_works() {
		let mut fsys = dial::mount_service("acme").unwrap();
		let mut index = fsys.open("index", OpenMode::READ).unwrap();
		io::copy(&mut index, &mut io::stdout()).unwrap();
	}
}
