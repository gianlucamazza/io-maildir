//! I/O-free coroutine moving a Maildir entry to another Maildir.
//!
//! Moving relocates the entry into `target` under a freshly minted Maildir
//! unique name (time / pid / hostname, like [`MaildirEntryStore`]) instead of
//! reusing the source basename. Reusing the source name would carry
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
//! use io_maildir::{client::MaildirClient, entry::r#move::MaildirEntryMove};
//!
//! let client = MaildirClient::new("/path/to/root");
//! let source = client.load_maildir("inbox").unwrap();
//! let target = client.load_maildir("archive").unwrap();
//!
//! let coroutine = MaildirEntryMove::new("1700000000.1.M0P1.host", source, target, None);
//! client.run(coroutine).unwrap();
//! ```

use core::fmt;

use alloc::string::ToString;

use log::trace;
use thiserror::Error;

use crate::{
    coroutine::*,
    entry::locate::*,
    entry::types::{INFORMATIONAL_SUFFIX_SEPARATOR, mint_id},
    flag::types::MaildirFlags,
    maildir::types::{Maildir, MaildirSubdir},
    maildir_try,
    path::FsPath,
};

/// Failure causes during a [`MaildirEntryMove`] step.
#[derive(Clone, Debug, Error)]
pub enum MaildirEntryMoveError {
    #[error("Maildir message move failed: unexpected arg {0:?}")]
    UnexpectedArg(Option<MaildirReply>),

    #[error(transparent)]
    Locate(#[from] MaildirEntryLocateError),
}

/// Moves a Maildir entry into `target`; `None` target_subdir keeps
/// the source subdir.
#[derive(Debug)]
pub struct MaildirEntryMove {
    target: Maildir,
    target_subdir: Option<MaildirSubdir>,
    state: State,
}

impl MaildirEntryMove {
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

impl MaildirCoroutine for MaildirEntryMove {
    type Yield = MaildirYield;
    type Return = Result<(), MaildirEntryMoveError>;

    fn resume(
        &mut self,
        arg: Option<MaildirReply>,
    ) -> MaildirCoroutineState<Self::Yield, Self::Return> {
        trace!("entry move: {}", self.state);

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
                self.state = State::AwaitRename;
                MaildirCoroutineState::Yielded(MaildirYield::WantsRename(pairs))
            }
            (State::AwaitRename, Some(MaildirReply::Rename)) => {
                MaildirCoroutineState::Complete(Ok(()))
            }
            (_, arg) => {
                let err = MaildirEntryMoveError::UnexpectedArg(arg);
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
    AwaitRename,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locate(_) => f.write_str("locate source"),
            Self::AwaitTime { .. } => f.write_str("await time reply"),
            Self::AwaitPid { .. } => f.write_str("await pid reply"),
            Self::AwaitHostname { .. } => f.write_str("await hostname reply"),
            Self::AwaitRename => f.write_str("await rename reply"),
        }
    }
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
    use alloc::string::String;

    use super::*;

    fn source() -> Maildir {
        Maildir::from_path("root/src")
    }

    fn target() -> Maildir {
        Maildir::from_path("root/dst")
    }

    #[test]
    fn unexpected_reply_returns_error() {
        let mut cor = MaildirEntryMove::new("abc", source(), target(), None);
        let _ = expect_wants_file_exists(&mut cor);

        let err = expect_complete_err(&mut cor, Some(MaildirReply::DirCreate));
        assert!(matches!(err, MaildirEntryMoveError::Locate(_)));
    }

    #[test]
    fn cur_move_mints_fresh_id_and_preserves_flags() {
        // Source carries mbsync's `,U=999` infix and `FS` flags.
        let mut cor = MaildirEntryMove::new("1700000000.abc.host,U=999", source(), target(), None);

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
            MaildirCoroutineState::Yielded(MaildirYield::WantsRename(pairs)) => {
                let (from, to) = &pairs[0];
                assert_eq!(
                    from,
                    &FsPath::from("root/src/cur/1700000000.abc.host,U=999:2,FS")
                );
                let to = to.as_str();
                assert!(to.starts_with("root/dst/cur/1."), "got {to}");
                assert!(!to.contains(",U=999"), "carried foreign UID: {to}");
                // Flags preserved; rendered in canonical (sorted) order.
                assert!(to.ends_with(":2,SF"), "flags not preserved: {to}");
            }
            state => panic!("expected WantsRename, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Rename)) {
            MaildirCoroutineState::Complete(Ok(())) => {}
            state => panic!("expected Complete(Ok), got {state:?}"),
        }
    }

    // --- utils

    fn expect_wants_file_exists(cor: &mut MaildirEntryMove) {
        match cor.resume(None) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsFileExists(_)) => {}
            state => panic!("expected WantsFileExists, got {state:?}"),
        }
    }

    fn expect_complete_err(
        cor: &mut MaildirEntryMove,
        arg: Option<MaildirReply>,
    ) -> MaildirEntryMoveError {
        match cor.resume(arg) {
            MaildirCoroutineState::Complete(Err(err)) => err,
            state => panic!("expected Complete(Err), got {state:?}"),
        }
    }
}
