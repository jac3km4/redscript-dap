use std::{mem, panic, ptr};

use control::{BreakpointKey, DebugControl, FunctionId, StepMode};
use red4ext_rs::types::{
    CName, Function, IScriptable, Instr, InvokeStatic, InvokeVirtual, RedString, StackFrame,
    CALL_INSTR_SIZE, OPCODE_SIZE,
};
use red4ext_rs::{
    export_plugin_symbols, addr_hashes, hooks, wcstr, GameApp, Plugin, PluginOps, SdkEnv, SemVer,
    StateListener, StateType, U16CStr, VoidPtr,
};
use server::{DebugEvent, EventCause, ServerHandle};
use static_assertions::const_assert_eq;

mod control;
mod server;
mod state;

static CONTROL: DebugControl = DebugControl::new();
static SERVER: ServerHandle = ServerHandle::new();

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

export_plugin_symbols!(RedscriptDap);

impl Plugin for RedscriptDap {
    const AUTHOR: &'static U16CStr = wcstr!("jekky");
    const NAME: &'static U16CStr = wcstr!("redscript-dap");
    const VERSION: SemVer = SemVer::new(0, 0, 6);

    fn on_init(env: &SdkEnv) {
        let bind_function_addr = addr_hashes::resolve(BIND_FUNCTION_HASH);
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
    let handlers = addr_hashes::resolve(addr_hashes::OpcodeHandlers)
        as *const unsafe extern "C" fn(
            i: *mut IScriptable,
            f: *mut StackFrame,
            a3: VoidPtr,
            a4: VoidPtr,
        );

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

    CONTROL
        .functions_mut()
        .add(FunctionId::from_func(func), source, info.source_line);

    ret
}

unsafe extern "C" fn on_invoke_static(
    i: *mut IScriptable,
    f: *mut StackFrame,
    a3: VoidPtr,
    a4: VoidPtr,
    cb: unsafe extern "C" fn(i: *mut IScriptable, f: *mut StackFrame, a3: VoidPtr, a4: VoidPtr),
) {
    let frame = &*f;
    if !frame.has_code() {
        return cb(i, f, a3, a4);
    }

    if let Some(instr) = frame.instr_at::<InvokeStatic>(-OPCODE_SIZE) {
        pre_call(frame, instr.line);
        cb(i, f, a3, a4);
        post_call(frame, instr.line);
    } else {
        cb(i, f, a3, a4);
    }
}

unsafe extern "C" fn on_invoke_virtual(
    i: *mut IScriptable,
    f: *mut StackFrame,
    a3: VoidPtr,
    a4: VoidPtr,
    cb: unsafe extern "C" fn(i: *mut IScriptable, f: *mut StackFrame, a3: VoidPtr, a4: VoidPtr),
) {
    let frame = &*f;
    if !frame.has_code() {
        return cb(i, f, a3, a4);
    }

    if let Some(instr) = frame.instr_at::<InvokeVirtual>(-OPCODE_SIZE) {
        pre_call(frame, instr.line);
        cb(i, f, a3, a4);
        post_call(frame, instr.line);
    } else {
        cb(i, f, a3, a4);
    }
}

fn pre_call(frame: &StackFrame, line: u16) {
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
        breakpoint(key, cause, StackFramePtr::PreCall(frame));
    }
}

fn post_call(frame: &StackFrame, line: u16) {
    if CONTROL.get_step_mode() == StepMode::StepOut {
        let last_parent = CONTROL.get_last_break_frame().and_then(StackFrame::parent);
        if last_parent.map_or(false, |parent| ptr::eq(parent, frame)) {
            let key = BreakpointKey::new(FunctionId::from_func(frame.func()), line);
            breakpoint(key, EventCause::Step, StackFramePtr::PostCall(frame));
        }
    }
}

fn breakpoint(key: BreakpointKey, cause: EventCause, frame: StackFramePtr) {
    let (ev, parker) = DebugEvent::new(key, cause, frame);
    if SERVER.sender().send(ev).is_ok() {
        CONTROL.set_last_break_frame(frame.as_ptr());
        CONTROL.set_step_mode(StepMode::None);

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

const_assert_eq!(mem::size_of::<FunctionInfo>(), 208);

#[repr(C)]
struct SourceFileInfo {
    vfs: VoidPtr,
    name: CName,
    unk: u64,
    crc: u32,
    index: u32,
    path_hash: u32,
    path: RedString,
}

const_assert_eq!(mem::size_of::<SourceFileInfo>(), 72);

#[derive(Debug, Clone, Copy)]
enum StackFramePtr {
    PreCall(*const StackFrame),
    PostCall(*const StackFrame),
}

impl StackFramePtr {
    #[inline]
    pub unsafe fn as_ref(&self) -> &StackFrame {
        unsafe { &*self.as_ptr() }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const StackFrame {
        match self {
            StackFramePtr::PreCall(ptr) | StackFramePtr::PostCall(ptr) => *ptr,
        }
    }

    #[inline]
    pub unsafe fn as_invoke_static(&self) -> Option<&InvokeStatic> {
        unsafe { self.as_ref() }.instr_at::<InvokeStatic>(self.call_offset())
    }

    #[inline]
    pub unsafe fn as_invoke_virtual(&self) -> Option<&InvokeVirtual> {
        unsafe { self.as_ref() }.instr_at::<InvokeVirtual>(self.call_offset())
    }

    fn call_offset(&self) -> isize {
        match self {
            StackFramePtr::PreCall(_) => -OPCODE_SIZE,
            StackFramePtr::PostCall(_) => -OPCODE_SIZE - CALL_INSTR_SIZE - OPCODE_SIZE,
        }
    }
}

unsafe impl Send for StackFramePtr {}

unsafe impl Sync for StackFramePtr {}
