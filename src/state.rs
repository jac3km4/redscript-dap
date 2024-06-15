use red4rs::{ArrayType, Class, ValueContainer, ValuePtr};
use slab::Slab;

use crate::server::DebugEvent;
use crate::StackFramePtr;

#[derive(Debug, Default)]
pub struct DebugState {
    scopes: Slab<Scope>,
    frames: Vec<StackFramePtr>,
    current: Option<DebugEvent>,
}

impl DebugState {
    #[inline]
    pub fn scope(&self, id: i64) -> Option<&Scope> {
        self.scopes.get((id - 1) as usize)
    }

    #[inline]
    pub fn frames(&self) -> &[StackFramePtr] {
        &self.frames
    }

    #[inline]
    pub fn add_scope(&mut self, scope: Scope) -> i64 {
        self.scopes.insert(scope) as i64 + 1
    }

    #[inline]
    pub fn take_event(&mut self) -> Option<DebugEvent> {
        self.current.take()
    }

    pub fn reset(&mut self, ev: DebugEvent) {
        let frame = unsafe { ev.frame().as_ref() };

        self.scopes.clear();
        self.frames.clear();
        self.frames
            .extend(frame.parent_iter().map(|f| StackFramePtr(f)));
        self.current = Some(ev);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Locals(StackFramePtr),
    Params(StackFramePtr),
    Object(ValueContainer, &'static Class),
    Struct(ValueContainer, &'static Class),
    Array(ValuePtr, &'static ArrayType),
}

unsafe impl Send for Scope {}

unsafe impl Sync for Scope {}
