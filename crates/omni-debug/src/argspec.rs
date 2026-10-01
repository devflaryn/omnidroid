//! The argument model for structured calls (C2 of the RE workbench).
//!
//! A raw [`Session::call_function`](crate::Session::call_function) takes `&[u64]` — the caller must
//! allocate buffers and read them back by hand. An `arg_spec` lets the agent above describe each
//! parameter declaratively: scalars pass through, input buffers are allocated and filled, output
//! buffers are allocated zeroed and read back after the call. This is what makes buffer-taking
//! functions first-class for both tracing and differential testing.

/// One argument to a structured call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// An integer/pointer passed by value in the next `X` register.
    Scalar(u64),
    /// An input buffer: allocated in guest memory, filled with these bytes; its guest address is
    /// passed. An empty buffer still yields a valid (1-byte) pointer, since the length is normally
    /// passed as a separate [`Arg::Scalar`].
    InBuffer(Vec<u8>),
    /// An output buffer of this length: allocated zeroed, its address passed, and its contents read
    /// back after the call. Zero length is a defined error.
    OutBuffer(usize),
    /// A buffer that is both input and output: allocated and filled, its address passed, and its
    /// contents read back after the call. Empty is a defined error.
    InOutBuffer(Vec<u8>),
}

/// The result of a structured call.
#[derive(Debug, Clone)]
pub struct CallSpecResult {
    /// `X0` at return.
    pub ret: u64,
    /// `X1` at return.
    pub ret1: u64,
    /// Guest instructions executed.
    pub instructions: u64,
    /// `(argument index, bytes)` for each [`Arg::OutBuffer`]/[`Arg::InOutBuffer`], read back after
    /// the call, in argument order.
    pub out_buffers: Vec<(usize, Vec<u8>)>,
}
