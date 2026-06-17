//! I/O-free coroutine copying a Maildir entry to another Maildir.
//!
//! Copying is a fresh *delivery* into `target`: a brand-new Maildir unique
//! name is minted (time / pid / hostname, like [`MaildirEntryStore`]) instead
//! of reusing the source basename. Reusing the source name would carry
//! folder-specific metadata baked into it by other tools — e.g. mbsync's
//! `,U=<uid>` infix, valid only in the source folder — into the destination,
//! corrupting its sync state and risking a silent overwrite of a same-named
//! entry. The source flags are preserved.
//!
//! [`MaildirEntryStore`]: crate::entry::store::MaildirEntryStore
//!
//! # Example
//!
//! ```rust,no_run
//! use io_maildir::{client::MaildirClient, entry::copy::MaildirEntryCopy};
//!
//! let client = MaildirClient::new("/path/to/root");
//! let source = client.load_maildir("inbox").unwrap();
//! let target = client.load_maildir("archive").unwrap();
//!
//! let coroutine = MaildirEntryCopy::new("1700000000.1.M0P1.host", source, target, None);
//! client.run(coroutine).unwrap();
//! ```

use core::{
    fmt,
    sync::atomic::{AtomicU32, Ordering},
};

use alloc::string::{String, ToString};

use log::trace;
use thiserror::Error;

use crate::{
    coroutine::*,
    entry::locate::*,
    entry::types::INFORMATIONAL_SUFFIX_SEPARATOR,
    flag::types::MaildirFlags,
    maildir::types::{Maildir, MaildirSubdir},
    maildir_try,
    path::FsPath,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Failure causes during a [`MaildirEntryCopy`] step.
#[derive(Clone, Debug, Error)]
pub enum MaildirEntryCopyError {
    #[error("Maildir message copy failed: unexpected arg {0:?}")]
    UnexpectedArg(Option<MaildirReply>),

    #[error(transparent)]
    Locate(#[from] MaildirEntryLocateError),
}

/// Copies a Maildir entry into `target`; `None` target_subdir keeps
/// the source subdir.
#[derive(Debug)]
pub struct MaildirEntryCopy {
    target: Maildir,
    target_subdir: Option<MaildirSubdir>,
    state: State,
}

impl MaildirEntryCopy {
    pub fn new(
        id: impl ToString,
        source: Maildir,
        target: Maildir,
        target_subdir: Option<MaildirSubdir>,
    ) -> Self {
        Self {
            state: State::Locate(MaildirEntryLocate::new(source, id)),
            target,
            target_subdir,
        }
    }
}

impl MaildirCoroutine for MaildirEntryCopy {
    type Yield = MaildirYield;
    type Return = Result<(), MaildirEntryCopyError>;

    fn resume(
        &mut self,
        arg: Option<MaildirReply>,
    ) -> MaildirCoroutineState<Self::Yield, Self::Return> {
        trace!("entry copy: {}", self.state);

        match (&mut self.state, arg) {
            (State::Locate(c), arg) => {
                let out = maildir_try!(c, arg);
                let subdir = self.target_subdir.clone().unwrap_or(out.subdir);
                self.state = State::AwaitTime {
                    source: out.path,
                    subdir,
                    flags: out.flags,
                };
                MaildirCoroutineState::Yielded(MaildirYield::WantsTime)
            }
            (
                State::AwaitTime {
                    source,
                    subdir,
                    flags,
                },
                Some(MaildirReply::Time { secs, nanos }),
            ) => {
                self.state = State::AwaitPid {
                    source: core::mem::take(source),
                    subdir: subdir.clone(),
                    flags: core::mem::take(flags),
                    secs,
                    nanos,
                };
                MaildirCoroutineState::Yielded(MaildirYield::WantsPid)
            }
            (
                State::AwaitPid {
                    source,
                    subdir,
                    flags,
                    secs,
                    nanos,
                },
                Some(MaildirReply::Pid(pid)),
            ) => {
                self.state = State::AwaitHostname {
                    source: core::mem::take(source),
                    subdir: subdir.clone(),
                    flags: core::mem::take(flags),
                    secs: *secs,
                    nanos: *nanos,
                    pid,
                };
                MaildirCoroutineState::Yielded(MaildirYield::WantsHostname)
            }
            (
                State::AwaitHostname {
                    source,
                    subdir,
                    flags,
                    secs,
                    nanos,
                    pid,
                },
                Some(MaildirReply::Hostname(hostname)),
            ) => {
                let id = mint_id(*secs, *nanos, *pid, &hostname);
                let target = build_target_path(&self.target, subdir, &id, flags);
                let pairs = vec![(core::mem::take(source), target)];
                self.state = State::AwaitCopy;
                MaildirCoroutineState::Yielded(MaildirYield::WantsCopy(pairs))
            }
            (State::AwaitCopy, Some(MaildirReply::Copy)) => MaildirCoroutineState::Complete(Ok(())),
            (_, arg) => {
                let err = MaildirEntryCopyError::UnexpectedArg(arg);
                MaildirCoroutineState::Complete(Err(err))
            }
        }
    }
}

#[derive(Debug)]
enum State {
    Locate(MaildirEntryLocate),
    AwaitTime {
        source: FsPath,
        subdir: MaildirSubdir,
        flags: MaildirFlags,
    },
    AwaitPid {
        source: FsPath,
        subdir: MaildirSubdir,
        flags: MaildirFlags,
        secs: u64,
        nanos: u32,
    },
    AwaitHostname {
        source: FsPath,
        subdir: MaildirSubdir,
        flags: MaildirFlags,
        secs: u64,
        nanos: u32,
        pid: u32,
    },
    AwaitCopy,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locate(_) => f.write_str("locate source"),
            Self::AwaitTime { .. } => f.write_str("await time reply"),
            Self::AwaitPid { .. } => f.write_str("await pid reply"),
            Self::AwaitHostname { .. } => f.write_str("await hostname reply"),
            Self::AwaitCopy => f.write_str("await copy reply"),
        }
    }
}

