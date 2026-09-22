//! Typed, diagnostic errors for the filesystem seam.
//!
//! The same discipline as [`VmError`](crate::vm::VmError) and
//! [`ProcessError`](crate::process::ProcessError): every variant names the operation and the
//! values it failed with (Global Constraint 7), and there is deliberately no catch-all
//! `Other(String)`.
//!
//! # Why the *kind* is a type of ours rather than an errno
//!
//! The caller that turns a failure into the guest's `errno` is `omni-android`'s adapter, and the
//! errno numbering it must use is the **guest's** — Linux arm64's, which `omni-bionic::errno`
//! owns and which is not this host's. `omni-platform` sits below both crates and cannot depend on
//! either, so it reports a classified [`FsErrorKind`] and the adapter maps it. That keeps exactly
//! one table of guest errno numbers in the workspace instead of two that can drift.
//!
//! The classification itself is [`std::io::ErrorKind`] and nothing else: portable standard
//! library, one implementation, no `cfg`. A host error `ErrorKind` does not classify arrives as
//! [`FsErrorKind::Other`] carrying its raw OS code in the message, and the adapter refuses such a
//! call **by name** rather than inventing an errno for it — an unclassified failure reported as a
//! specific errno is the plausible-wrong-answer shape this project keeps being bitten by.

use std::path::Path;

/// Result alias for every operation on the filesystem seam.
pub type FsResult<T> = Result<T, FsError>;

/// What kind of failure the host reported, in terms a guest `errno` can be derived from.
///
/// Deliberately small. Every variant is one this seam's operations can actually produce, and each
/// one maps onto exactly one Linux errno in the adapter above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsErrorKind {
    /// The path does not exist — `ENOENT`.
    NotFound,
    /// The host refused access — `EACCES`.
    PermissionDenied,
    /// The path exists and the call required that it did not — `EEXIST`.
    AlreadyExists,
    /// A path component that had to be a directory is not one — `ENOTDIR`.
    NotADirectory,
    /// The path is a directory and the operation is not defined on one — `EISDIR`.
    IsADirectory,
    /// `rmdir` on a directory that still has entries — `ENOTEMPTY`.
    DirectoryNotEmpty,
    /// The arguments do not form a valid call — `EINVAL`.
    InvalidInput,
    /// The volume is full — `ENOSPC`.
    StorageFull,
    /// The file is larger than the operation can express — `EFBIG`.
    FileTooLarge,
    /// This instance already has as many descriptors open as it may — `EMFILE`.
    TooManyOpenFiles,
    /// The volume is mounted read-only — `EROFS`.
    ReadOnlyFilesystem,
    /// The descriptor is not open in this instance — `EBADF`.
    BadDescriptor,
    /// A path, or one component of it, is longer than the guest's own limit — `ENAMETOOLONG`.
    NameTooLong,
    /// The call would have had to wait — `EAGAIN`, which on Linux is `EWOULDBLOCK`.
    ///
    /// **Produced by pipes and by nothing else here**, because a pipe is the only thing on this
    /// seam whose readiness depends on another descriptor. It is reported for a *blocking*
    /// descriptor as well as a non-blocking one: this seam never waits, and the caller decides
    /// what a blocking descriptor does about it (see [`pipe`](super::pipe)).
    WouldBlock,
    /// A write to a pipe whose every read end is closed — `EPIPE`.
    ///
    /// On a device this also raises `SIGPIPE`. There is no signal delivery in this runtime
    /// (D24), so the errno is the whole of what the guest receives.
    BrokenPipe,
    /// A socket operation on a descriptor that is open and is not a socket — `ENOTSOCK`.
    ///
    /// **Decided by this seam rather than reported by the host**, and it is the one kind here
    /// that no `std::io::ErrorKind` can produce: the descriptor table knows what each number is,
    /// so a `connect` on a regular file is a fact this layer can state before any host call is
    /// made. Added with [`Entry::Socket`](super::Filesystem::socket_at) in M6; every other kind
    /// predates sockets existing at all.
    NotASocket,
    /// A seek on a descriptor with no position in it -- a pipe, a socket, an eventfd, a standard
    /// stream -- `ESPIPE`. Decided by this seam from the descriptor's kind, as [`NotASocket`] is.
    ///
    /// [`NotASocket`]: FsErrorKind::NotASocket
    NotSeekable,
    /// `epoll_ctl` on a descriptor that cannot be polled -- a regular file or a directory --
    /// `EPERM`. Decided by this seam from the descriptor's kind.
    NotPollable,
    /// The host reported a failure [`std::io::ErrorKind`] does not classify.
    ///
    /// **Not mapped to an errno by the caller.** See the module documentation: a call that fails
    /// this way is refused by name, because the alternative is answering a specific errno for a
    /// failure nobody identified.
    Other,
}

