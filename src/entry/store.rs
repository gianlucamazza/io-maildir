//! I/O-free coroutine storing an entry in a Maildir per the
//! delivery protocol (write to `/tmp`, atomic rename into target).
//!
//! # Example
//!
//! ```rust,no_run
//! use io_maildir::{
//!     client::MaildirClient,
//!     entry::store::{MaildirEntryStore, MaildirEntryStoreOutput},
//!     flag::types::MaildirFlags,
//!     maildir::types::MaildirSubdir,
//! };
//!
//! let client = MaildirClient::new("/path/to/root");
//! let maildir = client.load_maildir("inbox").unwrap();
//! let contents = b"From: alice@example.com\r\nSubject: Hello\r\n\r\nHello!\r\n".to_vec();
//!
//! let coroutine = MaildirEntryStore::new(maildir, MaildirSubdir::New, MaildirFlags::default(), contents);
//! let MaildirEntryStoreOutput { id, path } = client.run(coroutine).unwrap();
//!
//! println!("stored {id} at {path}");
//! ```

use core::{fmt, mem};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};

use log::trace;
use thiserror::Error;

use crate::{
    coroutine::*,
    entry::types::{INFORMATIONAL_SUFFIX_SEPARATOR, mint_id},
    flag::types::MaildirFlags,
    maildir::types::{Maildir, MaildirSubdir},
    path::FsPath,
};

/// Failure causes during a [`MaildirEntryStore`] step.
#[derive(Clone, Debug, Error)]
pub enum MaildirEntryStoreError {
    #[error("Maildir message store failed: unexpected arg {0:?}")]
    UnexpectedArg(Option<MaildirReply>),
}

/// Successful output of [`MaildirEntryStore`].
#[derive(Clone, Debug)]
pub struct MaildirEntryStoreOutput {
    pub id: String,
    pub path: FsPath,
}

/// Stores a new entry in a Maildir via the tmp → cur/new rename
/// dance.
#[derive(Debug)]
pub struct MaildirEntryStore {
    maildir: Maildir,
    subdir: MaildirSubdir,
    flags: MaildirFlags,
    contents: Vec<u8>,
    state: State,
}

impl MaildirEntryStore {
    pub fn new(
        maildir: Maildir,
        subdir: MaildirSubdir,
        flags: MaildirFlags,
        contents: Vec<u8>,
    ) -> Self {
        Self {
            maildir,
            subdir,
            flags,
            contents,
            state: State::Start,
        }
    }
}

impl MaildirCoroutine for MaildirEntryStore {
    type Yield = MaildirYield;
    type Return = Result<MaildirEntryStoreOutput, MaildirEntryStoreError>;

