use std::collections::BTreeMap;

use prost::encoding::{WireType, decode_key, decode_varint};

use crate::frame::FrameError;

const MAX_DEPTH: usize = 32;
const MAX_REPEATED_FIELDS: usize = 1024;

struct Field {
    tag: u32,
    child: Option<usize>,
    repeated: bool,
    packed_varints: bool,
}

#[derive(Default)]
struct MessageBudget {
    repeated: usize,
    singular: BTreeMap<u32, MessageBudget>,
}

include!(concat!(env!("OUT_DIR"), "/wire_preflight.rs"));

pub(crate) fn envelope(bytes: &[u8]) -> Result<(), FrameError> {
    validate(bytes, ENVELOPE, 1, &mut MessageBudget::default())
}

pub(crate) fn render_header(bytes: &[u8]) -> Result<(), FrameError> {
    validate(bytes, RENDER_HEADER, 1, &mut MessageBudget::default())
}

fn validate(
    mut bytes: &[u8],
    shape: usize,
    depth: usize,
    budget: &mut MessageBudget,
) -> Result<(), FrameError> {
    if depth > MAX_DEPTH {
        return Err(FrameError::NestingLimit);
    }

    while !bytes.is_empty() {
        let (tag, wire_type) = decode_key(&mut bytes)?;
        let field = SHAPES[shape].iter().find(|field| field.tag == tag);
        if field.is_some_and(|field| field.repeated) && wire_type != WireType::LengthDelimited {
            count_element(&mut budget.repeated)?;
        }

        match wire_type {
            WireType::Varint => {
                decode_varint(&mut bytes)?;
            }
            WireType::ThirtyTwoBit => {
                take(&mut bytes, 4)?;
            }
            WireType::SixtyFourBit => {
                take(&mut bytes, 8)?;
            }
            WireType::LengthDelimited => {
                let len = usize::try_from(decode_varint(&mut bytes)?)
                    .map_err(|_| FrameError::MalformedField)?;
                let value = take(&mut bytes, len)?;
                if let Some(field) = field
                    && let Some(child) = field.child
                {
                    let mut independent = MessageBudget::default();
                    let child_budget = if field.repeated {
                        &mut independent
                    } else {
                        // The decoder can merge repeated occurrences of a singular child.
                        budget.singular.entry(tag).or_default()
                    };
                    validate(value, child, depth + 1, child_budget)?;
                }
                if field.is_some_and(|field| field.packed_varints) {
                    count_packed_varints(value, &mut budget.repeated)?;
                } else if field.is_some_and(|field| field.repeated) {
                    count_element(&mut budget.repeated)?;
                }
            }
            // This proto3 contract has no deprecated groups, including in unknown fields.
            WireType::StartGroup | WireType::EndGroup => return Err(FrameError::MalformedField),
        }
    }
    Ok(())
}

fn count_packed_varints(mut bytes: &[u8], repeated: &mut usize) -> Result<(), FrameError> {
    while !bytes.is_empty() {
        decode_varint(&mut bytes)?;
        count_element(repeated)?;
    }
    Ok(())
}

fn count_element(repeated: &mut usize) -> Result<(), FrameError> {
    *repeated += 1;
    if *repeated > MAX_REPEATED_FIELDS {
        return Err(FrameError::CollectionLimit);
    }
    Ok(())
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], FrameError> {
    let (field, rest) = bytes
        .split_at_checked(len)
        .ok_or(FrameError::MalformedField)?;
    *bytes = rest;
    Ok(field)
}
