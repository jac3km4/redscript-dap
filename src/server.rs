use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpListener};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::{env, fmt, io, mem, thread};

use dap::prelude::*;
use dap::server::ServerOutput;
use parking::{Parker, Unparker};
use red4ext_rs::types::{
    ArrayType, Class, IScriptable, Property, TaggedType, Type, ValueContainer, ValuePtr,
};
use red4ext_rs::{log, PluginOps, RttiSystem};
use thiserror::Error;

use crate::control::{FunctionId, StepMode};
use crate::state::{DebugState, Scope};
use crate::{BreakpointKey, RedscriptDap, StackFramePtr, CONTROL};

type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug)]
pub struct ServerHandle {
    sender: OnceLock<SyncSender<DebugEvent>>,
}

impl ServerHandle {
    #[inline]
    pub const fn new() -> Self {
        Self {
            sender: OnceLock::new(),
        }
    }

    #[inline]
    pub fn sender(&self) -> &SyncSender<DebugEvent> {
        self.sender.get_or_init(ServerHandle::init)
    }

    fn init() -> SyncSender<DebugEvent> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(0);

        thread::spawn(move || {
            let env = RedscriptDap::env();

            let listener = match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0))) {
                Ok(listener) => listener,
                Err(err) => {
                    log::error!(env, "Failed to bind debug server: {err}");
                    return;
                }
            };

            let addr = listener.local_addr().expect("should have a local address");
            log::info!(env, "Bound the debug server at {addr}");

            if let Err(err) = Self::create_port_file(addr.port()) {
                log::error!(env, "Failed to create port file: {err}");
            }

            for stream in listener.incoming().flatten() {
                let server = Server::new(BufReader::new(&stream), BufWriter::new(&stream));
                let result = DebugSession::run(server, &receiver);
                if let Err(err) = result {
                    log::error!(env, "Failed during debug session: {err}");
                }
            }
        });

        sender
    }

    fn create_port_file(port: u16) -> DynResult<()> {
        // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilea
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x04000000;

        let path = env::current_exe()?
            .parent()
            .ok_or("exe had no parent")?
            .join(format!("dap.{port}.debug"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)?;

        // leak the file handle, it will be closed when the process exits
        mem::forget(file);

        Ok(())
    }
}

struct DebugSession<R, W>
where
    R: io::Read,
    W: io::Write,
{
    server: Server<R, W>,
    state: Arc<RwLock<DebugState>>,
}

