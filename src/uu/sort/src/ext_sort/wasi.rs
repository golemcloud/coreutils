// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! WASI single-threaded sort: read all input into memory, sort, and output.
//! Threads are not available on WASI, so we bypass the chunked/threaded path.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::iter;

use itertools::Itertools;
use uucore::error::{UError, UResult, USimpleError};

use crate::chunks::{self, Chunk};
use crate::tmp_dir::TmpDirWrapper;
use crate::{GlobalSettings, SortError, compare_by, open, print_sorted, sort_by};
use crate::{Line, Output};

/// The most input sort holds in memory here, where it has no external merge: 64 MiB, and 2
/// million lines, whose bookkeeping costs more than short lines themselves.
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_INPUT_LINES: usize = 2_000_000;

fn input_too_large() -> Box<dyn UError> {
    USimpleError::new(
        2,
        "input over 64 MiB or 2000000 lines is unsupported in bash-tool",
    )
}

/// A read error naming the input it came from.
#[derive(Debug)]
struct ReadFailed(std::path::PathBuf, std::io::Error);

impl std::fmt::Display for ReadFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use uucore::display::Quotable;
        write!(
            f,
            "read failed: {}: {}",
            self.0.maybe_quote(),
            uucore::error::strip_errno(&self.1)
        )
    }
}

impl std::error::Error for ReadFailed {}

/// `reader`, whose read errors name `path` as GNU sort reports them (`read failed: PATH: …`).
pub fn named_reader(path: &OsStr, reader: Box<dyn Read + Send>) -> Box<dyn Read + Send> {
    struct Named(std::path::PathBuf, Box<dyn Read + Send>);
    impl Read for Named {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1.read(buf).map_err(|error| {
                // WASI reports reading a directory as a bad descriptor; GNU sort reports it as
                // the directory it is.
                let is_dir = self.0.as_os_str() != crate::STDIN_FILE
                    && std::fs::metadata(&self.0).is_ok_and(|m| m.is_dir());
                let error = if is_dir {
                    std::io::Error::new(std::io::ErrorKind::IsADirectory, "Is a directory")
                } else {
                    error
                };
                std::io::Error::other(ReadFailed(self.0.clone(), error))
            })
        }
    }
    Box::new(Named(std::path::PathBuf::from(path), reader))
}

/// What one sort holds in memory from all of its inputs together, counted against the limits.
#[derive(Default)]
struct Held {
    bytes: usize,
    lines: usize,
}

/// Appends all of `reader` to `input`, failing rather than holding more than the limits allow
/// together with what `held` already counts.
fn read_bounded(
    mut reader: impl Read,
    input: &mut Vec<u8>,
    separator: u8,
    held: &mut Held,
) -> UResult<()> {
    let start = input.len();
    let room = MAX_INPUT_BYTES.saturating_sub(held.bytes) as u64;
    if let Err(error) = reader.by_ref().take(room + 1).read_to_end(input) {
        // A read error that names its input is reported as GNU sort words it.
        if error
            .get_ref()
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<ReadFailed>)
        {
            return Err(USimpleError::new(2, error.to_string()));
        }
        return Err(error.into());
    }
    let read = &input[start..];
    held.bytes += read.len();
    // A last line without a separator is a line too.
    held.lines += memchr::memchr_iter(separator, read).count()
        + usize::from(read.last().is_some_and(|byte| *byte != separator));
    if held.bytes > MAX_INPUT_BYTES || held.lines > MAX_INPUT_LINES {
        return Err(input_too_large());
    }
    Ok(())
}

/// Read one input whole and split it into lines, or `None` for an empty input.
fn read_whole(path: &OsStr, settings: &GlobalSettings, held: &mut Held) -> UResult<Option<Chunk>> {
    let mut input = Vec::new();
    let reader = named_reader(path, open(path)?);
    read_bounded(reader, &mut input, settings.line_ending.into(), held)?;
    if input.is_empty() {
        return Ok(None);
    }
    let separator = settings.line_ending.into();
    Chunk::try_new(input, |buffer| {
        Ok::<_, Box<dyn UError>>(chunks::parse_into_chunk(buffer, separator, settings))
    })
    .map(Some)
}

/// `sort -c`/`-C` without the reader thread the threaded check uses: read the input whole and
/// compare each line with the one before it, reporting the first out of order.
pub fn check_input(path: &OsStr, settings: &GlobalSettings) -> UResult<()> {
    // With `-u` a line must sort strictly after the one before it.
    let max_allowed_cmp = if settings.unique {
        Ordering::Less
    } else {
        Ordering::Equal
    };
    let Some(chunk) = read_whole(path, settings, &mut Held::default())? else {
        return Ok(());
    };
    for (index, (a, b)) in chunk.lines().iter().tuple_windows().enumerate() {
        if compare_by(a, b, settings, chunk.line_data(), chunk.line_data()) > max_allowed_cmp {
            return Err(SortError::Disorder {
                file: path.to_owned(),
                line_number: index + 2,
                line: String::from_utf8_lossy(b.line).into_owned(),
                silent: settings.check_silent,
            }
            .into());
        }
    }
    Ok(())
}

