//! A guest process and its tasks. Completed in Task 10.
use std::sync::Arc;

use crate::guest::GuestMem;
use crate::syscall::{Refusals, Table};

pub struct Process {
    pub mem: GuestMem,
    pub table: Table,
    pub refusals: Refusals,
}

pub struct Task {
    pub tid: i32,
    pub process: Arc<Process>,
    /// The `SVC`'s address and `X30` at the current syscall, for refusal records.
    pub pc: u64,
    pub lr: u64,
}
