//! A guest process and its tasks. Completed in Task 10.
use std::sync::Arc;

use crate::fd::FdTable;
use crate::guest::GuestMem;
use crate::mm::Mm;
use crate::syscall::{Refusals, Table};
use crate::vfs::Vfs;

pub struct Process {
    pub mem: GuestMem,
    pub table: Table,
    pub refusals: Refusals,
    pub vfs: Vfs,
    pub fds: FdTable,
    pub cwd: parking_lot::Mutex<Vec<u8>>,
    pub mm: Mm,
    scratch: u64,
}

impl Process {
    /// Serve one syscall: the handler's answer as the guest sees it in `x0`.
    pub fn syscall(&self, task: &mut Task, number: u64, args: [u64; 6]) -> u64 {
        match self.table.get(number) {
            Some(handler) => match handler(self, task, args) {
                Ok(v) => v,
                Err(e) => e.as_return(),
            },
            None => {
                self.refusals.record(crate::syscall::name_of(number).into_owned(), task.pc, task.lr);
                crate::errno::ENOSYS.as_return()
            }
        }
    }

    /// A process with no program, for handler tests: a 1 MiB RW scratch mapping.
    pub fn for_tests(vfs: Vfs, stdout: crate::fd::Output) -> Arc<Self> {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let scratch = space
            .map_anonymous(omni_mem::Placement::Anywhere { align: space.page_size() }, 1 << 20,
                omni_mem::Protection::ReadWrite, omni_mem::CommitPolicy::Lazy)
            .expect("scratch") as u64;
        let mut table = Table::new();
        crate::install_all(&mut table);
        Arc::new(Self {
            mem: GuestMem::new(Arc::clone(&space)),
            mm: Mm::new(space),
            table,
            refusals: Refusals::default(),
            vfs,
            fds: crate::fd::FdTable::standard(stdout.clone(), stdout),
            cwd: parking_lot::Mutex::new(b"/".to_vec()),
            scratch,
        })
    }

    pub fn scratch(&self) -> u64 {
        self.scratch
    }

    pub fn test_task(self: &Arc<Self>) -> Task {
        Task { tid: 1, process: Arc::clone(self), pc: 0, lr: 0 }
    }
}

pub struct Task {
    pub tid: i32,
    pub process: Arc<Process>,
    /// The `SVC`'s address and `X30` at the current syscall, for refusal records.
    pub pc: u64,
    pub lr: u64,
}