/// Mints a fresh Maildir unique name, matching the delivery convention used by
/// [`MaildirEntryStore`](crate::entry::store::MaildirEntryStore).
fn mint_id(secs: u64, nanos: u32, pid: u32, hostname: &str) -> String {
    let counter = COUNTER.fetch_add(1, Ordering::AcqRel);
    format!("{secs}.#{counter:x}M{nanos}P{pid}.{hostname}")
}

fn build_target_path(
    target: &Maildir,
    subdir: &MaildirSubdir,
    id: &str,
    flags: &MaildirFlags,
) -> FsPath {
    match subdir {
        MaildirSubdir::Cur => {
            let name = format!("{id}{INFORMATIONAL_SUFFIX_SEPARATOR}2,{flags}");
            target.cur().join(&name)
        }
        MaildirSubdir::New => target.new().join(id),
        MaildirSubdir::Tmp => target.tmp().join(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Maildir {
        Maildir::from_path("root/src")
    }

    fn target() -> Maildir {
        Maildir::from_path("root/dst")
    }

    #[test]
    fn unexpected_reply_returns_error() {
        let mut cor = MaildirEntryCopy::new("abc", source(), target(), None);
        let _ = expect_wants_file_exists(&mut cor);

        let err = expect_complete_err(&mut cor, Some(MaildirReply::DirCreate));
        assert!(matches!(err, MaildirEntryCopyError::Locate(_)));
    }

    #[test]
    fn cur_copy_mints_fresh_id_and_preserves_flags() {
        // Source carries mbsync's `,U=999` infix and `FS` flags.
        let mut cor = MaildirEntryCopy::new("1700000000.abc.host,U=999", source(), target(), None);

        // Locate: probe new/tmp, miss, scan cur, find the entry.
        let _ = expect_wants_file_exists(&mut cor);
        let mut probe = alloc::collections::BTreeMap::new();
        probe.insert(
            FsPath::from("root/src/new/1700000000.abc.host,U=999"),
            false,
        );
        probe.insert(
            FsPath::from("root/src/tmp/1700000000.abc.host,U=999"),
            false,
        );
        match cor.resume(Some(MaildirReply::FileExists(probe))) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsDirRead(_)) => {}
            state => panic!("expected WantsDirRead, got {state:?}"),
        }
        let mut entries = alloc::collections::BTreeMap::new();
        let mut set = alloc::collections::BTreeSet::new();
        set.insert(FsPath::from("root/src/cur/1700000000.abc.host,U=999:2,FS"));
        entries.insert(FsPath::from("root/src/cur"), set);
        // Locate completes → first delivery step asks for time.
        match cor.resume(Some(MaildirReply::DirRead(entries))) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsTime) => {}
            state => panic!("expected WantsTime, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Time { secs: 1, nanos: 2 })) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsPid) => {}
            state => panic!("expected WantsPid, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Pid(3))) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsHostname) => {}
            state => panic!("expected WantsHostname, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Hostname(String::from("host")))) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsCopy(pairs)) => {
                let (from, to) = &pairs[0];
                assert_eq!(
                    from,
                    &FsPath::from("root/src/cur/1700000000.abc.host,U=999:2,FS")
                );
                let to = to.as_str();
                // Fresh id under target/cur, no `,U=999`, flags preserved.
                assert!(to.starts_with("root/dst/cur/1."), "got {to}");
                assert!(!to.contains(",U=999"), "carried foreign UID: {to}");
                // Flags preserved; rendered in canonical (sorted) order.
                assert!(to.ends_with(":2,SF"), "flags not preserved: {to}");
            }
            state => panic!("expected WantsCopy, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Copy)) {
            MaildirCoroutineState::Complete(Ok(())) => {}
            state => panic!("expected Complete(Ok), got {state:?}"),
        }
    }

    // --- utils

    fn expect_wants_file_exists(cor: &mut MaildirEntryCopy) {
        match cor.resume(None) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsFileExists(_)) => {}
            state => panic!("expected WantsFileExists, got {state:?}"),
        }
    }

    fn expect_complete_err(
        cor: &mut MaildirEntryCopy,
        arg: Option<MaildirReply>,
    ) -> MaildirEntryCopyError {
        match cor.resume(arg) {
            MaildirCoroutineState::Complete(Err(err)) => err,
            state => panic!("expected Complete(Err), got {state:?}"),
        }
    }
}
