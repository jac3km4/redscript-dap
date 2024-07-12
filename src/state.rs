use std::iter;

use red4ext_rs::types::{ArrayType, CName, ValueContainer, ValuePtr};
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
        // we avoid using 0 because DAP uses 0 to indicate an empty scope
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
        let frame = ev.frame();
        let parents = unsafe { frame.as_ref() }
            .parent_iter()
            .map(|f| StackFramePtr::PostCall(f));

        self.scopes.clear();
        self.frames.clear();
        self.frames.extend(iter::once(frame).chain(parents));
        self.current = Some(ev);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Locals(StackFramePtr),
    Params(StackFramePtr),
    Object(ValueContainer, CName),
    Struct(ValueContainer, CName),
    Array(ValuePtr, *const ArrayType),
}

unsafe impl Send for Scope {}

unsafe impl Sync for Scope {}
