# acre

acre is a [langserver](https://langserver.org/) client for [acme](https://www.youtube.com/watch?v=dP1xVpMPn8M) in [Rust](https://www.rust-lang.org/).

This is very much in **beta** and purposefully crashes on most errors. If a crash occurs, please file a bug so the feature can be added. Code actions, lenses, and some other features are not yet supported. Config files may change.

It functions by creating a new window in acme. The window lists all open supported files and commands. The commands can be run by right clicking on them. The currently focused window is prefixed by a `*`. Run the `Get` command in the acre window to clear the current output.

Note: while the open file list contains all supported file types, those files may or may not be supported by the server if, say, the project they are in has not been configured in acre.toml.

# Demo

![demo](https://user-images.githubusercontent.com/41181/79060721-afaa9080-7c45-11ea-92be-12846b108cf7.gif)

# Installation

The [latest release](https://github.com/mjibson/acre/releases/latest) is available for Linux and OSX.

# Configuration

Configuration (which servers to run) is handled by a file at `~/.config/acre.toml` (note: I'm not sure if this is true on OSX, but the location will be printed in an error if it does not exist). The file should contain an array of `servers` objects with the fields:

- `name`: the name of the server.
- `executable` (optional): the name of the binary to invoke. If not present, uses `name`.
- `files`: regex matching files that should be associated with this server.
- `root_uri` (optional): Root URI of the workspace.
- `workspace_folders` (optional): array of workspace folder URIs.
- `options'` (optional): list of options to be sent to the server.

URIs should look something like `file:///home/user/project`.

Here's an example file for `rust-analyzer` and `gopls`:

```
[[servers]]
name = "rust-analyzer"
files = "\\.rs$"
workspace_folders = [
	"file:///home/username/some-project",
	"file:///home/username/other-project",
]

[[servers]]
name = "gopls"
files = '\.go$'
root_uri = "file:///home/username/go-project"
```

This will execute the `rust-analyzer-linux` binary and associate it with all files ending in `.rs`. Two workspaces are configured. `gopls` will run on a single root for `.go` files.

Options to pass to each server can be added:

```
[[servers]]
name = "some-server"
files = '\.ext$'
root_uri = "file:///home/username/project"
[servers.options]
enableSomething = true
hoverMode = "OneLine"
```

# Tested servers

The following is a list of servers that have been tested with acre and are expected to work.

- [rust-analyzer](https://rust-analyzer.github.io/)
- [gopls](https://github.com/golang/tools/blob/master/gopls/README.md)

# Other clients

- [acme-lsp](https://github.com/fhs/acme-lsp) is another client for acme. It functions using Win commands instead of the new window method that acre uses.