/// `sort -m` without reader threads: read each input whole, then repeatedly take the least head
/// line, the earliest input's on a tie, as the threaded merger does. Every input is read before
/// any output is written, so an output file that is also an input needs no copy.
pub fn merge_inputs(files: &[OsString], settings: &GlobalSettings, output: Output) -> UResult<()> {
    let mut inputs = Vec::with_capacity(files.len());
    let mut held = Held::default();
    for file in files {
        inputs.extend(read_whole(file, settings, &mut held)?);
    }
    let mut next = vec![0; inputs.len()];
    let merged = iter::from_fn(|| {
        let mut least: Option<(usize, &Line<'_>)> = None;
        for (i, input) in inputs.iter().enumerate() {
            let Some(line) = input.lines().get(next[i]) else {
                continue;
            };
            let take = least.is_none_or(|(j, current)| {
                let order = compare_by(
                    line,
                    current,
                    settings,
                    input.line_data(),
                    inputs[j].line_data(),
                );
                order == Ordering::Less
            });
            if take {
                least = Some((i, line));
            }
        }
        let (i, line) = least?;
        next[i] += 1;
        Some((i, line))
    });
    if settings.unique {
        let unique = merged.dedup_by(|(i, a), (j, b)| {
            let order = compare_by(
                a,
                b,
                settings,
                inputs[*i].line_data(),
                inputs[*j].line_data(),
            );
            order == Ordering::Equal
        });
        print_sorted(unique.map(|(_, line)| line), settings, output)
    } else {
        print_sorted(merged.map(|(_, line)| line), settings, output)
    }
}

/// Sort files by reading all input into memory, sorting in a single thread, and outputting directly.
pub fn ext_sort(
    files: &mut impl Iterator<Item = UResult<Box<dyn Read + Send>>>,
    settings: &GlobalSettings,
    output: Output,
    _tmp_dir: &mut TmpDirWrapper,
) -> UResult<()> {
    let separator = settings.line_ending.into();
    // Read all input into memory at once, within the limits: WASI has no threads for the
    // chunked, merging path.
    let mut input = Vec::new();
    let mut held = Held::default();
    for file in files {
        // A file's own last line still ends at that file's EOF, even without a trailing
        // separator: GNU sort never splices one file's unterminated tail onto the next file's
        // head. Since every file here gets flattened into one buffer before it is split into
        // lines, force that boundary in the byte stream itself, before this file's own bytes
        // (not after -- read_bounded appends straight into the shared, bounds-checked `input`,
        // so there's no separate per-file buffer left to inspect afterward). Inserting it
        // unconditionally before every file but the first is equivalent to only inserting it
        // before the next *non-empty* file: an empty file leaves `input` already ending with
        // the separator this just added, so the check before the following file is a no-op.
        if !input.is_empty() && input.last() != Some(&separator) {
            // This ends a line `held` already counts.
            input.push(separator);
            held.bytes += 1;
        }
        read_bounded(file?, &mut input, separator, &mut held)?;
    }

    if input.is_empty() {
        // empty files are sorted to empty like in coreutils
        print_sorted(iter::empty::<&Line<'_>>(), settings, output)?;
        return Ok(());
    }

    let mut chunk = Chunk::try_new(input, |buffer| {
        Ok::<_, Box<dyn uucore::error::UError>>(chunks::parse_into_chunk(
            buffer, separator, settings,
        ))
    })?;
    chunk.with_dependent_mut(|_, contents| {
        sort_by(&mut contents.lines, settings, &contents.line_data);
    });
    if settings.unique {
        print_sorted(
            chunk.lines().iter().dedup_by(|a, b| {
                compare_by(a, b, settings, chunk.line_data(), chunk.line_data()) == Ordering::Equal
            }),
            settings,
            output,
        )?;
    } else {
        print_sorted(chunk.lines().iter(), settings, output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Held, MAX_INPUT_BYTES, MAX_INPUT_LINES, read_bounded};

    #[test]
    fn inputs_share_the_byte_limit() {
        let half = vec![b'a'; MAX_INPUT_BYTES / 2 + 1];
        let mut held = Held::default();
        read_bounded(&half[..], &mut Vec::new(), b'\n', &mut held).unwrap();
        assert!(read_bounded(&half[..], &mut Vec::new(), b'\n', &mut held).is_err());
    }

    #[test]
    fn inputs_share_the_line_limit() {
        let half = vec![b'\n'; MAX_INPUT_LINES / 2 + 1];
        let mut held = Held::default();
        read_bounded(&half[..], &mut Vec::new(), b'\n', &mut held).unwrap();
        assert!(read_bounded(&half[..], &mut Vec::new(), b'\n', &mut held).is_err());
    }

    #[test]
    fn a_last_line_without_a_separator_counts_toward_the_line_limit() {
        for separator in *b"\n\0" {
            let mut half = vec![separator; MAX_INPUT_LINES / 2];
            half.push(b'x');
            let mut held = Held::default();
            read_bounded(&half[..], &mut Vec::new(), separator, &mut held).unwrap();
            assert_eq!(held.lines, MAX_INPUT_LINES / 2 + 1);
            assert!(read_bounded(&half[..], &mut Vec::new(), separator, &mut held).is_err());
        }
    }

    #[test]
    fn empty_and_terminated_inputs_count_only_their_lines() {
        let full = vec![b'\0'; MAX_INPUT_LINES];
        let mut held = Held::default();
        read_bounded(&b""[..], &mut Vec::new(), b'\0', &mut held).unwrap();
        read_bounded(&full[..], &mut Vec::new(), b'\0', &mut held).unwrap();
        read_bounded(&b""[..], &mut Vec::new(), b'\0', &mut held).unwrap();
        assert_eq!(held.lines, MAX_INPUT_LINES);
    }
}
