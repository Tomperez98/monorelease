//! Cross-platform process-group lifecycle management.
//!
//! [`process_wrap`] owns the Unix process-group and Windows Job Object
//! details. Mono keeps this adapter deliberately small so the runner depends
//! only on one process-tree interface on every target.

use std::io;
use std::process::{Command, ExitStatus};

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};

use crate::runner::{ChildProcess, ProcessExit};

/// A child process whose process tree can be terminated reliably.
///
/// `process_wrap` uses a Unix process group or a Windows Job Object depending
/// on the wrapper selected below. The runner owns this value on one worker
/// thread at a time, and all process-tree operations remain behind the
/// [`ChildProcess`] seam.
pub(crate) struct ManagedChild {
    child: Box<dyn ChildWrapper>,
}

impl ManagedChild {
    /// Create a child in an OS-backed process group.
    pub(crate) fn spawn(command: Command) -> io::Result<Self> {
        let mut command = CommandWrap::from(command);
        configure_process_tree(&mut command);
        Ok(Self {
            child: command.spawn()?,
        })
    }

    /// Non-blocking check for exit status.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Send a hard termination signal to the complete process group or Job
    /// Object without waiting for the direct child.
    fn terminate_tree(&mut self) -> io::Result<()> {
        match self.child.start_kill() {
            Ok(()) => Ok(()),
            Err(_error) if self.child.try_wait()?.is_some() => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Reap the child after termination or normal completion.
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child
            .stdout()
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child
            .stderr()
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>)
    }
}

#[cfg(unix)]
fn configure_process_tree(command: &mut CommandWrap) {
    command.wrap(ProcessGroup::leader());
}

#[cfg(windows)]
fn configure_process_tree(command: &mut CommandWrap) {
    command.wrap(JobObject);
}

impl ChildProcess for ManagedChild {
    fn stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.take_stdout()
    }

    fn stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.take_stderr()
    }

    fn try_wait(&mut self) -> io::Result<Option<ProcessExit>> {
        self.try_wait().map(|status| {
            status.map(|status| ProcessExit {
                code: status.code(),
                success: status.success(),
            })
        })
    }

    fn wait(&mut self) -> io::Result<ProcessExit> {
        self.wait().map(|status| ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }

    fn terminate_tree(&mut self) -> io::Result<()> {
        self.terminate_tree()
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // A running child must not survive its owner. ChildWrapper::kill
        // applies to the complete process group or Job Object and waits for
        // the wrapper's child state to settle.
        if self.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
    }
}
