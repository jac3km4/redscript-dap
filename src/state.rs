use std::{iter, mem};

use dap::requests::SetBreakpointsArguments;
use red4ext_rs::types::{ArrayType, CName, ValueContainer, ValuePtr};
use slab::Slab;

use crate::StackFramePtr;
use crate::server::BreakpointEvent;

#[derive(Debug, Default)]
pub struct DebugState {
    scopes: Slab<Scope>,
    frames: Vec<StackFramePtr>,
    current: Option<BreakpointEvent>,
    uninitialized: UninitializedState,
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
    pub fn take_event(&mut self) -> Option<BreakpointEvent> {
        self.current.take()
    }

    pub fn reset(&mut self, ev: BreakpointEvent) {
        let frame = ev.frame();
        let parents = unsafe { frame.as_ref() }
            .parent_iter()
            .map(|f| StackFramePtr::PostCall(f));

        self.scopes.clear();
        self.frames.clear();
        self.frames.extend(iter::once(frame).chain(parents));
        self.current = Some(ev);
    }

    pub fn mark_ready(&mut self) -> Vec<SetBreakpointsArguments> {
        if let UninitializedState::Uninitialized(pending) = mem::take(&mut self.uninitialized) {
            self.uninitialized = UninitializedState::Initialized;
            pending
        } else {
            vec![]
        }
    }

    pub fn enqueue(&mut self, args: SetBreakpointsArguments) -> Option<SetBreakpointsArguments> {
        match &mut self.uninitialized {
            UninitializedState::Uninitialized(queue) => {
                queue.push(args);
                None
            }
            UninitializedState::Initialized => Some(args),
        }
    }
}

#[derive(Debug, Clone)]
enum UninitializedState {
    Uninitialized(Vec<SetBreakpointsArguments>),
    Initialized,
}

impl Default for UninitializedState {
    fn default() -> Self {
        UninitializedState::Uninitialized(Vec::new())
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
