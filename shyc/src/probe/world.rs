//! The declaration world a probe image carries to the host.
//!
//! Every `declared`, `implements`, `require_impl`, and `try_provide_impl` call
//! sends its arguments and the round world to the host. Literal and computed
//! names use the same [`WorldSnapshot`] matcher, including when the scheduler
//! decides whether a suspended generator can run again. This module is the one
//! format both ends speak: the compiler writes the blob into the image, the
//! driver reads it back before answering a request.
//!
//! Every number is a big-endian `u32`; every string is its byte length
//! followed by the UTF-8 bytes:
//!
//! ```text
//! version = 1
//! names:   count, then per name: len, bytes
//! impls:   count, then per pair: owner len, owner, trait len, trait
//! generic: count, then per impl:
//!            trait len, trait, base len, base,
//!            params count, then per parameter:
//!              bounds count, then per bound: len, bytes
//! ```
//!
//! A blob is self-describing, so the driver needs no other state. It is
//! rejected whole -- never half read -- when it is over [`MAX_BYTES`],
//! declares a count the remaining bytes cannot satisfy, carries a non-UTF-8
//! string, or leaves bytes after the last list.

use crate::parser::{GenericImplSnapshot, ImplPair, WorldSnapshot};

/// The one format this module writes and reads.
pub const VERSION: u32 = 1;

/// A wider blob is a compiler limit, not something the driver should accept:
/// the probe image and the host mailbox are both bounded, so a world that does
/// not fit is reported instead of being truncated.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Smallest number of bytes one entry of a list can occupy: the length prefix
/// of a string, or the count of a nested list. A decoded count is checked
/// against what the rest of the blob can hold before anything is allocated.
const STRING_COST: usize = 4;
const IMPL_COST: usize = 2 * STRING_COST;
const GENERIC_COST: usize = 2 * STRING_COST + 4;

/// Serializes one snapshot into the blob a probe image carries.
pub(super) fn encode_world(world: &WorldSnapshot) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    put_word(&mut bytes, VERSION);
    put_word(&mut bytes, count(world.names.len(), "names")?);
    for name in &world.names {
        put_string(&mut bytes, name)?;
    }
    put_word(&mut bytes, count(world.impls.len(), "impls")?);
    for pair in &world.impls {
        put_string(&mut bytes, &pair.owner)?;
        put_string(&mut bytes, &pair.trait_name)?;
    }
    put_word(
        &mut bytes,
        count(world.generic_impls.len(), "generic impls")?,
    );
    for generic in &world.generic_impls {
        put_string(&mut bytes, &generic.trait_name)?;
        put_string(&mut bytes, &generic.base)?;
        put_word(&mut bytes, count(generic.bounds.len(), "parameters")?);
        for bounds in &generic.bounds {
            put_word(&mut bytes, count(bounds.len(), "bounds")?);
            for bound in bounds {
                put_string(&mut bytes, bound)?;
            }
        }
    }
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "world snapshot is {} byte(s), over the {MAX_BYTES} byte limit",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Reads back what [`encode_world`] wrote.
pub fn decode_world(bytes: &[u8]) -> Result<WorldSnapshot, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "world snapshot is {} byte(s), over the {MAX_BYTES} byte limit",
            bytes.len()
        ));
    }
    let mut reader = Reader { bytes, at: 0 };
    let version = reader.word("version")?;
    if version != VERSION {
        return Err(format!(
            "world snapshot has version {version}, expected {VERSION}"
        ));
    }
    let name_count = reader.count("names", STRING_COST)?;
    let mut names = Vec::with_capacity(name_count);
    for _ in 0..name_count {
        names.push(reader.string("name")?);
    }
    let impl_count = reader.count("impls", IMPL_COST)?;
    let mut impls = Vec::with_capacity(impl_count);
    for _ in 0..impl_count {
        let owner = reader.string("impl owner")?;
        let trait_name = reader.string("impl trait")?;
        impls.push(ImplPair { owner, trait_name });
    }
    let generic_count = reader.count("generic impls", GENERIC_COST)?;
    let mut generic_impls = Vec::with_capacity(generic_count);
    for _ in 0..generic_count {
        let trait_name = reader.string("generic impl trait")?;
        let base = reader.string("generic impl base")?;
        let parameter_count = reader.count("parameters", 4)?;
        let mut bounds = Vec::with_capacity(parameter_count);
        for _ in 0..parameter_count {
            let bound_count = reader.count("bounds", STRING_COST)?;
            let mut parameter = Vec::with_capacity(bound_count);
            for _ in 0..bound_count {
                parameter.push(reader.string("bound")?);
            }
            bounds.push(parameter);
        }
        generic_impls.push(GenericImplSnapshot {
            trait_name,
            base,
            bounds,
        });
    }
    if reader.at != bytes.len() {
        return Err(format!(
            "world snapshot has {} trailing byte(s)",
            bytes.len() - reader.at
        ));
    }
    Ok(WorldSnapshot {
        names,
        impls,
        generic_impls,
    })
}

fn put_word(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_string(bytes: &mut Vec<u8>, text: &str) -> Result<(), String> {
    put_word(bytes, count(text.len(), "string bytes")?);
    bytes.extend_from_slice(text.as_bytes());
    Ok(())
}

/// One length as the format can spell it.
fn count(length: usize, what: &str) -> Result<u32, String> {
    u32::try_from(length)
        .map_err(|_| format!("world snapshot {what}: {length} does not fit in a u32"))
}

/// A cursor over one blob. Every read is bounds checked against the bytes that
/// are actually there, so a damaged blob reports where it ended early.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn word(&mut self, what: &str) -> Result<u32, String> {
        let Some(word) = self.bytes.get(self.at..self.at + 4) else {
            return Err(format!("world snapshot ends before {what}"));
        };
        self.at += 4;
        Ok(u32::from_be_bytes(
            word.try_into().expect("a slice of four bytes is a word"),
        ))
    }

    /// One count, rejected when the bytes left cannot hold that many entries of
    /// at least `least` bytes each. The vectors built from a count are
    /// therefore bounded by the blob size, not by the number a damaged blob
    /// happens to spell.
    fn count(&mut self, what: &str, least: usize) -> Result<usize, String> {
        let count = self.word(what)? as usize;
        if count > self.remaining() / least {
            return Err(format!(
                "world snapshot counts {count} {what}, but only {} byte(s) follow",
                self.remaining()
            ));
        }
        Ok(count)
    }

    fn string(&mut self, what: &str) -> Result<String, String> {
        let length = self.word(what)? as usize;
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| format!("world snapshot {what} ends past the address space"))?;
        let Some(bytes) = self.bytes.get(self.at..end) else {
            return Err(format!(
                "world snapshot ends inside {what}: {length} byte(s) needed"
            ));
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|_| format!("world snapshot {what} is not valid UTF-8"))?
            .to_string();
        self.at = end;
        Ok(text)
    }
}
