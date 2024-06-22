use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::{env, ptr};

use red4rs::{CName, Class, Function, StackFrame};

use crate::SourceFileInfo;

#[derive(Debug)]
pub struct DebugControl {
    functions: RwLock<FunctionMapping>,
    breakpoints: RwLock<Breakpoints>,
    last_break_frame: AtomicPtr<StackFrame>,
    step_mode: AtomicU8,
}

impl DebugControl {
    #[inline]
    pub const fn new() -> Self {
        Self {
            functions: RwLock::new(FunctionMapping::new()),
            breakpoints: RwLock::new(Breakpoints::new()),
            last_break_frame: AtomicPtr::new(std::ptr::null_mut()),
            step_mode: AtomicU8::new(StepMode::None as u8),
        }
    }

    #[inline]
    pub fn functions(&self) -> RwLockReadGuard<'_, FunctionMapping> {
        self.functions.read().unwrap()
    }

    #[inline]
    pub fn functions_mut(&self) -> RwLockWriteGuard<'_, FunctionMapping> {
        self.functions.write().unwrap()
    }

    #[inline]
    pub fn breakpoints(&self) -> RwLockReadGuard<'_, Breakpoints> {
        self.breakpoints.read().unwrap()
    }

    #[inline]
    pub fn breakpoints_mut(&self) -> RwLockWriteGuard<'_, Breakpoints> {
        self.breakpoints.write().unwrap()
    }

    pub fn get_step_mode(&self) -> StepMode {
        StepMode::try_from(self.step_mode.load(Ordering::Relaxed)).unwrap_or_default()
    }

    #[inline]
    pub fn set_step_mode(&self, mode: StepMode) {
        self.step_mode.store(mode as u8, Ordering::Relaxed);
    }

    #[inline]
    pub fn get_last_break_frame(&self) -> Option<&StackFrame> {
        unsafe { self.last_break_frame.load(Ordering::Relaxed).as_ref() }
    }

    #[inline]
    pub fn set_last_break_frame(&self, frame: *const StackFrame) {
        self.last_break_frame.store(frame as _, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.set_step_mode(StepMode::None);
        self.last_break_frame
            .store(ptr::null_mut(), Ordering::Relaxed);
        self.breakpoints_mut().clear();
    }
}

#[derive(Debug)]
pub struct Breakpoints {
    breakpoints: BTreeMap<BreakpointKey, Breakpoint>,
}

impl Breakpoints {
    #[inline]
    pub const fn new() -> Self {
        Self {
            breakpoints: BTreeMap::new(),
        }
    }

    #[inline]
    pub fn get(&self, key: &BreakpointKey) -> Option<Breakpoint> {
        self.breakpoints.get(key).cloned()
    }

    #[inline]
    pub fn add(&mut self, key: BreakpointKey) {
        self.breakpoints.insert(key, Breakpoint {});
    }

    pub fn unregister_by_fn(&mut self, func: FunctionId) {
        let range = BreakpointKey::new(func, 0)..=BreakpointKey::new(func, u16::MAX);
        let keys = self
            .breakpoints
            .range(range)
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();

        for key in keys {
            self.breakpoints.remove(&key);
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.breakpoints.clear();
    }
}

#[derive(Debug)]
pub struct FunctionMapping {
    source_to_fn: BTreeMap<SourceRef, FunctionId>,
    fn_to_source: BTreeMap<FunctionId, SourceRef>,
    index_to_path: BTreeMap<u32, Arc<str>>,
}

impl FunctionMapping {
    #[inline]
    pub const fn new() -> Self {
        Self {
            source_to_fn: BTreeMap::new(),
            fn_to_source: BTreeMap::new(),
            index_to_path: BTreeMap::new(),
        }
    }

    #[inline]
    pub fn get_by_loc(&self, path: &str, line: u32) -> Option<FunctionId> {
        self.get_fns_preceding_line(path, line).next()
    }

    #[inline]
    pub fn get_by_file<'a>(&'a self, path: &'a str) -> impl Iterator<Item = FunctionId> + 'a {
        self.get_fns_preceding_line(path, u32::MAX)
    }

    #[inline]
    pub fn get(&self, key: FunctionId) -> Option<&SourceRef> {
        self.fn_to_source.get(&key)
    }

    pub fn add(&mut self, id: FunctionId, source: &SourceFileInfo, line: u32) {
        let file_path = self.add_source(source);
        let source = SourceRef::new_unchecked(file_path, line);

        if let Some(old) = self.fn_to_source.insert(id, source.clone()) {
            self.source_to_fn.remove(&old).unwrap();
        }

        if let Some(old) = self.source_to_fn.insert(source, id) {
            self.fn_to_source.remove(&old).unwrap();
        }
    }

    fn add_source(&mut self, source: &SourceFileInfo) -> Arc<str> {
        self.index_to_path
            .entry(source.index)
            .or_insert_with(|| {
                let path = Path::new(source.path.as_ref());
                let mut path: Arc<str> = if path.extension() != Some(OsStr::new("reds")) {
                    // if it's not redscript we point to the redmod scripts
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
                        Some(res.to_string_lossy().into())
                    })()
                    .unwrap_or_else(|| source.path.as_ref().into())
                } else {
                    source.path.as_ref().into()
                };
                if path.is_ascii() {
                    Arc::get_mut(&mut path).unwrap().make_ascii_lowercase();
                    path
                } else {
                    path.to_lowercase().into()
                }
            })
            .clone()
    }

    fn get_fns_preceding_line<'a>(
        &'a self,
        path: &'a str,
        line: u32,
    ) -> impl Iterator<Item = FunctionId> + 'a {
        let key = SourceRef::new(path, line);
        self.source_to_fn
            .range(key..)
            .take_while(|(k, _)| k.file_path.eq_ignore_ascii_case(path))
            .map(|(_, v)| *v)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct BreakpointKey {
    func: FunctionId,
    line: u16,
}

impl BreakpointKey {
    #[inline]
    pub fn new(func: FunctionId, line: u16) -> Self {
        Self { func, line }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct SourceRef {
    file_path: Arc<str>,
    line: Reverse<u32>,
}

impl SourceRef {
    #[inline]
    pub fn new(file_path: impl AsRef<str>, line: u32) -> Self {
        Self::new_unchecked(file_path.as_ref().to_lowercase().into(), line)
    }

    #[inline]
    fn new_unchecked(file_path: Arc<str>, line: u32) -> Self {
        Self {
            file_path,
            line: Reverse(line),
        }
    }

    #[inline]
    pub fn file_path(&self) -> &str {
        &self.file_path
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct FunctionId {
    parent: CName,
    name: CName,
}

impl FunctionId {
    pub fn from_func(func: &Function) -> Self {
        Self {
            parent: func.parent().map_or_else(CName::undefined, Class::name),
            name: func.name(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Breakpoint {}

#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum StepMode {
    #[default]
    None = 0,
    StepOut = 1,
    StepOver = 2,
    StepIn = 3,
}

impl TryFrom<u8> for StepMode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::StepOut),
            2 => Ok(Self::StepOver),
            3 => Ok(Self::StepIn),
            _ => Err(()),
        }
    }
}
