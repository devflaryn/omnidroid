//! `epoll(7)`: an interest list that is a descriptor.
//!
//! # Why this is in the descriptor table
//!
//! [`pipe`](super::pipe)'s and [`eventfd`](super::eventfd)'s argument, one kind further on: an
//! epoll instance is created, closed and numbered exactly like every other descriptor, and a guest
//! that closes one and opens a file expects the lowest free number back. A second allocator would
//! hand out a number `close` would answer about the wrong object.
//!
//! # What this holds, and what it deliberately does not
//!
//! **The interest list and nothing else**: which descriptors are watched, for which event bits,
//! and the 64-bit `epoll_data_t` the guest attached to each. The seam never interprets the event
//! bits or the data -- they are the guest's, written back to it by the adapter -- and it computes
//! no readiness of its own. Readiness is each member's own ([`super::Filesystem::readiness`]), and
//! waiting for a change is the adapter's policy for the same reason [`pipe`](super::pipe) gives:
//! how long a guest may block is decided where D16's step budgets live, not here.
//!
//! # The rules that are the kernel's and are enforced here
//!
//! | `epoll_ctl` | answer |
//! |---|---|
//! | `ADD` of a descriptor already in the list | `EEXIST` |
//! | `MOD` or `DEL` of one that is not | `ENOENT` |
//! | a regular file or a directory | `EPERM` -- neither supports polling, so neither can be watched |
//! | the epoll descriptor itself | `EINVAL` |
//!
//! And one that is easy to miss: **closing a descriptor removes it from every interest list.** On
//! Linux a registration belongs to the open file description and goes when the last descriptor
//! for it closes; this runtime has no `dup`, so a descriptor *is* its description, and without
//! the removal a number reused by `open` would inherit a watch on a different object.
//!
//! Nested epoll -- an epoll descriptor in another's list, or one handed to `poll` -- is refused by
//! name. Linux allows it; no run has reached it, and its readiness is a question about every
//! member at once that this seam does not answer.

use std::collections::BTreeMap;

/// `EPOLL_CLOEXEC`, which is `O_CLOEXEC`. Recorded on the descriptor for `fcntl(F_GETFD)` and
/// otherwise inert, as `EFD_CLOEXEC` is: there is no `exec` in this runtime for it to act on.
pub const EPOLL_CLOEXEC: i32 = 0o2_000_000;

/// One watched descriptor: what the guest asked about, and what it asked to be handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpollMember {
    /// The `events` word from the guest's `struct epoll_event`, uninterpreted here.
    pub events: u32,
    /// The `epoll_data_t` union, as the 64 bits the guest stored.
    pub data: u64,
}

/// `epoll_ctl`'s three operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpollOp {
    /// `EPOLL_CTL_ADD` (1).
    Add,
    /// `EPOLL_CTL_DEL` (2).
    Delete,
    /// `EPOLL_CTL_MOD` (3).
    Modify,
}

/// One epoll instance's interest list.
#[derive(Debug, Default)]
pub struct EpollSet {
    members: BTreeMap<i32, EpollMember>,
}

impl EpollSet {
    /// Every watched descriptor, in ascending order.
    #[must_use]
    pub fn members(&self) -> Vec<(i32, EpollMember)> {
        self.members.iter().map(|(fd, member)| (*fd, *member)).collect()
    }

    /// `true` when `fd` was being watched and no longer is.
    pub(super) fn forget(&mut self, fd: i32) -> bool {
        self.members.remove(&fd).is_some()
    }

    /// Apply one `epoll_ctl` to the list, answering what the kernel would.
    ///
    /// Only the list's own rules are checked here -- `EEXIST` and `ENOENT`. What `fd` *is* (open,
    /// pollable, not this instance) is the table's knowledge and is checked by the caller first.
    pub(super) fn apply(
        &mut self,
        op: EpollOp,
        fd: i32,
        member: EpollMember,
    ) -> Result<(), EpollRefusal> {
        match op {
            EpollOp::Add => {
                if self.members.contains_key(&fd) {
                    return Err(EpollRefusal::AlreadyWatched);
                }
                self.members.insert(fd, member);
            }
            EpollOp::Modify => match self.members.get_mut(&fd) {
                Some(held) => *held = member,
                None => return Err(EpollRefusal::NotWatched),
            },
            EpollOp::Delete => {
                if self.members.remove(&fd).is_none() {
                    return Err(EpollRefusal::NotWatched);
                }
            }
        }
        Ok(())
    }
}

/// Why [`EpollSet::apply`] declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EpollRefusal {
    /// `ADD` of a descriptor already in the list: `EEXIST`.
    AlreadyWatched,
    /// `MOD` or `DEL` of one that is not: `ENOENT`.
    NotWatched,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(events: u32, data: u64) -> EpollMember {
        EpollMember { events, data }
    }

    /// The list's two refusals, and that a modification replaces both halves of a member.
    #[test]
    fn add_modify_delete_follow_the_kernels_rules() {
        let mut set = EpollSet::default();
        assert_eq!(set.apply(EpollOp::Add, 7, member(1, 0xAA)), Ok(()));
        assert_eq!(set.apply(EpollOp::Add, 7, member(4, 0xBB)), Err(EpollRefusal::AlreadyWatched));
        assert_eq!(set.members(), vec![(7, member(1, 0xAA))], "a refused ADD changed nothing");
        assert_eq!(set.apply(EpollOp::Modify, 7, member(5, 0xCC)), Ok(()));
        assert_eq!(set.members(), vec![(7, member(5, 0xCC))], "MOD replaces events and data");
        assert_eq!(set.apply(EpollOp::Modify, 8, member(1, 0)), Err(EpollRefusal::NotWatched));
        assert_eq!(set.apply(EpollOp::Delete, 7, member(0, 0)), Ok(()));
        assert_eq!(set.apply(EpollOp::Delete, 7, member(0, 0)), Err(EpollRefusal::NotWatched));
        assert!(set.members().is_empty());
    }
}