impl<R, W> DebugSession<R, W>
where
    R: io::Read,
    W: io::Write,
{
    fn new(server: Server<R, W>) -> Self {
        Self {
            server,
            state: Arc::new(RwLock::new(DebugState::default())),
        }
    }

    fn run(server: Server<R, W>, receiver: &Receiver<DebugEvent>) -> DynResult<()>
    where
        R: Send,
        W: Send,
    {
        let mut this = Self::new(server);
        let state = this.state.clone();
        let output = this.server.output.clone();

        let result = thread::scope(|scope| {
            let task = scope.spawn::<_, DynResult<_>>(|| loop {
                let req = match this.server.poll_request()? {
                    Some(req) => req,
                    None => return Err(Box::new(Error::MissingCommand)),
                };
                match this.handle_req(req)? {
                    Outcome::Shutdown => break Ok(()),
                    Outcome::Continue => {}
                }
            });
            loop {
                match receiver.try_recv() {
                    Ok(ev) => Self::handle_event(&state, &output, ev)?,
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => break,
                }
                if task.is_finished() {
                    return task.join().unwrap();
                }
                std::thread::yield_now();
            }
            Ok(())
        });

        CONTROL.reset();
        // clear the in-progress event if any
        state.clear_poison();
        if let Some(ev) = state.write().unwrap().take_event() {
            ev.unpark();
        };
        // allow all waiting threads to continue
        while let Ok(ev) = receiver.try_recv() {
            ev.unpark();
        }

        result
    }

    fn handle_event(
        state: &RwLock<DebugState>,
        output: &Mutex<ServerOutput<W>>,
        ev: DebugEvent,
    ) -> DynResult<()> {
        let reason = match ev.cause {
            EventCause::Breakpoint => types::StoppedEventReason::Breakpoint,
            EventCause::Step => types::StoppedEventReason::Step,
        };
        state.write().unwrap().reset(ev);

        output
            .lock()
            .unwrap()
            .send_event(Event::Stopped(events::StoppedEventBody {
                reason,
                description: None,
                thread_id: Some(0),
                preserve_focus_hint: None,
                text: None,
                all_threads_stopped: Some(false),
                hit_breakpoint_ids: None,
            }))?;

        Ok(())
    }

    fn handle_req(&mut self, req: Request) -> DynResult<Outcome> {
        match &req.command {
            Command::Attach(_) => {}
            Command::ConfigurationDone => {
                let res = req.success(ResponseBody::ConfigurationDone);
                self.server.respond(res)?;
            }
            Command::Continue(_) => {
                let Some(ev) = self.state.write().unwrap().take_event() else {
                    return Err(Box::new(Error::UnexpectedRequest));
                };
                ev.unpark();
                let res = req.success(ResponseBody::Continue(responses::ContinueResponse {
                    all_threads_continued: Some(true),
                }));
                self.server.respond(res)?;
            }
            Command::Disconnect(_) => {
                if let Some(ev) = self.state.write().unwrap().take_event() {
                    ev.unpark();
                }

                let res = req.success(ResponseBody::Disconnect);
                self.server.respond(res)?;
                return Ok(Outcome::Shutdown);
            }
            Command::Initialize(_) => {
                let res = req.success(ResponseBody::Initialize(types::Capabilities {
                    supports_configuration_done_request: Some(true),
                    ..Default::default()
                }));
                self.server.respond(res)?;
                self.server.send_event(Event::Initialized)?;
            }
            Command::Launch(_) => {
                let res = req.success(ResponseBody::Launch);
                self.server.respond(res)?;
            }
            Command::Next(_) => {
                let Some(ev) = self.state.write().unwrap().take_event() else {
                    return Err(Box::new(Error::UnexpectedRequest));
                };
                CONTROL.set_step_mode(StepMode::StepOver);
                ev.unpark();

                let res = req.success(ResponseBody::Next);
                self.server.respond(res)?;
            }
            Command::Pause(_) => {
                // TODO pause
            }
            Command::Scopes(scopes) => {
                let Some(&frame) = self
                    .state
                    .read()
                    .unwrap()
                    .frames()
                    .get(scopes.frame_id as usize)
                else {
                    return Err(Box::new(Error::UnkownStackFrame));
                };
                let mut guard = self.state.write().unwrap();
                let params = guard.add_scope(Scope::Params(frame));
                let locals = guard.add_scope(Scope::Locals(frame));

                let res = req.success(ResponseBody::Scopes(responses::ScopesResponse {
                    scopes: vec![
                        types::Scope {
                            name: "Arguments".to_owned(),
                            presentation_hint: Some(types::ScopePresentationhint::Arguments),
                            variables_reference: params,
                            ..Default::default()
                        },
                        types::Scope {
                            name: "Locals".to_owned(),
                            presentation_hint: Some(types::ScopePresentationhint::Locals),
                            variables_reference: locals,
                            ..Default::default()
                        },
                    ],
                }));
                self.server.respond(res)?;
            }
            Command::SetBreakpoints(args) => {
                let Some(path) = args.source.path.as_deref() else {
                    let res = req.success(ResponseBody::SetBreakpoints(
                        responses::SetBreakpointsResponse {
                            breakpoints: vec![],
                        },
                    ));
                    self.server.respond(res)?;
                    return Ok(Outcome::Continue);
                };

                let (breakpoints, pending): (Vec<_>, Vec<_>) = args
                    .breakpoints
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|bp| {
                        let line = bp.line as u16;
                        let func = CONTROL.functions().get_by_loc(path, line.into())?;
                        let pending = BreakpointKey::new(func, line);
                        let bp = types::Breakpoint {
                            verified: true,
                            source: Some(args.source.clone()),
                            line: Some(bp.line),
                            ..Default::default()
                        };
                        Some((bp, pending))
                    })
                    .unzip();

                let mut bp_guard = CONTROL.breakpoints_mut();
                for func in CONTROL.functions().get_by_file(path) {
                    bp_guard.unregister_by_fn(func);
                }
                for bp in pending {
                    bp_guard.add(bp);
                }

                let res = req.success(ResponseBody::SetBreakpoints(
                    responses::SetBreakpointsResponse { breakpoints },
                ));
                self.server.respond(res)?;
            }
            Command::StackTrace(args) => {
                let guard = self.state.read().unwrap();
                let start = args.start_frame.unwrap_or(0) as usize;
                let end = args
                    .levels
                    .map(|l| start + l as usize)
                    .filter(|&l| l != 0)
                    .unwrap_or(guard.frames().len())
                    .min(guard.frames().len());
                let range = start..end;

                let stack_frames = guard.frames()[start..end]
                    .iter()
                    .zip(range)
                    .map(|(frame, i)| {
                        let line = unsafe {
                            frame
                                .as_invoke_static()
                                .map(|i| i.line)
                                .or_else(|| frame.as_invoke_virtual().map(|i| i.line))
                        };
                        let frame = unsafe { frame.as_ref() };

                        let name = frame.func().name().as_str();
                        let (name, _) = name.split_once(';').unwrap_or((name, ""));
                        let name = match frame.func().parent() {
                            Some(class) => format!("{}::{}", class.name().as_str(), name),
                            None => name.to_owned(),
                        };

                        let fns = CONTROL.functions();
                        let source = fns.get(FunctionId::from_func(frame.func()));

                        let (source, line) = match (source, line) {
                            (Some(source), Some(line)) => {
                                let path = source.file_path();
                                let name = Path::new(path)
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned());
                                (
                                    Some(types::Source {
                                        name,
                                        path: Some(path.to_owned()),
                                        ..Default::default()
                                    }),
                                    line as i64,
                                )
                            }
                            _ => (None, 0),
                        };

                        types::StackFrame {
                            id: i as i64,
                            name,
                            source,
                            line,
                            ..Default::default()
                        }
                    })
                    .collect();

                let res = req.success(ResponseBody::StackTrace(responses::StackTraceResponse {
                    stack_frames,
                    total_frames: Some(guard.frames().len() as i64),
                }));
                self.server.respond(res)?;
            }
            Command::StepIn(_) => {
                let Some(ev) = self.state.write().unwrap().take_event() else {
                    return Err(Box::new(Error::UnexpectedRequest));
                };
                CONTROL.set_step_mode(StepMode::StepIn);
                ev.unpark();

                let res = req.success(ResponseBody::StepIn);
                self.server.respond(res)?;
            }
            Command::StepOut(_) => {
                let Some(ev) = self.state.write().unwrap().take_event() else {
                    return Err(Box::new(Error::UnexpectedRequest));
                };
                if unsafe { ev.frame().as_ref() }.parent().is_some() {
                    CONTROL.set_step_mode(StepMode::StepOut);
                }
                ev.unpark();

                let res = req.success(ResponseBody::StepOut);
                self.server.respond(res)?;
            }
            Command::Threads => {
                let res = req.success(ResponseBody::Threads(responses::ThreadsResponse {
                    threads: vec![types::Thread {
                        id: 0,
                        name: "Main".to_owned(),
                    }],
                }));
                self.server.respond(res)?;
            }
            Command::Variables(vars) => {
                let scope = {
                    let guard = self.state.read().unwrap();
                    *guard
                        .scope(vars.variables_reference)
                        .ok_or_else(|| Box::new(Error::UnknownScope))?
                };

                let mut variables = match scope {
                    Scope::Locals(frame) => {
                        let frame = unsafe { frame.as_ref() };
                        let mut variables = vec![];

                        if let Some(this) = frame
                            .context()
                            .filter(|_| !frame.func().flags().is_static())
                        {
                            let scope = Scope::Object(this.fields(), this.class().name());
                            let variable = self.state.write().unwrap().add_scope(scope);
                            variables.push(types::Variable {
                                name: "this".to_owned(),
                                variables_reference: variable,
                                ..Default::default()
                            });
                        }

                        let locals = self
                            .named_variables(frame.locals(), frame.func().locals().iter().copied());
                        variables.extend(locals);
                        variables
                    }
                    Scope::Params(frame) => {
                        let frame = unsafe { frame.as_ref() };
                        self.named_variables(frame.params(), frame.func().params().iter().copied())
                            .collect::<Vec<_>>()
                    }
                    Scope::Array(ptr, typ) => self
                        .array_variables(ptr, unsafe { &*typ })
                        .collect::<Vec<_>>(),
                    Scope::Object(ptr, name) => {
                        let rtti = RttiSystem::get();
                        let props = rtti
                            .get_class(name)
                            .into_iter()
                            .flat_map(Class::all_properties)
                            .filter(|p| p.flags().in_value_holder());
                        self.named_variables(ptr, props).collect::<Vec<_>>()
                    }
                    Scope::Struct(ptr, typ) => {
                        let rtti = RttiSystem::get();
                        let props = rtti
                            .get_class(typ)
                            .into_iter()
                            .flat_map(Class::all_properties);
                        self.named_variables(ptr, props).collect::<Vec<_>>()
                    }
                };
                let start = vars.start.unwrap_or(0) as usize;
                let end = vars
                    .count
                    .filter(|&c| c != 0)
                    .map(|c| start + c as usize)
                    .unwrap_or(variables.len())
                    .min(variables.len());
                let variables = variables.drain(start..end).collect();

                let res = req.success(ResponseBody::Variables(responses::VariablesResponse {
                    variables,
                }));
                self.server.respond(res)?;
            }
            Command::Source(_) | Command::Evaluate(_) => {
                // TODO
            }
            _ => return Err(Box::new(Error::UnhandledCommand)),
        };
        Ok(Outcome::Continue)
    }

    fn named_variables<'a>(
        &'a mut self,
        container: ValueContainer,
        props: impl IntoIterator<Item = &'a Property> + 'a,
    ) -> impl Iterator<Item = types::Variable> + 'a {
        props.into_iter().map(move |prop| {
            let val = unsafe { prop.value(container) };
            self.variable(prop.name().as_str(), val, prop.type_())
        })
    }

    fn array_variables<'a>(
        &'a mut self,
        value: ValuePtr,
        typ: &'a ArrayType,
    ) -> impl Iterator<Item = types::Variable> + 'a {
        let inner = typ.element_type();
        (0..unsafe { typ.length(value) })
            .map(move |i| self.variable(i, unsafe { typ.element(value, i) }, inner))
    }

    fn variable(&mut self, name: impl ToString, value: ValuePtr, typ: &Type) -> types::Variable {
        let mut name = name.to_string();
        name.truncate(name.find('$').unwrap_or(name.len()));

        let scope = match typ.tagged() {
            TaggedType::Class(class) => {
                Some(Scope::Struct(unsafe { value.to_container() }, class.name()))
            }
            TaggedType::Ref(_) | TaggedType::WeakRef(_) => unsafe { value.unwrap_ref() }
                .map(|inst| Scope::Object(inst.fields(), inst.class().name())),
            TaggedType::Array(_)
            | TaggedType::StaticArray(_)
            | TaggedType::NativeArray(_)
            | TaggedType::FixedArray(_) => Some(Scope::Array(value, typ.as_array().unwrap())),
            _ => None,
        };

        let variable = scope.map_or(0, |scope| self.state.write().unwrap().add_scope(scope));

        types::Variable {
            name,
            value: ValueFormatter::new(value, typ).to_string(),
            variables_reference: variable,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Shutdown,
    Continue,
}

#[derive(Error, Debug)]
enum Error {
    #[error("unhandled command")]
    UnhandledCommand,
    #[error("missing command")]
    MissingCommand,
    #[error("unexpected request")]
    UnexpectedRequest,
    #[error("unkown stack frame")]
    UnkownStackFrame,
    #[error("unkown scope")]
    UnknownScope,
}

#[derive(Debug)]
#[allow(unused)]
pub struct DebugEvent {
    key: BreakpointKey,
    cause: EventCause,
    frame: StackFramePtr,
    unparker: Unparker,
}

impl DebugEvent {
    pub fn new(key: BreakpointKey, cause: EventCause, frame: StackFramePtr) -> (Self, Parker) {
        let (parker, unparker) = parking::pair();
        (
            Self {
                key,
                cause,
                frame,
                unparker,
            },
            parker,
        )
    }

    #[inline]
    pub fn frame(&self) -> StackFramePtr {
        self.frame
    }

    #[inline]
    pub fn unpark(&self) {
        self.unparker.unpark();
    }
}

#[derive(Debug)]
pub enum EventCause {
    Breakpoint,
    Step,
}

#[derive(Debug)]
struct ValueFormatter<'a> {
    value: ValuePtr,
    typ: &'a Type,
    visited: RefCell<HashSet<*const IScriptable>>,
}