    fn resume(
        &mut self,
        arg: Option<MaildirReply>,
    ) -> MaildirCoroutineState<Self::Yield, Self::Return> {
        trace!("entry store: {}", self.state);

        match (&mut self.state, arg) {
            (State::Start, None) => {
                self.state = State::AwaitTime;
                MaildirCoroutineState::Yielded(MaildirYield::WantsTime)
            }
            (State::AwaitTime, Some(MaildirReply::Time { secs, nanos })) => {
                self.state = State::AwaitPid { secs, nanos };
                MaildirCoroutineState::Yielded(MaildirYield::WantsPid)
            }
            (State::AwaitPid { secs, nanos }, Some(MaildirReply::Pid(pid))) => {
                let secs = *secs;
                let nanos = *nanos;
                self.state = State::AwaitHostname { secs, nanos, pid };
                MaildirCoroutineState::Yielded(MaildirYield::WantsHostname)
            }
            (State::AwaitHostname { secs, nanos, pid }, Some(MaildirReply::Hostname(hostname))) => {
                let id = mint_id(*secs, *nanos, *pid, &hostname);

                let mut final_name = id.clone();
                if matches!(self.subdir, MaildirSubdir::Cur) {
                    final_name.push(INFORMATIONAL_SUFFIX_SEPARATOR);
                    final_name.push_str("2,");
                    final_name.push_str(&self.flags.to_string());
                }

                let tmp_path = self.maildir.tmp().join(&id);
                let final_path = self.maildir.subdir(&self.subdir).join(&final_name);

                let contents = mem::take(&mut self.contents);
                let files = BTreeMap::from_iter([(tmp_path.clone(), contents)]);
                self.state = State::AwaitCreateTmp {
                    tmp_path,
                    final_path,
                    id,
                };
                MaildirCoroutineState::Yielded(MaildirYield::WantsFileCreate(files))
            }
            (
                State::AwaitCreateTmp {
                    tmp_path,
                    final_path,
                    id,
                },
                Some(MaildirReply::FileCreate),
            ) => {
                let tmp_path = mem::take(tmp_path);
                let final_path = mem::take(final_path);
                let id = mem::take(id);
                let pairs = vec![(tmp_path, final_path.clone())];
                self.state = State::AwaitRename { final_path, id };
                MaildirCoroutineState::Yielded(MaildirYield::WantsRename(pairs))
            }
            (State::AwaitRename { final_path, id }, Some(MaildirReply::Rename)) => {
                let final_path = mem::take(final_path);
                let id = mem::take(id);
                MaildirCoroutineState::Complete(Ok(MaildirEntryStoreOutput {
                    id,
                    path: final_path,
                }))
            }
            (_, arg) => {
                let err = MaildirEntryStoreError::UnexpectedArg(arg);
                MaildirCoroutineState::Complete(Err(err))
            }
        }
    }
}

#[derive(Debug)]
enum State {
    Start,
    AwaitTime,
    AwaitPid {
        secs: u64,
        nanos: u32,
    },
    AwaitHostname {
        secs: u64,
        nanos: u32,
        pid: u32,
    },
    AwaitCreateTmp {
        tmp_path: FsPath,
        final_path: FsPath,
        id: String,
    },
    AwaitRename {
        final_path: FsPath,
        id: String,
    },
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start => f.write_str("start"),
            Self::AwaitTime => f.write_str("await time reply"),
            Self::AwaitPid { .. } => f.write_str("await pid reply"),
            Self::AwaitHostname { .. } => f.write_str("await hostname reply"),
            Self::AwaitCreateTmp { .. } => f.write_str("await tmp create reply"),
            Self::AwaitRename { .. } => f.write_str("await rename reply"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maildir() -> Maildir {
        Maildir::from_path("root")
    }

    #[test]
    fn full_flow_returns_ok() {
        let mut cor = MaildirEntryStore::new(
            maildir(),
            MaildirSubdir::New,
            MaildirFlags::default(),
            b"hello".to_vec(),
        );

        match cor.resume(None) {
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
            MaildirCoroutineState::Yielded(MaildirYield::WantsFileCreate(_)) => {}
            state => panic!("expected WantsFileCreate, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::FileCreate)) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsRename(_)) => {}
            state => panic!("expected WantsRename, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::Rename)) {
            MaildirCoroutineState::Complete(Ok(out)) => {
                assert!(out.id.starts_with("1."));
                assert!(out.path.as_str().contains("/new/"));
            }
            state => panic!("expected Complete(Ok), got {state:?}"),
        }
    }

    #[test]
    fn unexpected_reply_returns_error() {
        let mut cor = MaildirEntryStore::new(
            maildir(),
            MaildirSubdir::New,
            MaildirFlags::default(),
            b"hello".to_vec(),
        );

        match cor.resume(None) {
            MaildirCoroutineState::Yielded(MaildirYield::WantsTime) => {}
            state => panic!("expected WantsTime, got {state:?}"),
        }
        match cor.resume(Some(MaildirReply::DirCreate)) {
            MaildirCoroutineState::Complete(Err(MaildirEntryStoreError::UnexpectedArg(_))) => {}
            state => panic!("expected Complete(Err), got {state:?}"),
        }
    }
}