impl FsErrorKind {
    /// A short name for the kind, for a message that has to say what happened.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FsErrorKind::NotFound => "not found",
            FsErrorKind::PermissionDenied => "permission denied",
            FsErrorKind::AlreadyExists => "already exists",
            FsErrorKind::NotADirectory => "not a directory",
            FsErrorKind::IsADirectory => "is a directory",
            FsErrorKind::DirectoryNotEmpty => "directory not empty",
            FsErrorKind::InvalidInput => "invalid argument",
            FsErrorKind::StorageFull => "no space left on device",
            FsErrorKind::FileTooLarge => "file too large",
            FsErrorKind::TooManyOpenFiles => "too many open files",
            FsErrorKind::ReadOnlyFilesystem => "read-only filesystem",
            FsErrorKind::BadDescriptor => "bad file descriptor",
            FsErrorKind::NameTooLong => "file name too long",
            FsErrorKind::WouldBlock => "resource temporarily unavailable",
            FsErrorKind::BrokenPipe => "broken pipe",
            FsErrorKind::NotASocket => "not a socket",
            FsErrorKind::NotSeekable => "illegal seek",
            FsErrorKind::NotPollable => "operation not permitted",
            FsErrorKind::Other => "an unclassified host error",
        }
    }

    /// Classify a host [`std::io::Error`].
    ///
    /// `ErrorKind` is `#[non_exhaustive]`, so the catch-all is required rather than lazy — and it
    /// is the honest arm: a kind this match does not name is one nobody has decided an errno for.
    #[must_use]
    pub fn classify(error: &std::io::Error) -> FsErrorKind {
        use std::io::ErrorKind as K;
        match error.kind() {
            K::NotFound => FsErrorKind::NotFound,
            K::PermissionDenied => FsErrorKind::PermissionDenied,
            K::AlreadyExists => FsErrorKind::AlreadyExists,
            K::NotADirectory => FsErrorKind::NotADirectory,
            K::IsADirectory => FsErrorKind::IsADirectory,
            K::DirectoryNotEmpty => FsErrorKind::DirectoryNotEmpty,
            K::InvalidInput | K::InvalidData => FsErrorKind::InvalidInput,
            K::StorageFull => FsErrorKind::StorageFull,
            K::FileTooLarge => FsErrorKind::FileTooLarge,
            K::ReadOnlyFilesystem => FsErrorKind::ReadOnlyFilesystem,
            K::WouldBlock => FsErrorKind::WouldBlock,
            K::BrokenPipe => FsErrorKind::BrokenPipe,
            _ => FsErrorKind::Other,
        }
    }
}

/// Everything that can go wrong on the filesystem seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FsError {
    /// This backend does not implement the operation.
    ///
    /// Returned by the Linux and macOS backends, which are structural only, and by the two
    /// operations that need a target-specific call rather than one portable `std` one. `intended`
    /// names the POSIX call the implementation is expected to make.
    #[error(
        "filesystem operation `{operation}` is not implemented on {platform}: the intended \
         implementation is `{intended}`, and omni-platform's {platform} backend is structural \
         only and has never been run (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"pread"`.
        operation: &'static str,
        /// The POSIX call the implementation is meant to make, e.g. `"pread(2)"`.
        intended: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// The host refused the operation, classified into something an errno can be derived from.
    #[error("`{operation}` on `{path}`: {kind} ({detail})", kind = kind.as_str())]
    Io {
        /// The seam operation that was called.
        operation: &'static str,
        /// The guest path or descriptor the operation named.
        path: String,
        /// What kind of failure it was.
        kind: FsErrorKind,
        /// The host's own message, kept because an unclassified failure has nothing else.
        detail: String,
    },

    /// The guest path could not be resolved inside this instance's root.
    ///
    /// **This is the confinement refusal**, and it is deliberately not an `Io` failure with a
    /// plausible errno: a traversal attempt, a Windows device name, a drive-relative path or a
    /// symlink that leaves the root are each a *hostile* input rather than a file that happens
    /// not to exist, and reporting one as `ENOENT` would hide it in the ordinary noise of a guest
    /// probing for files.
    #[error(
        "`{operation}` refused the guest path `{path}`: {why}. Every path this guest can name is \
         resolved inside the instance's own root, because a guest that could open an arbitrary \
         host file would break the isolation several instances in one process depend on (D6: the \
         APK under test is cheat-injected, and the executor is treated as hostile)"
    )]
    Confined {
        /// The seam operation that was called.
        operation: &'static str,
        /// The guest path, rendered losslessly enough to be read.
        path: String,
        /// Which rule refused it.
        why: String,
    },

    /// The operation is well-formed and this layer will not carry it out.
    ///
    /// A flag whose guarantee cannot be met, a directory too large to snapshot, a filename that
    /// does not fit the guest's `struct dirent`. Never a plausible substitute.
    #[error("`{operation}` on `{path}`: {why}")]
    Refused {
        /// The seam operation that was called.
        operation: &'static str,
        /// The guest path or descriptor the operation named.
        path: String,
        /// Why it cannot be carried out.
        why: String,
    },
}

