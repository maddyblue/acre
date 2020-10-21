use std::cmp::Ordering;
use std::collections::HashMap;
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

use plan9::{acme::*, lsp, plumb};

#[derive(Deserialize)]
struct TomlConfig {
    servers: HashMap<String, ConfigServer>,
}

#[derive(Clone, Deserialize)]
struct ConfigServer {
    executable: Option<String>,
    files: String,
    root_uri: Option<String>,
    workspace_folders: Option<Vec<String>>,
    options: Option<Value>,
    actions_on_put: Option<Vec<CodeActionKind>>,
    format_on_put: Option<bool>,
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
        println!("empty servers in configuration file");
        std::process::exit(1);
    }
    let mut s = Server::new(config)?;
    s.wait()
}

struct WDProgress {
    name: String,
    percentage: Option<f64>,
    message: Option<String>,
    title: String,
}

impl WDProgress {
    fn new(
        name: String,
        percentage: Option<f64>,
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
    Completion(Url, CompletionItem),
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
    // Sorted Vec of (filenames, win id) to know which order to print windows in.
    names: Vec<(String, usize)>,
    // Vec of (position, win id) to map Look locations to windows.
    addr: Vec<(usize, usize)>,

    body: String,
    output: String,
    focus: String,
    progress: HashMap<String, WDProgress>,
    // file name -> list of diagnostics
    diags: HashMap<String, Vec<String>>,
    // request (client_name, id) -> (method, file Url)
    requests: HashMap<ClientId, (String, Url)>,
    actions: HashMap<ClientId, Vec<Action>>,
    // Vec of position and (ClientId, index) into the vec of actions.
    action_addrs: Vec<(usize, (ClientId, usize))>,

    log_r: Receiver<LogEvent>,
    ev_r: Receiver<Event>,
    err_r: Receiver<Error>,

    // client name -> client
    clients: HashMap<String, lsp::Client>,
    // client name -> capabilities
    capabilities: HashMap<String, lsp_types::ServerCapabilities>,
    // file name -> client name
    files: HashMap<String, String>,
    // list of LSP message IDs to auto-run actions
    autorun: HashMap<usize, ()>,
}

struct ServerWin {
    w: Win,
    doc: TextDocumentIdentifier,
    url: Url,
    version: i64,
    client: String,
}

impl ServerWin {
    fn new(name: String, w: Win, client: String) -> Result<ServerWin> {
        let url = Url::parse(&format!("file://{}", name))?;
        let doc = TextDocumentIdentifier::new(url.clone());
        Ok(ServerWin {
            w,
            doc,
            url,
            version: 1,
            client,
        })
    }
    fn pos(&mut self) -> Result<(usize, usize)> {
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
        let (line, col) = nl.offset_to_line(pos.0 as u64);
        Ok(Position::new(line, col))
    }
    fn text(&mut self) -> Result<(i64, String)> {
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
    fn text_doc_pos(&mut self) -> Result<TextDocumentPositionParams> {
        let pos = self.position()?;
        Ok(TextDocumentPositionParams::new(self.doc.clone(), pos))
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
                std::iter::empty(),
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
            addr: vec![],
            output: "".to_string(),
            body: "".to_string(),
            focus: "".to_string(),
            progress: HashMap::new(),
            requests,
            actions: HashMap::new(),
            action_addrs: vec![],
            diags: HashMap::new(),
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
                            "new" | "del" | "focus" | "put" => {
                                if cfg!(debug_assertions) {
                                    println!("log reader: {:?}", ev);
                                }
                                match log_s.send(ev) {
                                    Ok(_) => {}
                                    Err(err) => {
                                        println!("log_s send err {}", err);
                                        return;
                                    }
                                }
                            }
                            _ => {
                                if cfg!(debug_assertions) {
                                    println!("log reader: {:?} [uncaught]", ev);
                                }
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
                let mut ev = match wev.read_event() {
                    Ok(ev) => ev,
                    Err(err) => {
                        println!("read event err {}", err);
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
    fn get_sw_by_url(&mut self, url: &Url) -> Result<&mut ServerWin> {
        let filename = url.path();
        let mut wid: Option<usize> = None;
        for (name, id) in &self.names {
            if filename == name {
                wid = Some(*id);
                break;
            }
        }
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
    fn sync(&mut self) -> Result<()> {
        let mut body = String::new();
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
                if *file_name == self.focus { "*" } else { "" },
                file_name
            )?;
            let client_name = self.files.get(file_name).unwrap();
            let caps = match self.capabilities.get(client_name) {
                Some(v) => v,
                None => continue,
            };
            if caps.code_action_provider.is_some() {
                body.push_str("[assist] ");
            }
            if caps.completion_provider.is_some() {
                body.push_str("[complete] ");
            }
            if caps.definition_provider.is_some() {
                body.push_str("[definition] ");
            }
            if caps.hover_provider.is_some() {
                body.push_str("[hover] ");
            }
            if caps.implementation_provider.is_some() {
                body.push_str("[impl] ");
            }
            #[cfg(debug_assertions)]
            {
                if caps.code_lens_provider.is_some() {
                    body.push_str("[lens] ");
                }
            }
            if caps.references_provider.is_some() {
                body.push_str("[references] ");
            }
            if caps.document_symbol_provider.is_some() {
                body.push_str("[symbols] ");
            }
            if caps.signature_help_provider.is_some() {
                body.push_str("[signature] ");
            }
            if caps.type_definition_provider.is_some() {
                body.push_str("[typedef] ");
            }
            body.push('\n');
        }
        self.addr.push((body.len(), 0));
        write!(&mut body, "-----\n")?;
        self.action_addrs.clear();
        for (client_id, actions) in &self.actions {
            for (idx, action) in actions.iter().enumerate() {
                self.action_addrs
                    .push((body.len(), (client_id.clone(), idx)));
                match action {
                    Action::Command(CodeActionOrCommand::Command(cmd)) => {
                        write!(&mut body, "\n[{}]", cmd.title)?;
                    }
                    Action::Command(CodeActionOrCommand::CodeAction(action)) => {
                        write!(&mut body, "\n[{}]", action.title)?;
                    }
                    Action::Completion(_, item) => {
                        write!(&mut body, "\n[insert] {}:", item.label)?;
                        if item.deprecated.unwrap_or(false) {
                            write!(&mut body, " DEPRECATED")?;
                        }
                        if let Some(k) = item.kind {
                            write!(&mut body, " ({:?})", k)?;
                        }
                        if let Some(d) = &item.detail {
                            write!(&mut body, " {}", d)?;
                        }
                    }
                }
            }
            write!(&mut body, "\n")?;
        }
        self.action_addrs
            .push((body.len(), (ClientId::new("", 0), 100000)));
        if !self.output.is_empty() {
            write!(&mut body, "\n{}\n", self.output)?;
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
        wins.sort_by(|a, b| a.name.cmp(&b.name));
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

                    sw
                }
            };
            ws.insert(wi.id, w);
        }
        // close remaining files
        let to_close: Vec<(String, TextDocumentIdentifier)> = self
            .ws
            .iter()
            .map(|(_, w)| (w.client.clone(), w.doc.clone()))
            .collect();
        for (client_name, text_document) in to_close {
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
            self.lsp_response(ClientId::new(client_name, msg.id.unwrap()), msg.result)
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
        result: Option<Box<serde_json::value::RawValue>>,
    ) -> Result<()> {
        let (typ, url) = self
            .requests
            .remove(&client_id)
            .expect(&format!("expected client id {:?}", client_id));
        let result = match result {
            Some(v) => v,
            None => {
                self.output = "null".into();
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
                    .insert(client_id.client_name, msg.capabilities.clone());
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
                            self.output = o.join("\n");
                        }
                        HoverContents::Markup(mc) => {
                            self.output = mc.value.clone();
                        }
                        _ => panic!("unknown hover response: {:?}", msg),
                    };
                }
            }
            Completion::METHOD => {
                let msg = serde_json::from_str::<Option<CompletionResponse>>(result.get())?;
                if let Some(msg) = msg {
                    self.actions.clear();
                    let actions = match msg {
                        CompletionResponse::Array(cis) => cis,
                        CompletionResponse::List(cls) => cls.items,
                    };
                    let mut v = vec![];
                    for a in actions.iter().cloned() {
                        v.push(Action::Completion(url.clone(), a));
                    }
                    v.truncate(10);
                    self.actions.insert(client_id, v);
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
                    };
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
                            };
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
                    let mut o: Vec<String> = vec![];
                    for sig in &msg.signatures {
                        o.push(sig.label.clone());
                    }
                    if o.len() > 0 {
                        self.output = o.join("\n");
                    }
                }
            }
            CodeLensRequest::METHOD => {
                let msg = serde_json::from_str::<Option<Vec<CodeLens>>>(result.get())?;
                if let Some(msg) = msg {
                    let mut o: Vec<String> = vec![];
                    for lens in msg {
                        let loc = Location {
                            uri: url.clone(),
                            range: lens.range,
                        };
                        o.push(format!("{}", location_to_plumb(&loc)));
                    }
                    if o.len() > 0 {
                        self.output = o.join("\n");
                    }
                }
            }
            CodeActionRequest::METHOD => {
                let msg = serde_json::from_str::<Option<CodeActionResponse>>(result.get())?;
                if let Some(msg) = msg {
                    if self.autorun.remove_entry(&client_id.msg_id).is_some() {
                        for m in msg.iter().cloned() {
                            self.run_action(Action::Command(m))?;
                        }
                    } else {
                        self.actions.clear();
                        let mut v = vec![];
                        for m in msg.iter().cloned() {
                            v.push(Action::Command(m));
                        }
                        self.actions.insert(client_id, v);
                    }
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
                for p in &msg.diagnostics {
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
                panic!("unrecognized method: {}", method);
            }
        }
        Ok(())
    }
    fn lsp_request(&mut self, msg: lsp::DeMessage) -> Result<()> {
        println!("unknown request {:?}", msg);
        Ok(())
    }
    fn apply_workspace_edit(&mut self, edit: &WorkspaceEdit) -> Result<()> {
        if let Some(ref doc_changes) = edit.document_changes {
            match doc_changes {
                DocumentChanges::Edits(edits) => {
                    for edit in edits {
                        self.apply_text_edits(
                            &edit.text_document.uri,
                            InsertTextFormat::PlainText,
                            &edit.edits,
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
        let sw = match self.ws.get_mut(&wid) {
            Some(sw) => sw,
            // Ignore untracked windows.
            None => return Ok(()),
        };
        let client = sw.client.clone();
        let params = sw.change_params()?;
        self.send_notification::<DidChangeTextDocument>(&client, params)
    }
    fn run_event(&mut self, ev: Event, wid: usize) -> Result<()> {
        self.did_change(wid)?;
        let sw = self.ws.get_mut(&wid).unwrap();
        let client_name = &sw.client.clone();
        let url = sw.url.clone();
        let text_document_position_params = sw.text_doc_pos()?;
        let text_document_position = text_document_position_params.clone();
        let text_document = TextDocumentIdentifier::new(url.clone());
        let work_done_progress_params = WorkDoneProgressParams {
            work_done_token: None,
        };
        let partial_result_params = PartialResultParams {
            partial_result_token: None,
        };
        let range = Range {
            start: text_document_position.position,
            end: text_document_position.position,
        };
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
            "hover" => {
                self.send_request::<HoverRequest>(
                    client_name,
                    url,
                    HoverParams {
                        text_document_position_params,
                        work_done_progress_params,
                    },
                )?;
            }
            "complete" => {
                self.send_request::<Completion>(
                    client_name,
                    url,
                    CompletionParams {
                        text_document_position,
                        work_done_progress_params,
                        partial_result_params,
                        context: Some(CompletionContext {
                            trigger_kind: CompletionTriggerKind::Invoked,
                            trigger_character: None,
                        }),
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
            "signature" => {
                self.send_request::<SignatureHelpRequest>(
                    client_name,
                    url,
                    SignatureHelpParams {
                        context: None,
                        text_document_position_params,
                        work_done_progress_params,
                    },
                )?;
            }
            "lens" => {
                self.send_request::<CodeLensRequest>(
                    client_name,
                    url,
                    CodeLensParams {
                        text_document,
                        work_done_progress_params,
                        partial_result_params,
                    },
                )?;
            }
            "assist" => {
                self.send_request::<CodeActionRequest>(
                    client_name,
                    url,
                    CodeActionParams {
                        text_document,
                        range,
                        context: CodeActionContext {
                            diagnostics: vec![],
                            only: None,
                        },
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
        client_name: &String,
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
    fn run_code_action(&mut self, client_id: ClientId, idx: usize) -> Result<()> {
        let action = self.actions.remove(&client_id).unwrap().remove(idx);
        self.actions.clear();
        self.run_action(action)
    }
    fn run_action(&mut self, action: Action) -> Result<()> {
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
                                println!("json err {}", err);
                                continue;
                            }
                        }
                    }
                }
            }
            Action::Command(CodeActionOrCommand::CodeAction(action)) => {
                if let Some(edit) = action.edit.clone() {
                    self.apply_workspace_edit(&edit)?;
                }
            }
            Action::Completion(url, item) => {
                let format = item
                    .insert_text_format
                    .unwrap_or(InsertTextFormat::PlainText);
                if let Some(edit) = item.text_edit.clone() {
                    match edit {
                        CompletionTextEdit::Edit(edit) => {
                            return self.apply_text_edits(&url, format, &vec![edit])
                        }
                    }
                }
                panic!("unsupported");
            }
        }
        Ok(())
    }
    fn run_cmd(&mut self, ev: Event) -> Result<()> {
        match ev.c2 {
            'x' | 'X' => match ev.text.as_str() {
                "Get" => {
                    self.actions.clear();
                    self.output.clear();
                    self.sync_windows()?;
                    self.diags.clear();
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
                    let mut cid: Option<(ClientId, usize)> = None;
                    for (pos, (client_id, idx)) in self.action_addrs.iter().rev() {
                        if (*pos as u32) < ev.q0 && client_id.msg_id != 0 {
                            cid = Some((client_id.clone(), *idx));
                            break;
                        }
                    }
                    if let Some((cid, idx)) = cid {
                        return self.run_code_action(cid, idx);
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
        let text_document = sw.doc.clone();
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
        self.sync_windows()?;
        // chan index -> (recv chan, self.clients index)

        // one-time index setup
        let mut sel = Select::new();
        let sel_log_r = sel.recv(&self.log_r);
        let sel_ev_r = sel.recv(&self.ev_r);
        let sel_err_r = sel.recv(&self.err_r);
        let mut clients = HashMap::new();

        for (name, c) in &self.clients {
            clients.insert(sel.recv(&c.msg_r), (c.msg_r.clone(), name.to_string()));
        }
        drop(sel);

        let mut no_sync = false;
        loop {
            if !no_sync {
                self.sync()?;
            }
            no_sync = false;

            let mut sel = Select::new();
            sel.recv(&self.log_r);
            sel.recv(&self.ev_r);
            sel.recv(&self.err_r);
            for (_, c) in &self.clients {
                sel.recv(&c.msg_r);
            }
            let index = sel.ready();

            match index {
                _ if index == sel_log_r => {
                    let msg = self.log_r.recv();
                    println!("log {:?}", msg);
                    match msg {
                        Ok(ev) => match ev.op.as_str() {
                            "focus" => {
                                self.focus = ev.name;
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
                    println!("ev {:?}", msg);
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
                    println!("err {:?}", msg);
                    match msg {
                        Ok(_) => {
                            break;
                        }
                        Err(_) => {
                            break;
                        }
                    }
                }
                _ => {
                    let (ch, name) = clients.get(&index).unwrap();
                    let msg = ch.recv()?;
                    self.lsp_msg(name.to_string(), msg)?;
                }
            };
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

fn format_pct(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{:.0}", v),
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
