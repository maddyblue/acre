# acre

acre is a [langserver](https://langserver.org/) client for [acme](https://www.youtube.com/watch?v=dP1xVpMPn8M) in [Rust](https://www.rust-lang.org/).

This is very much in **beta** and purposefully crashes on most errors. Code actions, lenses, and some other features are not yet supported.

It functions by creating a new window in acre. The window lists all open supported files and commands. The commands can be run by right clicking on them. The currently focused window is prefixed by a `*`. Run the `Get` command in the acre window to clear the current output.

Note: while the open file list contains all supported file types, those files may or may not be supported by the server if, say, the project they are in has not been configured in acre.toml.

# Installation

The [latest release](https://github.com/mjibson/acre/releases/latest) is available for Linux and OSX.

# Configuration

Configuration (which servers to run) is handled by a file at `~/.config/acre.toml` (note: I'm not sure if this is true on OSX). Here's an example file for `rust-analyzer`:

```
[[servers]]
name = "rls"
executable = "rust-analyzer-linux"
extension = ".rs"
workspace_folders = [
	"file:///home/username/some-project",
	"file:///home/username/other-project",
]
```

This will execute the `rust-analyzer-linux` binary and associate it with all files ending in `.rs`. Two workspaces are configured. Add more `[[servers]]` blocks for others.

# Tested servers

The following is a list of servers that have been tested with acre and are expected to work.

- [rust-analyzer](https://rust-analyzer.github.io/)
- [gopls](https://github.com/golang/tools/blob/master/gopls/README.md)

# Other clients

- [acme-lsp](https://github.com/fhs/acme-lsp) is another client for acme. It functions using Win commands instead of the new window method that acre uses.
