use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::{fmt, io, ptr, thread};

use dap::prelude::*;
use dap::server::ServerOutput;
use parking::{Parker, Unparker};
use red4rs::{
    ArrayType, IScriptable, InvokeStatic, InvokeVirtual, Property, Type, ValueContainer, ValuePtr,
    CALL_INSTR_SIZE, OPCODE_SIZE,
};
use responses::{ScopesResponse, SetBreakpointsResponse};
use thiserror::Error;

use crate::control::{FunctionId, StepMode};
use crate::state::{DebugState, Scope};
use crate::{BreakpointKey, StackFramePtr, CONTROL};

type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug)]
pub struct ServerHandle {
    sender: OnceLock<SyncSender<DebugEvent>>,
    port: u16,
}

impl ServerHandle {
    #[inline]
    pub const fn new(port: u16) -> Self {
        Self {
            sender: OnceLock::new(),
            port,
        }
    }

    #[inline]
    pub fn sender(&self) -> &SyncSender<DebugEvent> {
        self.sender.get_or_init(|| ServerHandle::init(self.port))
    }

    fn init(port: u16) -> SyncSender<DebugEvent> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(0);

        thread::spawn(move || {
            let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))?;
            for stream in listener.incoming().flatten() {
                let server = Server::new(BufReader::new(&stream), BufWriter::new(&stream));
                let result = DebugSession::run(server, &receiver);
                if let Err(err) = result {
                    eprintln!("Error: {}", err);
                }
            }
            DynResult::Ok(())
        });
        sender
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
    R: io::Read + Send,
    W: io::Write + Send,
{
    fn new(server: Server<R, W>) -> Self {
        Self {
            server,
            state: Arc::new(RwLock::new(DebugState::default())),
        }
    }

    fn run(server: Server<R, W>, receiver: &Receiver<DebugEvent>) -> DynResult<()> {
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

        // clear the in-progress event if any
        state.clear_poison();
        if let Some(ev) = state.write().unwrap().take_event() {
            ev.unpark();
        };

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

    fn handle_req(&mut self, req: Request) -> DynResult<Outcome>
    where
        R: io::Read,
        W: io::Write,
    {
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

                let res = req.success(ResponseBody::Scopes(ScopesResponse {
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
                    let res = req.success(ResponseBody::SetBreakpoints(SetBreakpointsResponse {
                        breakpoints: vec![],
                    }));
                    self.server.respond(res)?;
                    return Ok(Outcome::Continue);
                };

                let (breakpoints, pending): (Vec<_>, Vec<_>) = args
                    .breakpoints
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|bp| {
                        // DAP uses 1-based line numbers
                        let line = bp.line as u16 - 1;
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

                let res = req.success(ResponseBody::SetBreakpoints(SetBreakpointsResponse {
                    breakpoints,
                }));
                self.server.respond(res)?;
            }

            Command::Source(_) => {
                // TODO source
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
                let top = guard.frames().first().map(|&StackFramePtr(ptr)| ptr);

                let stack_frames = guard.frames()[start..end]
                    .iter()
                    .enumerate()
                    .map(|(i, frame)| {
                        let frame = unsafe { frame.as_ref() };

                        let offset = if top.is_some_and(|top| ptr::eq(top, frame)) {
                            // the top frame will be pointed at a call instruction + OPCODE_SIZE
                            -OPCODE_SIZE
                        } else {
                            // the parent frames will be pointed at the beginning of the next instruction
                            -OPCODE_SIZE - CALL_INSTR_SIZE - OPCODE_SIZE
                        };

                        let line = unsafe {
                            frame
                                .instr_at::<InvokeStatic>(offset)
                                .map(|i| i.line)
                                .or_else(|| frame.instr_at::<InvokeVirtual>(offset).map(|i| i.line))
                        };

                        let fns = CONTROL.functions();
                        let source = fns.get(FunctionId::from_func(frame.func()));
                        let name = frame.func().name().as_str();
                        let (name, _) = name.split_once(';').unwrap_or((name, ""));

                        types::StackFrame {
                            id: i as i64,
                            name: name.to_owned(),
                            source: source.map(|s| types::Source {
                                name: Path::new(s.file_path())
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned()),
                                path: Some(s.file_path().to_owned()),
                                ..Default::default()
                            }),
                            // DAP uses 1-based line numbers
                            line: line.unwrap_or(0) as i64 + 1,
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
                // TODO: step out is currently broken so this will just step over
                CONTROL.set_step_mode(StepMode::StepOver);
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
                        self.named_variables(frame.locals(), frame.func().locals().iter().copied())
                    }
                    Scope::Params(frame) => {
                        let frame = unsafe { frame.as_ref() };
                        self.named_variables(frame.params(), frame.func().params().iter().copied())
                    }
                    Scope::Array(ptr, typ) => self.array_variables(ptr, typ),
                    Scope::Object(ptr, typ) => {
                        let props = typ.all_properties().filter(|p| p.is_in_value_holder());
                        self.named_variables(ptr, props)
                    }
                    Scope::Struct(ptr, typ) => self.named_variables(ptr, typ.all_properties()),
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
            _ => return Err(Box::new(Error::UnhandledCommand)),
        };
        Ok(Outcome::Continue)
    }

    fn named_variables<'a>(
        &mut self,
        container: ValueContainer,
        props: impl IntoIterator<Item = &'a Property>,
    ) -> Vec<types::Variable> {
        props
            .into_iter()
            .map(|prop| {
                let val = unsafe { prop.value(container) };
                self.variable(prop.name().as_str(), val, prop.type_())
            })
            .collect()
    }

    fn array_variables(&mut self, value: ValuePtr, typ: &ArrayType) -> Vec<types::Variable> {
        let inner = typ.inner_type();
        (0..unsafe { typ.length(value) })
            .map(|i| self.variable(i, unsafe { typ.element(value, i) }, inner))
            .collect()
    }

    fn variable(
        &mut self,
        name: impl ToString,
        value: ValuePtr,
        typ: &'static Type,
    ) -> types::Variable {
        let mut name = name.to_string();
        name.truncate(name.find('$').unwrap_or(name.len()));

        let scope = if typ.kind().is_pointer() {
            unsafe { value.unwrap_ref() }.map(|inst| Scope::Object(inst.fields(), inst.class()))
        } else if let Some(class) = typ.as_class() {
            Some(Scope::Struct(unsafe { value.to_container() }, class))
        } else {
            typ.as_array().map(|array| Scope::Array(value, array))
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
    pub fn frame(&self) -> &StackFramePtr {
        &self.frame
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
        if self.typ.kind().is_pointer() {
            let inst = unsafe { self.value.unwrap_ref() };
            let Some(inst) = inst else {
                return write!(f, "null");
            };
            if !self.visited.borrow_mut().insert(inst as _) {
                return write!(f, "<recursive>");
            }
            write!(f, "{{",)?;
            for prop in inst
                .class()
                .all_properties()
                .filter(|p| p.is_in_value_holder())
            {
                let inner = Self {
                    value: unsafe { prop.value(inst.fields()) },
                    typ: prop.type_(),
                    visited: self.visited.clone(),
                };
                write!(f, "{}: {}, ", prop.name().as_str(), inner)?;
            }
            write!(f, "}}")
        } else {
            write!(f, "{}", unsafe { self.typ.to_string(self.value) })
        }
    }
}
