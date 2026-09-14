//! The class of a runtime fault, named where the fault is detected.
//!
//! Oro used to have two internal spellings for "this operation failed": a bare
//! `String` whose exception class was reconstructed five thousand lines later by
//! substring-matching the English prose (`vm::classify_error`, plus a table each
//! in `net` and `json`), and a typed raise that named its class at the raise
//! site. The string channel was the older and by far the larger of the two, and
//! it was wrong in a way no amount of care in the table could fix:
//!
//! ```text
//! "abc".to_int()                        ->  ValueError
//! "timed out".to_int()                  ->  TimeoutError
//! "No such file or directory".to_int()  ->  FileNotFoundError
//! ```
//!
//! The messages those tables matched *interpolate values the program chose*, so
//! the class of a fault was decided by the contents of the data — and, through
//! `std/http.oro`, by the text of a path segment a client sent. The table's own
//! comment claimed "every message here is produced by this crate, so the
//! matching is reliable"; the three lines above are the counterexample.
//!
//! So the class travels. [`Exc`] is one byte, it is named by the code that
//! detects the fault, and nothing downstream ever has to guess. A fault whose
//! class is genuinely uncategorised says so, by naming [`Exc::RuntimeError`].
//!
//! Why an enum rather than the `&'static str` the old typed channel used
//! (`sched.rs`'s `raise("TimeoutError", …)`): the string is looked up in the
//! class registry with a panicking index, so a typo is a crash in a cold path
//! that may never be exercised, and `&'static str` is sixteen bytes where this
//! is one. [`RuntimeError`](crate::vm::RuntimeError) is carried through the
//! dispatch loop's return value, so its width is not free.

/// The exception class a fault raises. Every name here exists in the registry
/// built by [`crate::vm::exceptions::build_registry`]; that correspondence is
/// asserted by a test, so [`Exc::name`] can index the registry directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Exc {
    // The two roots.
    BaseException,
    Exception,
    SystemExit,
    // Named faults, in the registry's order.
    ImportError,
    ModuleNotFoundError,
    ValueError,
    TypeError,
    KeyError,
    IndexError,
    AttributeError,
    NameError,
    ZeroDivisionError,
    RuntimeError,
    RecursionError,
    EOFError,
    CommandError,
    ChannelClosed,
    OSError,
    FileNotFoundError,
    PermissionError,
    TimeoutError,
    ConnectionError,
    ConnectionRefusedError,
    ConnectionResetError,
    ConnectionAbortedError,
    BrokenPipeError,
}

impl Exc {
    /// The class's name, which is also its key in the VM's exception registry.
    pub const fn name(self) -> &'static str {
        match self {
            Exc::BaseException => "BaseException",
            Exc::Exception => "Exception",
            Exc::SystemExit => "SystemExit",
            Exc::ImportError => "ImportError",
            Exc::ModuleNotFoundError => "ModuleNotFoundError",
            Exc::ValueError => "ValueError",
            Exc::TypeError => "TypeError",
            Exc::KeyError => "KeyError",
            Exc::IndexError => "IndexError",
            Exc::AttributeError => "AttributeError",
            Exc::NameError => "NameError",
            Exc::ZeroDivisionError => "ZeroDivisionError",
            Exc::RuntimeError => "RuntimeError",
            Exc::RecursionError => "RecursionError",
            Exc::EOFError => "EOFError",
            Exc::CommandError => "CommandError",
            Exc::ChannelClosed => "ChannelClosed",
            Exc::OSError => "OSError",
            Exc::FileNotFoundError => "FileNotFoundError",
            Exc::PermissionError => "PermissionError",
            Exc::TimeoutError => "TimeoutError",
            Exc::ConnectionError => "ConnectionError",
            Exc::ConnectionRefusedError => "ConnectionRefusedError",
            Exc::ConnectionResetError => "ConnectionResetError",
            Exc::ConnectionAbortedError => "ConnectionAbortedError",
            Exc::BrokenPipeError => "BrokenPipeError",
        }
    }

    /// Every variant, for the test that checks each one is in the registry.
    pub const ALL: &'static [Exc] = &[
        Exc::BaseException,
        Exc::Exception,
        Exc::SystemExit,
        Exc::ImportError,
        Exc::ModuleNotFoundError,
        Exc::ValueError,
        Exc::TypeError,
        Exc::KeyError,
        Exc::IndexError,
        Exc::AttributeError,
        Exc::NameError,
        Exc::ZeroDivisionError,
        Exc::RuntimeError,
        Exc::RecursionError,
        Exc::EOFError,
        Exc::CommandError,
        Exc::ChannelClosed,
        Exc::OSError,
        Exc::FileNotFoundError,
        Exc::PermissionError,
        Exc::TimeoutError,
        Exc::ConnectionError,
        Exc::ConnectionRefusedError,
        Exc::ConnectionResetError,
        Exc::ConnectionAbortedError,
        Exc::BrokenPipeError,
    ];
}

