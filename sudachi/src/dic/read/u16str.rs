/*
 *  Copyright (c) 2021 Works Applications Co., Ltd.
 *
 *  Licensed under the Apache License, Version 2.0 (the "License");
 *  you may not use this file except in compliance with the License.
 *  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *   Unless required by applicable law or agreed to in writing, software
 *  distributed under the License is distributed on an "AS IS" BASIS,
 *  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  See the License for the specific language governing permissions and
 *  limitations under the License.
 */

use crate::error::{SudachiNomError, SudachiNomResult};
use nom::number::complete::le_u8;

pub fn utf16_string_parser(input: &[u8]) -> SudachiNomResult<&[u8], String> {
    utf16_string_data(input).and_then(|(rest, data)| {
        if data.is_empty() {
            Ok((rest, String::new()))
        } else {
            match decode_utf16_le(data) {
                Some(result) => Ok((rest, result)),
                None => Err(nom::Err::Failure(SudachiNomError::Utf16String)),
            }
        }
    })
}

/// Decodes little-endian UTF-16 bytes into a String.
/// Returns None if the data contains unpaired surrogates.
///
/// Manual encoding loop: this is called for every string field of every
/// analyzed word, and it is much faster than `char::decode_utf16` + `String::push`.
fn decode_utf16_le(data: &[u8]) -> Option<String> {
    // BMP code unit -> up to 3 bytes, surrogate pair (2 units) -> 4 bytes
    let n_units = data.len() / 2;
    let mut result: Vec<u8> = Vec::with_capacity(n_units * 3);
    let mut out = result.as_mut_ptr();
    let mut written = 0usize;
    // SAFETY: the loop writes at most 3 bytes per code unit
    // (a surrogate pair consumes 2 units and writes 4 bytes), so it never
    // exceeds the reserved capacity of `n_units * 3` bytes.
    macro_rules! push {
        ($b:expr) => {
            unsafe {
                debug_assert!(written < n_units * 3);
                out.write($b);
                out = out.add(1);
            }
            written += 1;
        };
    }
    let mut i = 0;
    while i < n_units {
        let u = u16::from_le_bytes([data[2 * i], data[2 * i + 1]]);
        i += 1;
        if u < 0x80 {
            push!(u as u8);
        } else if u < 0x800 {
            push!(0xC0 | (u >> 6) as u8);
            push!(0x80 | (u & 0x3F) as u8);
        } else if !(0xD800..0xE000).contains(&u) {
            push!(0xE0 | (u >> 12) as u8);
            push!(0x80 | ((u >> 6) & 0x3F) as u8);
            push!(0x80 | (u & 0x3F) as u8);
        } else {
            // surrogate pair
            if u >= 0xDC00 || i >= n_units {
                return None;
            }
            let low = u16::from_le_bytes([data[2 * i], data[2 * i + 1]]);
            i += 1;
            if !(0xDC00..0xE000).contains(&low) {
                return None;
            }
            let cp = 0x10000 + (((u as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
            push!(0xF0 | (cp >> 18) as u8);
            push!(0x80 | ((cp >> 12) & 0x3F) as u8);
            push!(0x80 | ((cp >> 6) & 0x3F) as u8);
            push!(0x80 | (cp & 0x3F) as u8);
        }
    }
    // SAFETY: `written` bytes were initialized above and they form valid UTF-8
    unsafe {
        result.set_len(written);
        Some(String::from_utf8_unchecked(result))
    }
}

#[cfg(test)]
mod decode_test {
    use super::decode_utf16_le;

    fn enc(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn roundtrip() {
        for s in ["", "a", "é", "東京都", "\u{1F600}x", "aあ\u{10000}", "ｱｲｳ"] {
            assert_eq!(decode_utf16_le(&enc(s)).as_deref(), Some(s));
        }
    }

    #[test]
    fn invalid_surrogates() {
        assert_eq!(decode_utf16_le(&[0x00, 0xD8]), None);
        assert_eq!(decode_utf16_le(&[0x00, 0xDC, 0x41, 0x00]), None);
        assert_eq!(decode_utf16_le(&[0x00, 0xD8, 0x41, 0x00]), None);
    }
}

pub fn skip_u16_string(input: &[u8]) -> SudachiNomResult<&[u8], String> {
    utf16_string_data(input).map(|(rest, _)| (rest, String::new()))
}

#[inline]
pub fn utf16_string_data(input: &[u8]) -> SudachiNomResult<&[u8], &[u8]> {
    let (rest, length) = string_length_parser(input)?;
    if length == 0 {
        return Ok((rest, &[]));
    }
    let num_bytes = (length * 2) as usize;
    if rest.len() < num_bytes {
        return Err(nom::Err::Failure(SudachiNomError::Utf16String));
    }

    let (data, rest) = rest.split_at(num_bytes);

    Ok((rest, data))
}

pub fn string_length_parser(input: &[u8]) -> SudachiNomResult<&[u8], u16> {
    let (rest, length) = le_u8(input)?;
    // word length can be 1 or 2 bytes
    let (rest, opt_low) = nom::combinator::cond(length >= 128, le_u8)(rest)?;
    Ok((
        rest,
        match opt_low {
            Some(low) => ((length as u16 & 0x7F) << 8) | low as u16,
            None => length as u16,
        },
    ))
}
