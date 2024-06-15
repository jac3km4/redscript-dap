use std::ffi::OsStr;
use std::path::Path;
use std::{env, mem, panic, ptr};

use control::{BreakpointKey, DebugControl, FunctionId, SourceRef, StepMode};
use red4rs::{
    export_plugin, hashes, hooks, wcstr, CName, Function, GameApp, IScriptable, Instr,
    InvokeStatic, InvokeVirtual, OpcodeHandler, Plugin, PluginSyntax, SdkEnv, SemVer, StackFrame,
    StateListener, StateType, U16CStr, VoidPtr, OPCODE_SIZE,
};
use server::{DebugEvent, EventCause, ServerHandle};

mod control;
mod server;
mod state;

static CONTROL: DebugControl = DebugControl::new();
static SERVER: ServerHandle = ServerHandle::new(DEBUG_PORT);

// the TCP port where the debugger server will listen
const DEBUG_PORT: u16 = 8435;

// hash of the method that binds script functions
const BIND_FUNCTION_HASH: u32 = 777921665;

hooks! {
    static BIND_FUNCTION:
        fn(this: VoidPtr, f: *mut FunctionInfo, arg2: VoidPtr) -> bool;

    static INVOKE_STATIC_HANDLER:
        fn(i: *mut IScriptable, f: *mut StackFrame, a3: VoidPtr, a4: VoidPtr) -> ();

    static INVOKE_VIRTUAL_HANDLER:
        fn(i: *mut IScriptable, f: *mut StackFrame, a3: VoidPtr, a4: VoidPtr) -> ();
}

struct RedscriptDap;

export_plugin!(RedscriptDap);

impl Plugin for RedscriptDap {
    const AUTHOR: &'static U16CStr = wcstr!("jekky");
    const NAME: &'static U16CStr = wcstr!("redscript-dap");
    const VERSION: SemVer = SemVer::new(0, 1, 0);

    fn on_init(env: &SdkEnv) {
        let bind_function_addr = hashes::resolve(BIND_FUNCTION_HASH);
        // set up the function bind hook first because it happens on initialization
        unsafe {
            env.attach_hook(
                BIND_FUNCTION,
                #[allow(clippy::missing_transmute_annotations)]
                mem::transmute(bind_function_addr),
                on_bind_function,
            )
        };

        // we do the remaining setup after the game is initialized
        env.add_listener(
            StateType::Initialization,
            StateListener::default().with_on_exit(on_app_init),
        );

        // use the server handle to make sure it's initialized
        let _ = SERVER.sender();
    }
}

unsafe extern "C" fn on_app_init(_app: &GameApp) {
    let handlers = hashes::resolve(hashes::OpcodeHandlers) as *const OpcodeHandler;

    let invoke_static_handler = *handlers.add(InvokeStatic::OPCODE.into());
    let invoke_virtual_handler = *handlers.add(InvokeVirtual::OPCODE.into());

    // bind remaining hooks once the game is initialized
    // these are responsible for handling breakpoints and stepping
    let env = RedscriptDap::env();
    env.attach_hook(
        INVOKE_STATIC_HANDLER,
        invoke_static_handler,
        on_invoke_static,
    );
    env.attach_hook(
        INVOKE_VIRTUAL_HANDLER,
        invoke_virtual_handler,
        on_invoke_virtual,
    );
}

unsafe extern "C" fn on_bind_function(
    this: VoidPtr,
    info: *mut FunctionInfo,
    arg: VoidPtr,
    cb: unsafe extern "C" fn(this: VoidPtr, f: *mut FunctionInfo, arg2: VoidPtr) -> bool,
) -> bool {
    let ret = cb(this, info, arg);

    let info = &*info;
    if info.func.is_null() || info.source_info.is_null() {
        return ret;
    };
    let func = &*info.func;
    let source = &*info.source_info;

    let path = Path::new(source.path.as_ref());

    // if it's not redscript we point to the redmod scripts
    let path = if path.extension() != Some(OsStr::new("reds")) {
        (|| {
            let res = env::current_exe()
                .ok()?
                .parent()?
                .parent()?
                .parent()?
                .join("tools")
                .join("redmod")
                .join("scripts")
                .join(path);
            Some(res.clone())
        })()
        .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };

    CONTROL.functions_mut().add(
        FunctionId::from_func(func),
        SourceRef::new(path.to_string_lossy(), info.source_line),
    );

    ret
}

unsafe extern "C" fn on_invoke_static(
    i: *mut IScriptable,
    f: *mut StackFrame,
    a3: VoidPtr,
    a4: VoidPtr,
    cb: OpcodeHandler,
) {
    let frame = &*f;
    if !frame.has_code() {
        return cb(i, f, a3, a4);
    }

    if let Some(i) = frame.instr_at::<InvokeStatic>(-OPCODE_SIZE) {
        handle_call(frame, i.line);
    }

    cb(i, f, a3, a4)
}

unsafe extern "C" fn on_invoke_virtual(
    i: *mut IScriptable,
    f: *mut StackFrame,
    a3: VoidPtr,
    a4: VoidPtr,
    cb: OpcodeHandler,
) {
    let frame = &*f;
    if !frame.has_code() {
        return cb(i, f, a3, a4);
    }

    if let Some(i) = frame.instr_at::<InvokeVirtual>(-OPCODE_SIZE) {
        handle_call(frame, i.line);
    }

    cb(i, f, a3, a4)
}

fn handle_call(frame: &StackFrame, line: u16) {
    let key = BreakpointKey::new(FunctionId::from_func(frame.func()), line);

    let step_mode = CONTROL.get_step_mode();
    let break_cause = {
        if step_mode > StepMode::None {
            let last = CONTROL.get_last_break_frame();
            let last_parent = last.and_then(StackFrame::parent);

            let should_break = matches!(last_parent, Some(parent) if ptr::eq(parent, frame))
                || (step_mode >= StepMode::StepOver
                    && matches!(last, Some(last) if ptr::eq(last, frame)))
                || (step_mode == StepMode::StepIn
                    && matches!((last, frame.parent()), (Some(last), Some(parent)) if ptr::eq(last, parent)));

            should_break.then_some(EventCause::Step)
        } else {
            CONTROL
                .breakpoints()
                .get(&key)
                .map(|_| EventCause::Breakpoint)
        }
    };

    if let Some(cause) = break_cause {
        breakpoint(key, cause, frame);
    }
}

fn breakpoint(key: BreakpointKey, cause: EventCause, frame: &StackFrame) {
    CONTROL.set_last_break_frame(frame);
    CONTROL.set_step_mode(StepMode::None);

    let (ev, parker) = DebugEvent::new(key, cause, StackFramePtr(frame));
    if SERVER.sender().send(ev).is_ok() {
        parker.park();
    }
}

#[repr(C)]
struct FunctionInfo {
    vft: VoidPtr,
    name: CName,
    unk: u64,
    func: *mut Function,
    padding: [u8; 160],
    source_info: *mut SourceFileInfo,
    source_line: u32,
}

#[repr(C)]
struct SourceFileInfo {
    vfs: VoidPtr,
    name: CName,
    hash: u32,
    crc: u32,
    index: u32,
    path_hash: u64,
    path: red4rs::String,
}

#[derive(Debug, Clone, Copy)]
struct StackFramePtr(*const StackFrame);

impl StackFramePtr {
    pub unsafe fn as_ref(&self) -> &StackFrame {
        unsafe { &*self.0 }
    }
}

unsafe impl Send for StackFramePtr {}

unsafe impl Sync for StackFramePtr {}
