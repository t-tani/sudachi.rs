/*
 * Copyright (c) 2021-2024 Works Applications Co., Ltd.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::iter::FusedIterator;

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct TrieEntry {
    /// Value of Trie, this is not the pointer to WordId, but the offset in WordId table
    pub value: u32,
    /// Offset of word end
    pub end: usize,
}

impl TrieEntry {
    #[inline]
    pub fn new(value: u32, offset: usize) -> TrieEntry {
        TrieEntry { value, end: offset }
    }
}

/// Double array trie units, stored as little-endian u32 values
/// in a byte buffer which is not required to be 4-byte aligned.
///
/// The trie is the largest part of the dictionary (tens of MB) and its position
/// inside the dictionary file is generally not aligned, so reinterpreting it as
/// `&[u32]` would require making a full copy of it at load time.
/// Unaligned loads are cheap on every target we care about, so we read the
/// units directly from the mapped bytes instead.
#[derive(Clone, Copy)]
struct TrieUnits<'a> {
    bytes: &'a [u8],
    len: usize,
}

impl<'a> TrieUnits<'a> {
    #[inline(always)]
    fn get(&self, index: usize) -> u32 {
        debug_assert!(index < self.len);
        // UB if out of bounds
        // Should we panic in release builds here instead?
        // Safe version is not optimized away
        // SAFETY: bytes has at least len * 4 bytes, see Trie::new
        let raw: [u8; 4] = unsafe {
            self.bytes
                .as_ptr()
                .add(index * 4)
                .cast::<[u8; 4]>()
                .read_unaligned()
        };
        u32::from_le_bytes(raw)
    }
}

pub struct Trie<'a> {
    units: TrieUnits<'a>,
    /// Backing storage for the owned trie, `units` point into it
    _storage: Option<Vec<u32>>,
}

pub struct TrieEntryIter<'a> {
    trie: TrieUnits<'a>,
    node_pos: usize,
    data: &'a [u8],
    offset: usize,
}

impl<'a> TrieEntryIter<'a> {
    #[inline(always)]
    fn get(&self, index: usize) -> u32 {
        self.trie.get(index)
    }
}

impl<'a> Iterator for TrieEntryIter<'a> {
    type Item = TrieEntry;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let mut node_pos = self.node_pos;
        let mut unit;

        for i in self.offset..self.data.len() {
            // Unwrap is safe: access is always in bounds
            // It is optimized away: https://rust.godbolt.org/z/va9K3az4n
            let k = self.data.get(i).unwrap();
            node_pos ^= *k as usize;
            unit = self.get(node_pos) as usize;
            if Trie::label(unit) != *k as usize {
                return None;
            }

            node_pos ^= Trie::offset(unit);
            if Trie::has_leaf(unit) {
                let r = TrieEntry::new(Trie::value(self.get(node_pos)), i + 1);
                self.offset = r.end;
                self.node_pos = node_pos;
                return Some(r);
            }
        }
        None
    }
}

impl FusedIterator for TrieEntryIter<'_> {}

impl<'a> Trie<'a> {
    /// Creates a trie over the first `size` units (4 bytes each) of `data`.
    ///
    /// Panics if `data` is too short.
    pub fn new(data: &'a [u8], size: usize) -> Trie<'a> {
        let bytes = &data[..size * 4];
        Trie {
            units: TrieUnits { bytes, len: size },
            _storage: None,
        }
    }

    pub fn new_owned(data: Vec<u32>) -> Trie<'a> {
        // trie units are stored as little-endian in the dictionary
        let data: Vec<u32> = data.into_iter().map(u32::to_le).collect();
        let len = data.len();
        // SAFETY: Vec<u32> memory is valid for len * 4 bytes.
        // The slice points to the vector contents, which are moved into the trie
        // and are never modified or reallocated after this point,
        // so the 'a lifetime is sound in practice (same pattern as CowArray).
        let bytes: &'a [u8] =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, len * 4) };
        Trie {
            units: TrieUnits { bytes, len },
            _storage: Some(data),
        }
    }

    pub fn total_size(&self) -> usize {
        4 * self.units.len
    }

    #[inline]
    pub fn common_prefix_iterator<'b>(&'a self, input: &'b [u8], offset: usize) -> TrieEntryIter<'b>
    where
        'a: 'b,
    {
        let unit: usize = self.get(0) as usize;

        TrieEntryIter {
            node_pos: Trie::offset(unit),
            data: input,
            trie: self.units,
            offset,
        }
    }

    #[inline(always)]
    fn get(&self, index: usize) -> u32 {
        self.units.get(index)
    }

    #[inline(always)]
    fn has_leaf(unit: usize) -> bool {
        ((unit >> 8) & 1) == 1
    }

    #[inline(always)]
    fn value(unit: u32) -> u32 {
        unit & ((1 << 31) - 1)
    }

    #[inline(always)]
    fn label(unit: usize) -> usize {
        unit & ((1 << 31) | 0xFF)
    }

    #[inline(always)]
    fn offset(unit: usize) -> usize {
        (unit >> 10) << ((unit & (1 << 9)) >> 6)
    }
}
