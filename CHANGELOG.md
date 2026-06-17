# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `MaildirEntryCopy` and `MaildirEntryMove` now mint a fresh unique name and preserve flags ([#1]).

  Both previously built the destination filename from the raw source basename (`{id}:2,`), which (1) reused the source unique name verbatim — carrying folder-specific metadata baked in by other tools, notably mbsync's `,U=<uid>` infix (valid only in the source folder), into the destination where it corrupts sync state and can silently overwrite a same-named entry — and (2) dropped the source flags. Copy and move now follow the same delivery convention as `MaildirEntryStore` (shared `mint_id` from time / pid / hostname) and carry the source flags into the target `:2,<flags>` suffix.

[#1]: https://github.com/pimalaya/io-maildir/issues/1

## [0.1.0] - 2026-06-05

### Added

- Added the `MaildirCoroutine` trait mirroring `core::ops::Coroutine`.

  Composed of `Yield` and `Return` associated types plus a two-variant `MaildirCoroutineState<Y, R>` (`Yielded(Y)` / `Complete(R)`). Every coroutine picks the shared `MaildirYield` enum (filesystem `Wants*` requests plus the three delivery inputs `WantsTime` / `WantsPid` / `WantsHostname`) and is fed back via the matching `MaildirReply` enum.

- Added the `maildir_try!` macro: coroutine equivalent of `?`.

  Advances one inner resume step, re-yields intermediate `Yielded(y)` (via `Into`), and short-circuits on `Complete(Err(_))`.

- Added I/O-free `MaildirCreate` coroutine.

  Creates `root`, `cur`, `new`, `tmp` in lexicographic order.

- Added I/O-free `MaildirDelete` coroutine.

  Recursively removes a Maildir.

- Added I/O-free `MaildirRename` coroutine.

  Renames a Maildir within its parent directory.

- Added I/O-free `MaildirList` coroutine.

  Walks every valid Maildir under a root; `MaildirList::new(root).maildirpp(true)` switches to the Maildir++ flat-dotted-siblings layout.

- Added I/O-free `MaildirEntryStore` coroutine.

  Follows the Maildir delivery protocol: writes to `/tmp` first, then atomically renames into `/cur` or `/new`, producing IDs of the shape `secs.#counter.M<nanos>P<pid>.<host>`.

- Added I/O-free `MaildirEntryGet` coroutine.

  Reads a single entry by ID and validates it against the on-disk filename.

- Added I/O-free `MaildirEntryList` coroutine.

  Scans both `/new` and `/cur` and returns every confirmed entry.

- Added I/O-free `MaildirEntryCopy` and `MaildirEntryMove` coroutines.

  Propagate an entry across Maildirs.

- Added I/O-free `MaildirEntryLocate` coroutine.

  Finds an entry file by ID across `cur`, `new` and `tmp`.

- Added I/O-free `MaildirFlagsAdd`, `MaildirFlagsRemove` and `MaildirFlagsSet` coroutines.

  Each rewrites the `:2,<flags>` suffix on the entry filename in place; custom keywords round-trip via optional `X-Keywords` / `X-Label` headers gated by per-client `keywords_header` / `strip_headers` switches.

- Added I/O-free `DovecotLoad` and `DovecotStore` coroutines.

  Read / write the `dovecot-keywords` slot table mapping `a..z` letters to user-defined keyword strings, gated by the per-client `dovecot_keywords` switch.

- Added the `FsPath` / `MaildirPath` split with `MaildirStore` as the translator.

  `FsPath` is the literal `/`-separated filesystem path; `MaildirPath` is the logical mailbox hierarchy. `MaildirStore { root: FsPath, maildirpp: bool }` resolves logical names to fs paths (`Foo/Bar` → `<root>/Foo/Bar` in fs layout, `<root>/.Foo.Bar` in Maildir++).

- Added the `client` cargo feature (default) enabling `MaildirClient`.

  Standard, blocking client backed by `std::fs` that drives any standard-Yield coroutine to completion, with high-level helpers (`create_maildir`, `delete_maildir`, `rename_maildir`, `load_maildir`) taking logical mailbox names.

- Added the `parser` cargo feature (default).

  Pulls in `mail-parser` to expose `MaildirFullEntry`, an entry paired with its parsed headers.

- Added the `serde` cargo feature (default).

  Forwards `serde` support to `mail-parser` so parsed entries can be serialized.

- Added a `# Example` `rust,no_run` block at the top of every coroutine module.

  Drives the coroutine through `MaildirClient::run` so the snippet stays self-contained and `cargo test --doc` compiles it.

[unreleased]: https://github.com/pimalaya/io-maildir/compare/v0.1.0..HEAD
[0.1.0]: https://github.com/pimalaya/io-maildir/compare/root..v0.1.0