/// The exception class for an `io::Error`, taken from its *kind* and never
/// from its text.
///
/// This is the one place a class is derived rather than written down, and the
/// derivation is from the datum the operating system reported — an
/// `ErrorKind`, which is an enum — rather than from a rendered sentence. The
/// message an `io::Error` carries is `strerror`'s, which is locale-dependent
/// and, once a path or a hostname is interpolated into it, partly chosen by
/// the program. Callers keep whatever message they were already producing;
/// only the class comes from here.
pub fn io_class(kind: std::io::ErrorKind) -> Exc {
    use std::io::ErrorKind::*;
    match kind {
        NotFound => Exc::FileNotFoundError,
        PermissionDenied => Exc::PermissionError,
        ConnectionRefused => Exc::ConnectionRefusedError,
        ConnectionReset => Exc::ConnectionResetError,
        ConnectionAborted => Exc::ConnectionAbortedError,
        BrokenPipe => Exc::BrokenPipeError,
        // `WouldBlock` reaching a caller means it escaped the readiness
        // machinery; answering with the timeout that was asked for beats
        // inventing an errno for it. See `net::err_msg`.
        TimedOut | WouldBlock => Exc::TimeoutError,
        // Everything else is a failed syscall, which is what `OSError` is.
        _ => Exc::OSError,
    }
}

/// A fault: the class it raises and the message it raises with.
///
/// This is the error half of [`VResult`](crate::value::VResult) — the signature
/// of every native builtin, every stream method and every module entry point.
/// It used to be a bare `String`; the class is the whole point of the change.
#[derive(Debug, Clone, PartialEq)]
pub struct VErr {
    pub class: Exc,
    pub message: String,
}

impl VErr {
    pub fn new(class: Exc, message: impl Into<String>) -> VErr {
        VErr {
            class,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The constructors. One per class a fault is actually raised with, named so
/// that a raise site reads as the sentence it is: `Err(type_error(…))`.
macro_rules! ctors {
    ($($fname:ident => $variant:ident),* $(,)?) => {
        $(
            #[doc = concat!("A fault that raises `", stringify!($variant), "`.")]
            pub fn $fname(message: impl Into<String>) -> VErr {
                VErr::new(Exc::$variant, message)
            }
        )*
    };
}

ctors! {
    value_error => ValueError,
    type_error => TypeError,
    key_error => KeyError,
    index_error => IndexError,
    attribute_error => AttributeError,
    name_error => NameError,
    zero_division_error => ZeroDivisionError,
    runtime_error => RuntimeError,
    recursion_error => RecursionError,
    eof_error => EOFError,
    command_error => CommandError,
    channel_closed => ChannelClosed,
    os_error => OSError,
    file_not_found_error => FileNotFoundError,
    permission_error => PermissionError,
    timeout_error => TimeoutError,
    connection_refused_error => ConnectionRefusedError,
    connection_reset_error => ConnectionResetError,
    connection_aborted_error => ConnectionAbortedError,
    broken_pipe_error => BrokenPipeError,
    import_error => ImportError,
    module_not_found_error => ModuleNotFoundError,
}