impl<'a> ValueFormatter<'a> {
    #[inline]
    fn new(value: ValuePtr, typ: &'a Type) -> Self {
        Self {
            value,
            typ,
            visited: RefCell::new(HashSet::new()),
        }
    }
}

impl fmt::Display for ValueFormatter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.typ.tagged() {
            TaggedType::Ref(_) | TaggedType::WeakRef(_) => {
                let inst = unsafe { self.value.unwrap_ref() };
                let Some(inst) = inst else {
                    return write!(f, "null");
                };
                if !self.visited.borrow_mut().insert(inst as _) {
                    return write!(f, "<cyclic>");
                }
                write!(f, "{{",)?;

                let mut prop_it = inst
                    .class()
                    .all_properties()
                    .filter(|p| p.flags().in_value_holder());
                for prop in prop_it.by_ref().take(8) {
                    let inner = ValueFormatter {
                        value: unsafe { prop.value(inst.fields()) },
                        typ: prop.type_(),
                        visited: self.visited.clone(),
                    };
                    write!(f, "{}: {}, ", prop.name().as_str(), inner)?;
                }
                if prop_it.next().is_some() {
                    write!(f, "...")?;
                }

                write!(f, "}}")
            }
            _ => {
                write!(f, "{}", unsafe { self.typ.to_string(self.value) })
            }
        }
    }
}