impl FsError {
    /// True when this failure means "this backend has no implementation".
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, FsError::Unsupported { .. })
    }

    /// The classified host failure, when there is one.
    ///
    /// `None` for every variant the adapter must refuse by name rather than turn into an errno.
    #[must_use]
    pub fn kind(&self) -> Option<FsErrorKind> {
        match self {
            FsError::Io { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Build an [`FsError::Io`] from a host error.
    pub(super) fn io(operation: &'static str, path: impl AsRef<Path>, error: &std::io::Error) -> FsError {
        FsError::Io {
            operation,
            path: path.as_ref().display().to_string(),
            kind: FsErrorKind::classify(error),
            detail: error.to_string(),
        }
    }

    /// Build an [`FsError::Io`] with a kind this seam decided rather than the host.
    pub(super) fn kinded(
        operation: &'static str,
        path: impl Into<String>,
        kind: FsErrorKind,
        detail: impl Into<String>,
    ) -> FsError {
        FsError::Io { operation, path: path.into(), kind, detail: detail.into() }
    }

    /// Build an [`FsError::Refused`].
    pub(super) fn refused(
        operation: &'static str,
        path: impl Into<String>,
        why: impl Into<String>,
    ) -> FsError {
        FsError::Refused { operation, path: path.into(), why: why.into() }
    }

    /// Build an [`FsError::Confined`].
    pub(super) fn confined(
        operation: &'static str,
        path: impl Into<String>,
        why: impl Into<String>,
    ) -> FsError {
        FsError::Confined { operation, path: path.into(), why: why.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind has a distinct name, and `Other` is the one that carries no errno.
    #[test]
    fn the_kinds_are_distinct_and_only_one_of_them_refuses_to_be_an_errno() {
        let all = [
            FsErrorKind::NotFound,
            FsErrorKind::PermissionDenied,
            FsErrorKind::AlreadyExists,
            FsErrorKind::NotADirectory,
            FsErrorKind::IsADirectory,
            FsErrorKind::DirectoryNotEmpty,
            FsErrorKind::InvalidInput,
            FsErrorKind::StorageFull,
            FsErrorKind::FileTooLarge,
            FsErrorKind::TooManyOpenFiles,
            FsErrorKind::ReadOnlyFilesystem,
            FsErrorKind::BadDescriptor,
            FsErrorKind::NameTooLong,
            FsErrorKind::WouldBlock,
            FsErrorKind::BrokenPipe,
            FsErrorKind::Other,
        ];
        let mut names: Vec<&str> = all.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "two kinds render the same way");
    }

    /// The classification is the host's, and a failure the host does not classify stays `Other`.
    #[test]
    fn a_host_error_is_classified_and_an_unknown_one_is_not_guessed_at() {
        let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        assert_eq!(FsErrorKind::classify(&not_found), FsErrorKind::NotFound);
        let weird = std::io::Error::other("a failure with no ErrorKind of its own");
        assert_eq!(
            FsErrorKind::classify(&weird),
            FsErrorKind::Other,
            "an unclassified failure must not be given a specific errno's kind"
        );
    }

    /// Only `Io` carries a kind; the two refusals deliberately do not.
    #[test]
    fn only_a_classified_host_failure_offers_a_kind() {
        let io = FsError::kinded("read", "fd 3", FsErrorKind::BadDescriptor, "no such descriptor");
        assert_eq!(io.kind(), Some(FsErrorKind::BadDescriptor));
        assert_eq!(FsError::refused("open", "/x", "why").kind(), None);
        assert_eq!(FsError::confined("open", "/x", "why").kind(), None);
        let unsupported = FsError::Unsupported {
            operation: "pread",
            intended: "pread(2)",
            platform: "linux",
        };
        assert_eq!(unsupported.kind(), None);
        assert!(unsupported.is_unsupported());
        assert!(unsupported.to_string().contains("pread(2)"), "{unsupported}");
    }
}
